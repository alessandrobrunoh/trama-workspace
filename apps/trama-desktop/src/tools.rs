//! Tool call views in the transcript.
//!
//! File edits (Edit, MultiEdit, Write, NotebookEdit) render as inline
//! diffs built from the tool input, Bash calls as command blocks with
//! their output's tail, Trama subagent calls as links to the subthread,
//! and runs of the remaining calls plus thinking fold into one summary row
//! ("Thought 2 times, read 3 files"). The pure helpers come first and are
//! tested; the renderers below them only lay out what they compute.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, spinner::Spinner, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;
use trama_domain::ids::ThreadId;

use crate::chat::{ChatEntry, ChatPanel};
use crate::diff::{DiffLine, Hunk, LineKind};
use crate::theme::{self, Palette};

// -- classification -------------------------------------------------------

/// How a tool call is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Edit,
    Command,
    /// A Trama subthread (spawned or messaged): a link row.
    Subthread,
    Minor(Activity),
}

/// What a folded call or thought did, for the summary row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Activity {
    Thought,
    Read,
    Search,
    Web,
    Skill,
    Subagent,
    Todo,
    Other,
}

/// The tool's own name, without an `mcp__server__` prefix.
fn base_name(name: &str) -> &str {
    match name.strip_prefix("mcp__") {
        Some(rest) => rest.rsplit("__").next().unwrap_or(rest),
        None => name,
    }
}

pub fn tool_kind(name: &str) -> ToolKind {
    if name.starts_with("mcp__") && matches!(base_name(name), "spawn_subagent" | "send_to_thread") {
        return ToolKind::Subthread;
    }
    match name {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ToolKind::Edit,
        "Bash" => ToolKind::Command,
        "Read" | "NotebookRead" => ToolKind::Minor(Activity::Read),
        "Grep" | "Glob" | "LS" | "ToolSearch" => ToolKind::Minor(Activity::Search),
        "WebFetch" | "WebSearch" => ToolKind::Minor(Activity::Web),
        "Skill" => ToolKind::Minor(Activity::Skill),
        "Task" | "Agent" => ToolKind::Minor(Activity::Subagent),
        "TodoWrite" => ToolKind::Minor(Activity::Todo),
        _ => ToolKind::Minor(Activity::Other),
    }
}

fn activity(entry: &ChatEntry) -> Option<Activity> {
    match entry {
        ChatEntry::Thinking { .. } => Some(Activity::Thought),
        ChatEntry::ToolCall { name, .. } => match tool_kind(name) {
            ToolKind::Minor(activity) => Some(activity),
            _ => None,
        },
        _ => None,
    }
}

fn is_edit(entry: &ChatEntry) -> bool {
    matches!(entry, ChatEntry::ToolCall { name, .. } if tool_kind(name) == ToolKind::Edit)
}

/// One transcript row's worth of entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Single(usize),
    /// Thinking and minor tool calls folded into a summary row.
    Group(Range<usize>),
    /// Consecutive file edits under one "Applied N edits" header.
    Edits(Range<usize>),
}

impl Item {
    pub fn start(&self) -> usize {
        match self {
            Item::Single(index) => *index,
            Item::Group(range) | Item::Edits(range) => range.start,
        }
    }
}

/// Fold entries into rows. A lone minor tool call keeps its own row.
pub fn layout_items(entries: &[ChatEntry]) -> Vec<Item> {
    let mut items = Vec::new();
    let mut index = 0;
    while index < entries.len() {
        let start = index;
        if is_edit(&entries[index]) {
            while index < entries.len() && is_edit(&entries[index]) {
                index += 1;
            }
            items.push(Item::Edits(start..index));
        } else if activity(&entries[index]).is_some() {
            while index < entries.len() && activity(&entries[index]).is_some() {
                index += 1;
            }
            let single_call =
                index - start == 1 && matches!(entries[start], ChatEntry::ToolCall { .. });
            items.push(if single_call {
                Item::Single(start)
            } else {
                Item::Group(start..index)
            });
        } else {
            items.push(Item::Single(index));
            index += 1;
        }
    }
    items
}

/// "Thought 2 times, loaded a skill, ran 3 searches, read 6 files", in the
/// order each kind first appears.
pub fn group_summary(entries: &[ChatEntry]) -> String {
    let mut order: Vec<Activity> = Vec::new();
    let mut counts: HashMap<Activity, usize> = HashMap::new();
    for activity in entries.iter().filter_map(activity) {
        if !order.contains(&activity) {
            order.push(activity);
        }
        *counts.entry(activity).or_default() += 1;
    }
    let phrase = |activity: Activity, n: usize| -> String {
        let (one, many): (&str, String) = match activity {
            Activity::Thought => ("thought once", format!("thought {n} times")),
            Activity::Read => ("read a file", format!("read {n} files")),
            Activity::Search => ("ran a search", format!("ran {n} searches")),
            Activity::Web => ("browsed the web", format!("browsed the web {n} times")),
            Activity::Skill => ("loaded a skill", format!("loaded {n} skills")),
            Activity::Subagent => ("ran a subagent", format!("ran {n} subagents")),
            Activity::Todo => ("updated the plan", format!("updated the plan {n} times")),
            Activity::Other => ("called a tool", format!("called {n} tools")),
        };
        if n == 1 { one.to_string() } else { many }
    };
    let text = order
        .iter()
        .map(|activity| phrase(*activity, counts[activity]))
        .collect::<Vec<_>>()
        .join(", ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

// -- tool output ----------------------------------------------------------

/// Text of a tool result: a string, the text blocks of a content array, or
/// the common stdout/stderr fields; other JSON is pretty-printed.
pub fn output_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block {
                Value::String(text) => text.clone(),
                other => other
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| output_text(other)),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => {
            let fields: Vec<&str> = ["stdout", "stderr", "output", "content", "text"]
                .iter()
                .filter_map(|key| map.get(*key).and_then(Value::as_str))
                .filter(|text| !text.is_empty())
                .collect();
            if fields.is_empty() {
                serde_json::to_string_pretty(value).unwrap_or_default()
            } else {
                fields.join("\n")
            }
        }
        other => other.to_string(),
    }
}

/// When the call started and finished, in milliseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolTiming {
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
}

impl ToolTiming {
    pub fn duration_ms(&self) -> Option<u64> {
        self.finished_ms
            .map(|end| end.saturating_sub(self.started_ms).max(0) as u64)
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// `420ms`, `2.3s`, `1m 04s`.
pub fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.)
    } else {
        let secs = ms / 1_000;
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

// -- edits → unified diff -------------------------------------------------

/// One edit call's change to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    /// "Edit", "Write" or "Edit notebook".
    pub verb: &'static str,
    pub path: String,
    /// Replacements in this call (MultiEdit carries several).
    pub edits: usize,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
}

impl FileEdit {
    pub fn line_count(&self) -> usize {
        self.hunks.iter().map(|hunk| hunk.lines.len()).sum()
    }
}

const CONTEXT: usize = 3;

/// The diff an edit call makes, from its input. `output` (the tool result)
/// places a single Edit at its real line when it quotes the edited lines.
pub fn file_edit(name: &str, input: &Value, output: Option<&Value>) -> Option<FileEdit> {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let (verb, path, pairs): (&'static str, String, Vec<(String, String)>) = match name {
        "Edit" => (
            "Edit",
            field("file_path")?.to_string(),
            vec![(
                field("old_string").unwrap_or_default().to_string(),
                field("new_string").unwrap_or_default().to_string(),
            )],
        ),
        "MultiEdit" => {
            let edits = input.get("edits").and_then(Value::as_array);
            let pairs = edits
                .map(|edits| {
                    edits
                        .iter()
                        .map(|edit| {
                            let text = |key: &str| {
                                edit.get(key)
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string()
                            };
                            (text("old_string"), text("new_string"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            ("Edit", field("file_path")?.to_string(), pairs)
        }
        "Write" => (
            "Write",
            field("file_path")?.to_string(),
            vec![(
                String::new(),
                field("content").unwrap_or_default().to_string(),
            )],
        ),
        "NotebookEdit" => {
            let source = field("new_source").unwrap_or_default().to_string();
            let pair = if field("edit_mode") == Some("delete") {
                (source, String::new())
            } else {
                (String::new(), source)
            };
            (
                "Edit notebook",
                field("notebook_path")?.to_string(),
                vec![pair],
            )
        }
        _ => return None,
    };
    let snippet = output.map(output_text).unwrap_or_default();
    let single = pairs.len() == 1;
    let mut hunks = Vec::new();
    for (old, new) in &pairs {
        let start = if name == "Write" {
            1
        } else if single {
            locate_in_snippet(&snippet, new).unwrap_or(1)
        } else {
            1
        };
        let lines = line_diff(old, new, start, start);
        hunks.extend(build_hunks(&lines, CONTEXT));
    }
    let (additions, deletions) =
        hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .fold((0, 0), |(add, del), line| match line.kind {
                LineKind::Added => (add + 1, del),
                LineKind::Removed => (add, del + 1),
                _ => (add, del),
            });
    Some(FileEdit {
        verb,
        path,
        edits: pairs.len(),
        hunks,
        additions,
        deletions,
    })
}

/// The line where `new` starts, read from a `cat -n` style snippet in the
/// tool result ("   12→text" or "   12\ttext").
fn locate_in_snippet(snippet: &str, new: &str) -> Option<u32> {
    let first = new.lines().find(|line| !line.trim().is_empty())?;
    let offset = new.lines().position(|line| line == first)? as u32;
    snippet.lines().find_map(|line| {
        let trimmed = line.trim_start();
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let rest = &trimmed[digits..];
        let text = rest.strip_prefix('→').or_else(|| rest.strip_prefix('\t'))?;
        (text.trim_end() == first.trim_end())
            .then(|| trimmed[..digits].parse::<u32>().ok())
            .flatten()
            .map(|number| number.saturating_sub(offset).max(1))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal,
    Delete,
    Insert,
}

/// Line operations turning `old` into `new` (longest common subsequence
/// after trimming the shared head and tail).
fn diff_ops<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(Op, &'a str)> {
    let head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &old[head..old.len() - tail];
    let b = &new[head..new.len() - tail];
    let mut ops: Vec<(Op, &str)> = old[..head].iter().map(|line| (Op::Equal, *line)).collect();
    if a.len().saturating_mul(b.len()) > 1_000_000 {
        ops.extend(a.iter().map(|line| (Op::Delete, *line)));
        ops.extend(b.iter().map(|line| (Op::Insert, *line)));
    } else {
        // lcs[i][j]: common lines of a[i..] and b[j..].
        let width = b.len() + 1;
        let mut lcs = vec![0u32; (a.len() + 1) * width];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i * width + j] = if a[i] == b[j] {
                    lcs[(i + 1) * width + j + 1] + 1
                } else {
                    lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            if i < a.len() && j < b.len() && a[i] == b[j] {
                ops.push((Op::Equal, a[i]));
                i += 1;
                j += 1;
            } else if j < b.len()
                && (i == a.len() || lcs[i * width + j + 1] >= lcs[(i + 1) * width + j])
            {
                ops.push((Op::Insert, b[j]));
                j += 1;
            } else {
                ops.push((Op::Delete, a[i]));
                i += 1;
            }
        }
        // Show removals before additions within each change.
        let mut start = 0;
        while start < ops.len() {
            if ops[start].0 == Op::Equal {
                start += 1;
                continue;
            }
            let end = ops[start..]
                .iter()
                .position(|(op, _)| *op == Op::Equal)
                .map_or(ops.len(), |offset| start + offset);
            ops[start..end].sort_by_key(|(op, _)| *op == Op::Insert);
            start = end;
        }
    }
    ops.extend(
        old[old.len() - tail..]
            .iter()
            .map(|line| (Op::Equal, *line)),
    );
    ops
}

/// Numbered diff lines for `old` → `new`, both starting at the given line.
pub fn line_diff(old: &str, new: &str, old_start: u32, new_start: u32) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let (mut old_no, mut new_no) = (old_start, new_start);
    diff_ops(&old_lines, &new_lines)
        .into_iter()
        .map(|(op, text)| {
            let (kind, old_line, new_line) = match op {
                Op::Equal => {
                    let numbers = (LineKind::Context, Some(old_no), Some(new_no));
                    old_no += 1;
                    new_no += 1;
                    numbers
                }
                Op::Delete => {
                    old_no += 1;
                    (LineKind::Removed, Some(old_no - 1), None)
                }
                Op::Insert => {
                    new_no += 1;
                    (LineKind::Added, None, Some(new_no - 1))
                }
            };
            DiffLine {
                kind,
                old_line,
                new_line,
                text: text.to_string(),
            }
        })
        .collect()
}

/// Group changed lines into hunks with `context` lines around them.
pub fn build_hunks(lines: &[DiffLine], context: usize) -> Vec<Hunk> {
    let changed: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.kind != LineKind::Context)
        .map(|(index, _)| index)
        .collect();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for &index in &changed {
        let start = index.saturating_sub(context);
        let end = (index + context + 1).min(lines.len());
        match ranges.last_mut() {
            Some(last) if start <= last.end => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    ranges
        .into_iter()
        .map(|range| {
            let slice = &lines[range];
            let old_len = slice.iter().filter(|l| l.old_line.is_some()).count() as u32;
            let new_len = slice.iter().filter(|l| l.new_line.is_some()).count() as u32;
            let old_start = slice
                .iter()
                .find_map(|line| line.old_line)
                .unwrap_or_else(|| slice[0].new_line.unwrap_or(1).saturating_sub(1));
            let new_start = slice
                .iter()
                .find_map(|line| line.new_line)
                .unwrap_or_else(|| slice[0].old_line.unwrap_or(1).saturating_sub(1));
            Hunk {
                header: format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@"),
                old_start,
                old_len,
                new_start,
                new_len,
                section: String::new(),
                lines: slice.to_vec(),
            }
        })
        .collect()
}

/// "Applied 3 edits across 2 files".
pub fn edits_header<'a>(edits: impl IntoIterator<Item = &'a FileEdit>) -> String {
    let mut count = 0;
    let mut files: Vec<&str> = Vec::new();
    for edit in edits {
        count += edit.edits.max(1);
        files.push(edit.path.as_str());
    }
    files.sort_unstable();
    files.dedup();
    format!(
        "Applied {} across {}",
        crate::comments::plural(count, "edit", "edits"),
        crate::comments::plural(files.len(), "file", "files")
    )
}

/// `/Users/me/src/app/main.rs` → `~/src/app/main.rs`.
pub fn display_path(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => match path.strip_prefix(&home) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        _ => path.to_string(),
    }
}

// -- commands -------------------------------------------------------------

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    Running,
    Success,
    Code(i32),
    Failed,
    Interrupted,
}

/// Status and remaining text of a Bash result. Claude Code reports a
/// non-zero exit as an error whose text starts with "Exit code N".
pub fn command_result(finished: bool, is_error: bool, text: &str) -> (ExitStatus, String) {
    if !finished {
        return (ExitStatus::Running, text.to_string());
    }
    let trimmed = text.trim_start();
    if trimmed.starts_with("[Request interrupted") {
        return (ExitStatus::Interrupted, String::new());
    }
    let (first, rest) = trimmed.split_once('\n').unwrap_or((trimmed, ""));
    let code = first
        .trim()
        .trim_start_matches("Error: ")
        .strip_prefix("Exit code ")
        .and_then(|code| code.trim().parse::<i32>().ok());
    match code {
        Some(0) => (ExitStatus::Success, rest.to_string()),
        Some(code) => (ExitStatus::Code(code), rest.to_string()),
        None if is_error => (ExitStatus::Failed, text.to_string()),
        None => (ExitStatus::Success, text.to_string()),
    }
}

/// A terminal color from an SGR sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnsiColor {
    /// 0–7 normal, 8–15 bright.
    Basic(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnsiStyle {
    pub fg: Option<AnsiColor>,
    pub bold: bool,
    pub dim: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsiSpan {
    pub text: String,
    pub style: AnsiStyle,
}

pub type AnsiLine = Vec<AnsiSpan>;

/// Split terminal output into styled lines. SGR colors and weights are
/// kept; other escapes (cursor moves, erase, OSC titles and links) are
/// dropped; a carriage return keeps the text written after it.
pub fn parse_ansi(text: &str) -> Vec<AnsiLine> {
    let mut style = AnsiStyle::default();
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let raw = raw.rsplit('\r').next().unwrap_or(raw);
        let mut line: AnsiLine = Vec::new();
        let mut current = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' => match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let mut fin = None;
                        for next in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&next) {
                                fin = Some(next);
                                break;
                            }
                            params.push(next);
                        }
                        if fin == Some('m') {
                            let next_style = apply_sgr(style, &params);
                            if next_style != style && !current.is_empty() {
                                line.push(AnsiSpan {
                                    text: std::mem::take(&mut current),
                                    style,
                                });
                            }
                            style = next_style;
                        }
                    }
                    Some(']') => {
                        // OSC: up to BEL or ESC \.
                        while let Some(next) = chars.next() {
                            if next == '\u{7}' {
                                break;
                            }
                            if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                                chars.next();
                                break;
                            }
                        }
                    }
                    _ => {}
                },
                '\t' => current.push_str("    "),
                c if c.is_control() => {}
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            line.push(AnsiSpan {
                text: current,
                style,
            });
        }
        lines.push(line);
    }
    while lines
        .last()
        .is_some_and(|line| line_text(line).trim().is_empty())
    {
        lines.pop();
    }
    lines
}

pub fn line_text(line: &AnsiLine) -> String {
    line.iter().map(|span| span.text.as_str()).collect()
}

fn apply_sgr(mut style: AnsiStyle, params: &str) -> AnsiStyle {
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            .map(|code| code.parse().unwrap_or(0))
            .collect()
    };
    let mut index = 0;
    while index < codes.len() {
        match codes[index] {
            0 => style = AnsiStyle::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            code @ 30..=37 => style.fg = Some(AnsiColor::Basic((code - 30) as u8)),
            code @ 90..=97 => style.fg = Some(AnsiColor::Basic((code - 90 + 8) as u8)),
            39 => style.fg = None,
            kind @ (38 | 48) => {
                let color = match codes.get(index + 1).copied() {
                    Some(5) => {
                        let n = codes.get(index + 2).copied().unwrap_or(0).min(255) as u8;
                        index += 2;
                        Some(color_256(n))
                    }
                    Some(2) => {
                        let channel =
                            |k: usize| codes.get(index + k).copied().unwrap_or(0).min(255) as u8;
                        let rgb = AnsiColor::Rgb(channel(2), channel(3), channel(4));
                        index += 4;
                        Some(rgb)
                    }
                    _ => None,
                };
                if kind == 38 {
                    style.fg = color.or(style.fg);
                }
            }
            _ => {}
        }
        index += 1;
    }
    style
}

fn color_256(n: u8) -> AnsiColor {
    match n {
        0..=15 => AnsiColor::Basic(n),
        16..=231 => {
            let n = n - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            AnsiColor::Rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let gray = 8 + (n - 232) * 10;
            AnsiColor::Rgb(gray, gray, gray)
        }
    }
}

/// Lines shown by default: the last `limit`. Returns the first visible line
/// and how many are hidden above it.
pub fn visible_tail(total: usize, limit: usize, expanded: bool) -> (usize, usize) {
    if expanded || total <= limit {
        (0, 0)
    } else {
        (total - limit, total - limit)
    }
}

pub const OUTPUT_TAIL: usize = 12;

// -- subthreads -----------------------------------------------------------

/// A Trama subthread call: its title and, when known, the thread it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubthreadLink {
    pub title: String,
    pub thread: Option<ThreadId>,
    /// `send_to_thread`: a message to an existing subthread.
    pub message: bool,
}

/// Find the subthread from the call's input and result, against the
/// threads the sidebar knows (id first, then title).
pub fn subthread_link(
    name: &str,
    input: &Value,
    output: Option<&Value>,
    threads: &[(ThreadId, String)],
) -> SubthreadLink {
    let output_text = output.map(output_text).unwrap_or_default();
    let output_json: Option<Value> = output
        .filter(|value| value.is_object())
        .cloned()
        .or_else(|| serde_json::from_str(output_text.trim()).ok());
    let text_field = |value: &Value, keys: &[&str]| {
        keys.iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .map(str::to_string)
    };
    const ID_KEYS: &[&str] = &["thread_id", "threadId", "target_thread_id", "subthread_id"];
    const TITLE_KEYS: &[&str] = &["title", "name", "description"];
    let id = text_field(input, ID_KEYS)
        .or_else(|| {
            output_json
                .as_ref()
                .and_then(|json| text_field(json, ID_KEYS))
        })
        .or_else(|| {
            threads
                .iter()
                .find(|(id, _)| !id.0.is_empty() && output_text.contains(&id.0))
                .map(|(id, _)| id.0.clone())
        });
    let known = id
        .as_ref()
        .and_then(|id| threads.iter().find(|(known, _)| &known.0 == id));
    let title = text_field(input, TITLE_KEYS)
        .or_else(|| known.map(|(_, title)| title.clone()))
        .or_else(|| {
            output_json
                .as_ref()
                .and_then(|json| text_field(json, TITLE_KEYS))
        })
        .unwrap_or_else(|| "Untitled".into());
    let thread = known.map(|(id, _)| id.clone()).or_else(|| {
        threads
            .iter()
            .find(|(_, known)| known.trim().eq_ignore_ascii_case(title.trim()))
            .map(|(id, _)| id.clone())
    });
    SubthreadLink {
        title: crate::comments::one_line(&title, 80),
        thread,
        message: base_name(name) == "send_to_thread",
    }
}

// -- rendering ------------------------------------------------------------

/// Expansion state the tool views read, owned by the chat panel.
#[derive(Default)]
pub struct ToolViewState {
    /// Summary rows opened, by their first entry index.
    pub open_groups: HashSet<usize>,
    /// Edit rows opened or closed by hand, by tool use id.
    pub edit_open: HashMap<String, bool>,
    /// Commands whose whole output is shown, by tool use id.
    pub full_output: HashSet<String>,
    pub timings: HashMap<String, ToolTiming>,
    /// Diffs built so far, by tool use id, with whether the call had
    /// finished (its result can move the diff to the real line numbers).
    edits: EditCache,
}

/// Built diffs by tool use id, with the `finished` flag they were built at.
type EditCache = RefCell<HashMap<String, (bool, Option<Rc<FileEdit>>)>>;

impl ToolViewState {
    pub fn clear(&mut self) {
        self.open_groups.clear();
        self.edit_open.clear();
        self.full_output.clear();
        self.timings.clear();
        self.edits.borrow_mut().clear();
    }

    /// The call's diff, built once per state of the call.
    fn file_edit(&self, entry: &ChatEntry) -> Option<Rc<FileEdit>> {
        let ChatEntry::ToolCall {
            tool_use_id,
            name,
            input_json,
            finished,
            output,
            ..
        } = entry
        else {
            return None;
        };
        let mut cache = self.edits.borrow_mut();
        if let Some((seen, edit)) = cache.get(tool_use_id)
            && seen == finished
        {
            return edit.clone();
        }
        let edit = file_edit(name, input_json, output.as_ref()).map(Rc::new);
        cache.insert(tool_use_id.clone(), (*finished, edit.clone()));
        edit
    }
}

/// Diffs up to this many lines start open.
const OPEN_EDIT_LINES: usize = 24;
/// Lines drawn for one expanded diff or output.
const MAX_RENDERED_LINES: usize = 600;

fn status_icon(finished: bool, is_error: bool, p: &Palette) -> AnyElement {
    if is_error {
        Icon::new(IconName::CircleX)
            .with_size(px(13.))
            .text_color(p.danger)
            .into_any_element()
    } else if finished {
        Icon::new(IconName::Check)
            .with_size(px(13.))
            .text_color(p.success)
            .into_any_element()
    } else {
        Spinner::new().xsmall().color(p.accent).into_any_element()
    }
}

fn chevron(open: bool, p: &Palette) -> Icon {
    Icon::new(if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .with_size(px(12.))
    .text_color(p.text_faint)
}

/// A path as a faint directory and a stronger file name.
fn path_label(path: &str, p: &Palette) -> Div {
    let path = display_path(path);
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name.to_string()),
        None => (String::new(), path),
    };
    h_flex()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .font_family(theme::MONO_FONT)
        .text_size(theme::text_xs())
        .child(
            div()
                .flex_shrink(1.)
                .min_w_0()
                .truncate()
                .text_color(p.text_faint)
                .child(dir),
        )
        .child(div().flex_shrink_0().text_color(p.text).child(name))
}

fn counts(additions: u32, deletions: u32, p: &Palette) -> Div {
    h_flex()
        .flex_shrink_0()
        .gap(theme::sp(1.5))
        .font_family(theme::MONO_FONT)
        .text_size(theme::text_xs())
        .when(additions > 0, |this| {
            this.child(
                div()
                    .text_color(p.diff_add_text)
                    .child(format!("+{additions}")),
            )
        })
        .when(deletions > 0, |this| {
            this.child(
                div()
                    .text_color(p.diff_del_text)
                    .child(format!("\u{2212}{deletions}")),
            )
        })
}

/// "Thought 2 times, read 3 files", opening to the individual rows.
pub fn render_group(
    range: Range<usize>,
    entries: &[ChatEntry],
    open: bool,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let slice = &entries[range.clone()];
    let running = slice.iter().any(|entry| {
        matches!(
            entry,
            ChatEntry::ToolCall {
                finished: false,
                ..
            }
        )
    });
    let failed = slice
        .iter()
        .filter(|entry| matches!(entry, ChatEntry::ToolCall { is_error: true, .. }))
        .count();
    let thinking_only = slice
        .iter()
        .all(|entry| matches!(entry, ChatEntry::Thinking { .. }));
    let start = range.start;
    let header = h_flex()
        .id(("tool-group", start))
        .w_full()
        .h(px(26.))
        .px(theme::sp(2.))
        .gap(theme::sp(2.))
        .items_center()
        .rounded(theme::radius())
        .cursor_pointer()
        .text_size(theme::text_sm())
        .hover(|style| style.bg(p.hover))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(start, cx)))
        .child(
            Icon::new(if thinking_only {
                IconName::Brain
            } else {
                IconName::Sparkles
            })
            .with_size(px(13.))
            .text_color(p.text_muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(p.text_muted)
                .child(group_summary(slice)),
        )
        .when(failed > 0, |this| {
            this.child(
                div()
                    .flex_shrink_0()
                    .text_size(theme::text_xs())
                    .text_color(p.danger)
                    .child(format!("{failed} failed")),
            )
        })
        .when(running, |this| {
            this.child(Spinner::new().xsmall().color(p.accent))
        })
        .child(chevron(open, p));
    let body = open.then(|| {
        let mut list = v_flex()
            .w_full()
            .pl(theme::sp(4.))
            .gap(theme::sp(0.5))
            .border_l_1()
            .border_color(p.border)
            .ml(theme::sp(3.5));
        for (offset, entry) in slice.iter().enumerate() {
            let ix = start + offset;
            list = list.child(match entry {
                ChatEntry::Thinking { text } => render_thought(text, p),
                _ => crate::chat::tool_row(ix, entry, p, cx),
            });
        }
        list
    });
    div()
        .w_full()
        .pl(theme::sp(7.))
        .child(
            v_flex()
                .w_full()
                .gap(theme::sp(1.))
                .child(header)
                .children(body),
        )
        .into_any_element()
}

fn render_thought(text: &str, p: &Palette) -> AnyElement {
    v_flex()
        .w_full()
        .px(theme::sp(2.))
        .py(theme::sp(1.))
        .gap(theme::sp(1.))
        .child(
            h_flex()
                .gap(theme::sp(2.))
                .items_center()
                .text_size(theme::text_sm())
                .text_color(p.text_muted)
                .child(
                    Icon::new(IconName::Brain)
                        .with_size(px(13.))
                        .text_color(p.text_faint),
                )
                .child("Thought"),
        )
        .when(!text.trim().is_empty(), |this| {
            this.child(
                div()
                    .pl(theme::sp(5.))
                    .text_size(theme::text_sm())
                    .line_height(relative(1.5))
                    .italic()
                    .text_color(p.text_faint)
                    .child(text.trim().to_string()),
            )
        })
        .into_any_element()
}

/// "Applied N edits across M files" with one expandable diff per call.
pub fn render_edits(
    range: Range<usize>,
    entries: &[ChatEntry],
    state: &ToolViewState,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    struct Row<'a> {
        id: &'a str,
        edit: Rc<FileEdit>,
        finished: bool,
        is_error: bool,
        error: Option<String>,
    }
    let rows: Vec<Row> = entries[range.clone()]
        .iter()
        .filter_map(|entry| match entry {
            ChatEntry::ToolCall {
                tool_use_id,
                finished,
                is_error,
                output,
                ..
            } => {
                let edit = state.file_edit(entry)?;
                let error = is_error
                    .then(|| output.as_ref().map(output_text))
                    .flatten()
                    .map(|text| crate::comments::one_line(&strip_tags(&text), 160));
                Some(Row {
                    id: tool_use_id,
                    edit,
                    finished: *finished,
                    is_error: *is_error,
                    error,
                })
            }
            _ => None,
        })
        .collect();
    if rows.is_empty() {
        // Inputs this view cannot read: plain tool rows.
        let list = v_flex().w_full().children(
            range
                .clone()
                .map(|ix| crate::chat::tool_row(ix, &entries[ix], p, cx)),
        );
        return div()
            .w_full()
            .pl(theme::sp(7.))
            .child(list)
            .into_any_element();
    }
    let any_failed = rows.iter().any(|row| row.is_error);
    let running = rows.iter().any(|row| !row.finished);
    let header = h_flex()
        .h(px(24.))
        .px(theme::sp(2.))
        .gap(theme::sp(2.))
        .items_center()
        .text_size(theme::text_sm())
        .child(
            Icon::new(IconName::FilePen)
                .with_size(px(13.))
                .text_color(p.text_muted),
        )
        .child(
            div()
                .text_color(p.text_muted)
                .child(edits_header(rows.iter().map(|row| row.edit.as_ref()))),
        )
        .when(any_failed, |this| {
            this.child(div().text_color(p.danger).child("\u{2014} Failed"))
        })
        .when(running, |this| {
            this.child(Spinner::new().xsmall().color(p.accent))
        });

    let mut list = v_flex().w_full().gap(theme::sp(1.5));
    for (n, row) in rows.iter().enumerate() {
        let lines = row.edit.line_count();
        let open = state
            .edit_open
            .get(row.id)
            .copied()
            .unwrap_or(lines <= OPEN_EDIT_LINES && !row.is_error);
        let id = row.id.to_string();
        let ix = range.start + n;
        let title = h_flex()
            .id(("edit-row", ix))
            .w_full()
            .h(px(28.))
            .px(theme::sp(2.))
            .gap(theme::sp(2.))
            .items_center()
            .cursor_pointer()
            .hover(|style| style.bg(p.hover))
            .when(open && lines > 0, |this| {
                this.border_b_1().border_color(p.border)
            })
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_edit(&id, open, cx)))
            .child(chevron(open, p))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(theme::text_sm())
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.text)
                    .child(row.edit.verb),
            )
            .child(path_label(&row.edit.path, p))
            .child(counts(row.edit.additions, row.edit.deletions, p))
            .child(status_icon(row.finished, row.is_error, p));
        let body = (open && lines > 0).then(|| {
            let mut column = v_flex()
                .flex_shrink_0()
                .min_w_full()
                .py(theme::sp(1.))
                .font_family(theme::MONO_FONT)
                .text_size(theme::text_xs());
            let mut drawn = 0;
            for (hunk_ix, hunk) in row.edit.hunks.iter().enumerate() {
                if hunk_ix > 0 {
                    column = column.child(
                        div()
                            .w_full()
                            .px(theme::sp(2.))
                            .bg(p.diff_hunk_bg)
                            .text_color(p.text_faint)
                            .child("\u{22ef}"),
                    );
                }
                for line in &hunk.lines {
                    if drawn == MAX_RENDERED_LINES {
                        break;
                    }
                    column = column.child(crate::changes::render_line(line, false, p));
                    drawn += 1;
                }
            }
            if lines > drawn {
                column = column.child(
                    div()
                        .px(theme::sp(2.))
                        .py(theme::sp(1.))
                        .font_family(theme::UI_FONT)
                        .text_color(p.text_faint)
                        .child(format!("{} more lines not shown", lines - drawn)),
                );
            }
            h_flex()
                .id(("edit-body", ix))
                .w_full()
                .overflow_x_scroll()
                .child(column)
        });
        list = list.child(
            v_flex()
                .w_full()
                .rounded(theme::radius())
                .border_1()
                .border_color(if row.is_error {
                    p.danger.opacity(0.4)
                } else {
                    p.border
                })
                .bg(p.bg)
                .overflow_hidden()
                .child(title)
                .when_some(row.error.clone(), |this, error| {
                    this.child(
                        div()
                            .px(theme::sp(3.))
                            .py(theme::sp(1.5))
                            .text_size(theme::text_xs())
                            .text_color(p.danger)
                            .child(error),
                    )
                })
                .children(body),
        );
    }
    div()
        .w_full()
        .pl(theme::sp(7.))
        .child(
            v_flex()
                .w_full()
                .gap(theme::sp(1.5))
                .child(header)
                .child(list),
        )
        .into_any_element()
}

/// Drop `<tool_use_error>`-style wrappers from an error message.
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open..].find('>') else {
            break;
        };
        let tag = &rest[open + 1..open + close];
        out.push_str(&rest[..open]);
        if tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '/')
        {
            rest = &rest[open + close + 1..];
        } else {
            out.push('<');
            rest = &rest[open + 1..];
        }
    }
    out.push_str(rest);
    out
}

fn ansi_color(color: AnsiColor, p: &Palette) -> Hsla {
    match color {
        AnsiColor::Rgb(r, g, b) => rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into(),
        AnsiColor::Basic(n) => match n % 8 {
            0 => p.text_faint,
            1 => p.danger,
            2 => p.success,
            3 => p.warning,
            4 => p.accent_text,
            5 => hsla(300. / 360., 0.45, 0.65, 1.),
            6 => hsla(187. / 360., 0.45, 0.55, 1.),
            _ => p.text,
        },
    }
}

fn styled_line(line: &AnsiLine, p: &Palette) -> StyledText {
    let mut text = String::new();
    let mut highlights = Vec::new();
    for span in line {
        let start = text.len();
        text.push_str(&span.text);
        let style = span.style;
        if style.fg.is_some() || style.bold || style.dim {
            let color = style.fg.map(|color| ansi_color(color, p));
            highlights.push((
                start..text.len(),
                HighlightStyle {
                    color: color.or(style.dim.then_some(p.text_faint)),
                    font_weight: style.bold.then_some(FontWeight::BOLD),
                    ..Default::default()
                },
            ));
        }
    }
    StyledText::new(text).with_highlights(highlights)
}

/// A Bash call: description, command, duration, exit status and the tail
/// of its output.
pub fn render_command(
    ix: usize,
    entry: &ChatEntry,
    state: &ToolViewState,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let ChatEntry::ToolCall {
        tool_use_id,
        input_json,
        finished,
        is_error,
        output,
        ..
    } = entry
    else {
        return div().into_any_element();
    };
    let field = |key: &str| input_json.get(key).and_then(Value::as_str);
    let command = field("command").unwrap_or_default().trim().to_string();
    let description = field("description").map(str::to_string);
    let text = output.as_ref().map(output_text).unwrap_or_default();
    let (status, body) = command_result(*finished, *is_error, &text);
    let lines = parse_ansi(&body);
    let show_all = state.full_output.contains(tool_use_id);
    let (first, hidden) = visible_tail(lines.len(), OUTPUT_TAIL, show_all);
    let duration = state
        .timings
        .get(tool_use_id)
        .and_then(ToolTiming::duration_ms)
        .map(format_duration);

    let (status_label, status_color): (String, Hsla) = match status {
        ExitStatus::Running => ("Running".into(), p.accent_text),
        ExitStatus::Success => ("exit 0".into(), p.success),
        ExitStatus::Code(code) => (format!("exit {code}"), p.danger),
        ExitStatus::Failed => ("failed".into(), p.danger),
        ExitStatus::Interrupted => ("interrupted".into(), p.warning),
    };
    let header = h_flex()
        .w_full()
        .px(theme::sp(2.5))
        .py(theme::sp(1.5))
        .gap(theme::sp(2.))
        .items_center()
        .text_size(theme::text_sm())
        .child(
            Icon::new(IconName::SquareTerminal)
                .with_size(px(13.))
                .text_color(p.text_muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.text)
                .child(description.unwrap_or_else(|| "Ran a command".into())),
        )
        .when_some(duration, |this, duration| {
            this.child(
                div()
                    .flex_shrink_0()
                    .font_family(theme::MONO_FONT)
                    .text_size(theme::text_xs())
                    .text_color(p.text_faint)
                    .child(duration),
            )
        })
        .child(
            h_flex()
                .flex_shrink_0()
                .h(px(18.))
                .px(theme::sp(1.5))
                .gap(theme::sp(1.))
                .items_center()
                .rounded(theme::radius_sm())
                .bg(status_color.opacity(0.12))
                .font_family(theme::MONO_FONT)
                .text_size(theme::text_xs())
                .text_color(status_color)
                .when(status == ExitStatus::Running, |this| {
                    this.child(Spinner::new().xsmall().color(p.accent))
                })
                .child(status_label),
        );

    let command_lines: Vec<&str> = command.lines().collect();
    let shown_command = command_lines.iter().take(6).copied().collect::<Vec<_>>();
    let mut command_block = v_flex()
        .w_full()
        .px(theme::sp(2.5))
        .py(theme::sp(1.5))
        .border_t_1()
        .border_color(p.border)
        .bg(p.bg)
        .font_family(theme::MONO_FONT)
        .text_size(theme::text_xs())
        .line_height(relative(1.5))
        .text_color(p.text);
    for (n, line) in shown_command.iter().enumerate() {
        command_block = command_block.child(
            h_flex()
                .w_full()
                .child(
                    div()
                        .w(px(14.))
                        .flex_shrink_0()
                        .text_color(p.text_faint)
                        .child(if n == 0 { "$" } else { " " }),
                )
                .child(div().flex_1().min_w_0().child(line.to_string())),
        );
    }
    if command_lines.len() > shown_command.len() {
        command_block = command_block.child(div().pl(px(14.)).text_color(p.text_faint).child(
            format!("… {} more lines", command_lines.len() - shown_command.len()),
        ));
    }

    let output_block = (!lines.is_empty() || (*finished && status != ExitStatus::Interrupted))
        .then(|| {
            let id = tool_use_id.clone();
            let toggle = (hidden > 0 || show_all && lines.len() > OUTPUT_TAIL).then(|| {
                h_flex()
                    .id(("command-more", ix))
                    .w_full()
                    .px(theme::sp(2.5))
                    .py(theme::sp(1.))
                    .gap(theme::sp(1.5))
                    .items_center()
                    .cursor_pointer()
                    .font_family(theme::UI_FONT)
                    .text_size(theme::text_xs())
                    .text_color(p.accent_text)
                    .hover(|style| style.bg(p.hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_output(&id, cx)))
                    .child(
                        Icon::new(if show_all {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronsUpDown
                        })
                        .with_size(px(11.)),
                    )
                    .child(if show_all {
                        "Show less".to_string()
                    } else {
                        format!(
                            "Show {}",
                            crate::comments::plural(hidden, "more line", "more lines")
                        )
                    })
            });
            let mut column = v_flex()
                .flex_shrink_0()
                .min_w_full()
                .px(theme::sp(2.5))
                .py(theme::sp(1.5))
                .font_family(theme::MONO_FONT)
                .text_size(theme::text_xs())
                .line_height(relative(1.5))
                .text_color(p.text_muted);
            if lines.is_empty() {
                column = column.child(div().text_color(p.text_faint).child("(no output)"));
            }
            let end = lines.len().min(first + MAX_RENDERED_LINES);
            for line in &lines[first..end] {
                column = column.child(div().whitespace_nowrap().child(styled_line(line, p)));
            }
            if end < lines.len() {
                column = column.child(
                    div()
                        .text_color(p.text_faint)
                        .child(format!("… {} more lines not shown", lines.len() - end)),
                );
            }
            v_flex()
                .w_full()
                .border_t_1()
                .border_color(p.border)
                .bg(p.bg)
                .children(toggle)
                .child(
                    h_flex()
                        .id(("command-output", ix))
                        .w_full()
                        .overflow_x_scroll()
                        .child(column),
                )
        });

    div()
        .w_full()
        .pl(theme::sp(7.))
        .child(
            v_flex()
                .w_full()
                .rounded(theme::radius())
                .border_1()
                .border_color(match status {
                    ExitStatus::Code(_) | ExitStatus::Failed => p.danger.opacity(0.4),
                    _ => p.border,
                })
                .bg(p.elevated)
                .overflow_hidden()
                .child(header)
                .child(command_block)
                .children(output_block),
        )
        .into_any_element()
}

/// "Subagent: <title>", selecting the subthread when the sidebar knows it.
pub fn render_subthread(
    ix: usize,
    entry: &ChatEntry,
    link: SubthreadLink,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let ChatEntry::ToolCall {
        finished, is_error, ..
    } = entry
    else {
        return div().into_any_element();
    };
    let linked = link.thread.clone();
    div()
        .w_full()
        .pl(theme::sp(7.))
        .child(
            h_flex()
                .id(("subthread-row", ix))
                .w_full()
                .h(px(28.))
                .px(theme::sp(2.))
                .gap(theme::sp(2.))
                .items_center()
                .rounded(theme::radius())
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .text_size(theme::text_sm())
                .when_some(linked, |this, thread| {
                    this.cursor_pointer()
                        .hover(|style| style.bg(p.hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_thread(thread.clone(), cx);
                        }))
                })
                .child(
                    Icon::new(IconName::Bot)
                        .with_size(px(13.))
                        .text_color(p.accent_text),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(p.text_muted)
                        .child(if link.message {
                            "Message to subagent:"
                        } else {
                            "Subagent:"
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if link.thread.is_some() {
                            p.accent_text
                        } else {
                            p.text
                        })
                        .child(link.title.clone()),
                )
                .when(link.thread.is_some(), |this| {
                    this.child(
                        Icon::new(IconName::ArrowUpRight)
                            .with_size(px(12.))
                            .text_color(p.accent_text),
                    )
                })
                .child(status_icon(*finished, *is_error, p)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` brings GPUI's own `test` attribute into scope.
    use core::prelude::v1::test;
    use serde_json::json;

    fn call(name: &str, input: Value) -> ChatEntry {
        ChatEntry::ToolCall {
            tool_use_id: format!("t-{name}"),
            name: name.into(),
            input_json: input,
            finished: true,
            is_error: false,
            expanded: false,
            output: None,
        }
    }

    fn thought() -> ChatEntry {
        ChatEntry::Thinking { text: "hmm".into() }
    }

    fn text(markdown: &str) -> ChatEntry {
        ChatEntry::Assistant {
            markdown: markdown.into(),
            streaming: false,
            message_id: None,
        }
    }

    fn kinds(lines: &[DiffLine]) -> String {
        lines
            .iter()
            .map(|line| match line.kind {
                LineKind::Added => '+',
                LineKind::Removed => '-',
                LineKind::Context => ' ',
                LineKind::NoNewline => '\\',
            })
            .collect()
    }

    #[test]
    fn tools_are_classified_by_name() {
        assert_eq!(tool_kind("Edit"), ToolKind::Edit);
        assert_eq!(tool_kind("Write"), ToolKind::Edit);
        assert_eq!(tool_kind("Bash"), ToolKind::Command);
        assert_eq!(tool_kind("mcp__trama__spawn_subagent"), ToolKind::Subthread);
        assert_eq!(tool_kind("mcp__trama__send_to_thread"), ToolKind::Subthread);
        assert_eq!(tool_kind("Grep"), ToolKind::Minor(Activity::Search));
        assert_eq!(tool_kind("Task"), ToolKind::Minor(Activity::Subagent));
        assert_eq!(
            tool_kind("mcp__github__get_issue"),
            ToolKind::Minor(Activity::Other)
        );
    }

    #[test]
    fn runs_fold_into_groups_and_edit_blocks() {
        let entries = vec![
            thought(),
            call("Skill", json!({})),
            call("Grep", json!({})),
            text("Found it."),
            call("Read", json!({})),
            call("Edit", json!({})),
            call("Write", json!({})),
            call("Bash", json!({})),
            thought(),
            call("mcp__trama__spawn_subagent", json!({})),
        ];
        assert_eq!(
            layout_items(&entries),
            vec![
                Item::Group(0..3),
                Item::Single(3),
                Item::Single(4),
                Item::Edits(5..7),
                Item::Single(7),
                Item::Group(8..9),
                Item::Single(9),
            ]
        );
    }

    #[test]
    fn group_summary_counts_in_first_seen_order() {
        let entries = vec![
            thought(),
            call("Skill", json!({})),
            thought(),
            call("Grep", json!({})),
            call("Glob", json!({})),
            call("Read", json!({})),
            call("Read", json!({})),
            thought(),
            call("Grep", json!({})),
            call("WebFetch", json!({})),
            call("Task", json!({})),
            call("mcp__x__y", json!({})),
        ];
        assert_eq!(
            group_summary(&entries),
            "Thought 3 times, loaded a skill, ran 3 searches, read 2 files, browsed the web, ran a subagent, called a tool"
        );
        assert_eq!(group_summary(&[thought()]), "Thought once");
        assert_eq!(group_summary(&[]), "");
    }

    #[test]
    fn edit_input_becomes_a_numbered_unified_diff() {
        let input = json!({
            "file_path": "/repo/src/auth.rs",
            "old_string": "fn login() {\n    todo!()\n}",
            "new_string": "fn login() {\n    let token = issue();\n    Session::new(token)\n}",
        });
        let output = json!(
            "The file /repo/src/auth.rs has been updated. Here's the result of running `cat -n` on a snippet of the edited file:\n    40→}\n    41→\n    42→fn login() {\n    43→    let token = issue();"
        );
        let edit = file_edit("Edit", &input, Some(&output)).unwrap();
        assert_eq!(edit.verb, "Edit");
        assert_eq!(edit.path, "/repo/src/auth.rs");
        assert_eq!((edit.additions, edit.deletions, edit.edits), (2, 1, 1));
        assert_eq!(edit.hunks.len(), 1);
        let hunk = &edit.hunks[0];
        assert_eq!(hunk.header, "@@ -42,3 +42,4 @@");
        assert_eq!(kinds(&hunk.lines), " -++ ");
        assert_eq!(hunk.lines[1].old_line, Some(43));
        assert_eq!(hunk.lines[2].new_line, Some(43));
        assert_eq!(hunk.lines[3].text, "    Session::new(token)");
        assert_eq!(hunk.lines[4].old_line, Some(44));
        assert_eq!(hunk.lines[4].new_line, Some(45));

        // Without a snippet the diff counts from line 1.
        let bare = file_edit("Edit", &input, None).unwrap();
        assert_eq!(bare.hunks[0].header, "@@ -1,3 +1,4 @@");
    }

    #[test]
    fn multi_edit_write_and_notebook_inputs() {
        let multi = file_edit(
            "MultiEdit",
            &json!({"file_path": "a.rs", "edits": [
                {"old_string": "a", "new_string": "b"},
                {"old_string": "c\nd", "new_string": "c"},
            ]}),
            None,
        )
        .unwrap();
        assert_eq!(multi.edits, 2);
        assert_eq!(multi.hunks.len(), 2);
        assert_eq!((multi.additions, multi.deletions), (1, 2));

        let write = file_edit(
            "Write",
            &json!({"file_path": "new.md", "content": "# Title\n\nBody\n"}),
            None,
        )
        .unwrap();
        assert_eq!(write.verb, "Write");
        assert_eq!((write.additions, write.deletions), (3, 0));
        assert_eq!(write.hunks[0].header, "@@ -0,0 +1,3 @@");

        let notebook = file_edit(
            "NotebookEdit",
            &json!({"notebook_path": "n.ipynb", "new_source": "x = 1", "edit_mode": "insert"}),
            None,
        )
        .unwrap();
        assert_eq!(notebook.verb, "Edit notebook");
        assert_eq!(notebook.additions, 1);
        assert!(file_edit("Edit", &json!({}), None).is_none());

        assert_eq!(
            edits_header(&[multi.clone(), write, multi]),
            "Applied 5 edits across 2 files"
        );
    }

    #[test]
    fn long_unchanged_runs_split_into_hunks() {
        let old: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let new = old
            .replace("line 2\n", "line two\n")
            .replace("line 18\n", "line eighteen\n");
        let lines = line_diff(&old, &new, 1, 1);
        let hunks = build_hunks(&lines, 3);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].header, "@@ -1,5 +1,5 @@");
        assert_eq!(hunks[1].header, "@@ -15,6 +15,6 @@");
        assert!(build_hunks(&line_diff("same", "same", 1, 1), 3).is_empty());
    }

    #[test]
    fn bash_results_read_exit_codes() {
        assert_eq!(
            command_result(true, true, "Exit code 2\nerror: no such file"),
            (ExitStatus::Code(2), "error: no such file".into())
        );
        assert_eq!(
            command_result(true, false, "ok\n"),
            (ExitStatus::Success, "ok\n".into())
        );
        assert_eq!(
            command_result(true, true, "permission denied"),
            (ExitStatus::Failed, "permission denied".into())
        );
        assert_eq!(
            command_result(true, true, "[Request interrupted by user for tool use]").0,
            ExitStatus::Interrupted
        );
        assert_eq!(command_result(false, false, "").0, ExitStatus::Running);
        assert_eq!(
            output_text(&json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])),
            "a\nb"
        );
        assert_eq!(output_text(&json!({"stdout": "out", "stderr": ""})), "out");
    }

    #[test]
    fn ansi_colors_are_kept_and_other_escapes_dropped() {
        let lines = parse_ansi(
            "\u{1b}[1;32mok\u{1b}[0m done\n\u{1b}]0;title\u{7}\u{1b}[2Kprogress 10%\rprogress 100%\n\u{1b}[38;5;196mred\u{1b}[39m\tx\n\n",
        );
        assert_eq!(lines.len(), 3, "trailing blank lines are dropped");
        assert_eq!(
            lines[0],
            vec![
                AnsiSpan {
                    text: "ok".into(),
                    style: AnsiStyle {
                        fg: Some(AnsiColor::Basic(2)),
                        bold: true,
                        dim: false
                    }
                },
                AnsiSpan {
                    text: " done".into(),
                    style: AnsiStyle::default()
                },
            ]
        );
        assert_eq!(line_text(&lines[1]), "progress 100%");
        assert_eq!(lines[2][0].style.fg, Some(AnsiColor::Rgb(255, 0, 0)));
        assert_eq!(line_text(&lines[2]), "red    x");
    }

    #[test]
    fn output_shows_the_tail_until_expanded() {
        assert_eq!(visible_tail(40, 12, false), (28, 28));
        assert_eq!(visible_tail(40, 12, true), (0, 0));
        assert_eq!(visible_tail(12, 12, false), (0, 0));
        assert_eq!(format_duration(420), "420ms");
        assert_eq!(format_duration(2_340), "2.3s");
        assert_eq!(format_duration(64_000), "1m 04s");
        let timing = ToolTiming {
            started_ms: 1_000,
            finished_ms: Some(3_500),
        };
        assert_eq!(timing.duration_ms(), Some(2_500));
    }

    #[test]
    fn subthread_links_resolve_by_id_then_title() {
        let threads = vec![
            (ThreadId::from("01ABC"), "Fix the parser".to_string()),
            (ThreadId::from("01DEF"), "Write docs".to_string()),
        ];
        let by_output = subthread_link(
            "mcp__trama__spawn_subagent",
            &json!({"title": "Fix the parser", "prompt": "…"}),
            Some(&json!([{"type": "text", "text": "{\"thread_id\": \"01ABC\"}"}])),
            &threads,
        );
        assert_eq!(by_output.title, "Fix the parser");
        assert_eq!(by_output.thread, Some(ThreadId::from("01ABC")));
        assert!(!by_output.message);

        let by_title = subthread_link(
            "mcp__trama__spawn_subagent",
            &json!({"title": "write docs"}),
            None,
            &threads,
        );
        assert_eq!(by_title.thread, Some(ThreadId::from("01DEF")));

        let message = subthread_link(
            "mcp__trama__send_to_thread",
            &json!({"thread_id": "01DEF", "text": "go on"}),
            None,
            &threads,
        );
        assert!(message.message);
        assert_eq!(message.title, "Write docs");

        let unknown = subthread_link(
            "mcp__trama__spawn_subagent",
            &json!({"title": "Elsewhere"}),
            Some(&json!("Started thread 01ZZZ")),
            &threads,
        );
        assert_eq!(unknown.thread, None);
        assert_eq!(unknown.title, "Elsewhere");
    }

    #[test]
    fn error_tags_are_stripped() {
        assert_eq!(
            strip_tags("<tool_use_error>String to replace not found</tool_use_error>"),
            "String to replace not found"
        );
        assert_eq!(strip_tags("a < b > c"), "a < b > c");
    }
}
