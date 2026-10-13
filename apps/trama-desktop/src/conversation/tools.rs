//! Tool call views in the transcript, drawn as document lines.
//!
//! Runs of thinking and tool calls fold into one muted summary line
//! ("Thought 3 times, ran 2 commands, read 3 files ›") that opens to a
//! line per step ("Thinking: <first line> ›", "Ran <command> ›", "Read
//! <path> ›"); a step opens to the thought, the command and the tail of
//! its output ("Show N more lines"), or the call's input and output. File
//! edits (Edit, MultiEdit, Write, NotebookEdit) are "Apply patch <path> ⌄"
//! lines over the diff built from the tool input, whose lines can be picked
//! and commented on. Trama subagent calls are "Agent spawned: <title>"
//! lines that open the subagent next to the transcript. All their text is
//! selectable and commentable ([`crate::doc_text`]). The pure helpers come
//! first and are tested; the renderers below them only lay out what they
//! compute.

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
use crate::comments;
use crate::diff::{DiffLine, Hunk, LineKind};
use crate::doc_text::{BlockTexts, Mark, code_block, escape_markdown};
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
    Command,
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

/// MCP server names of Trama's own tools: the daemon's `trama-daemon mcp`
/// helper registers `trama-workspace`; `trama` is accepted as well.
const TRAMA_MCP_SERVERS: &[&str] = &["trama-workspace", "trama"];

/// `spawn_subagent` / `send_to_thread` of a Trama MCP server.
fn is_trama_subthread_tool(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("mcp__") else {
        return false;
    };
    TRAMA_MCP_SERVERS.iter().any(|server| {
        rest.strip_prefix(server)
            .and_then(|tool| tool.strip_prefix("__"))
            .is_some_and(|tool| matches!(tool, "spawn_subagent" | "send_to_thread"))
    })
}

pub fn tool_kind(name: &str) -> ToolKind {
    if is_trama_subthread_tool(name) {
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
            ToolKind::Command => Some(Activity::Command),
            _ => None,
        },
        _ => None,
    }
}

fn is_edit(entry: &ChatEntry) -> bool {
    matches!(entry, ChatEntry::ToolCall { name, .. } if tool_kind(name) == ToolKind::Edit)
}

/// One transcript block's worth of entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Single(usize),
    /// Thinking, commands and minor tool calls folded into a summary line.
    Group(Range<usize>),
    /// Consecutive file edits, an "Apply patch" line each.
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

/// Fold entries into blocks. A lone step (a thought, a command, a call)
/// keeps its own line.
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
            items.push(if index - start == 1 {
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

/// "Thought 2 times, loaded a skill, ran 3 commands, read 6 files", in the
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
            Activity::Command => ("ran a command", format!("ran {n} commands")),
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
    /// Summary lines opened, by their first entry index.
    pub open_groups: HashSet<usize>,
    /// Thoughts opened, by entry index.
    pub open_thoughts: HashSet<usize>,
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
        self.open_thoughts.clear();
        self.edit_open.clear();
        self.full_output.clear();
        self.timings.clear();
        self.edits.borrow_mut().clear();
    }

    /// The call's diff, built once per state of the call.
    pub fn file_edit(&self, entry: &ChatEntry) -> Option<Rc<FileEdit>> {
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

pub fn status_icon(finished: bool, is_error: bool, p: &Palette) -> AnyElement {
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

// -- document lines ---------------------------------------------------------

/// What the tool views need from the transcript while drawing.
pub struct DocCtx<'a> {
    pub p: &'a Palette,
    pub state: &'a ToolViewState,
    pub texts: &'a mut BlockTexts,
    /// Comments on this turn's blocks, to highlight and place.
    pub marks: &'a [Mark],
    pub turn: usize,
}

impl DocCtx<'_> {
    /// Selectable plain text.
    fn text(
        &mut self,
        key: &str,
        plain: &str,
        kind: &'static str,
        cx: &mut Context<ChatPanel>,
    ) -> AnyElement {
        self.texts.view(
            key,
            &escape_markdown(plain),
            plain,
            kind,
            self.turn,
            self.marks,
            cx,
        )
    }

    /// Selectable Markdown.
    fn markdown(
        &mut self,
        key: &str,
        markdown: &str,
        kind: &'static str,
        cx: &mut Context<ChatPanel>,
    ) -> AnyElement {
        self.texts
            .view(key, markdown, markdown, kind, self.turn, self.marks, cx)
    }
}

/// What a click on a document line does.
pub type LineClick = Rc<dyn Fn(&mut ChatPanel, &ClickEvent, &mut Window, &mut Context<ChatPanel>)>;

/// A muted document line: a label, its selectable text, what trails it,
/// and a chevron when it opens. A click acts unless it ended a selection
/// in the line's text (`text_key`).
#[allow(clippy::too_many_arguments)]
fn line(
    id: impl Into<ElementId>,
    label: Option<String>,
    text: AnyElement,
    text_key: String,
    trailing: Vec<AnyElement>,
    open: Option<bool>,
    click: Option<LineClick>,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let row = h_flex()
        .id(id.into())
        .w_full()
        .min_h(px(24.))
        .gap(theme::sp(1.5))
        .items_start()
        .text_size(theme::text_ui())
        .line_height(px(22.))
        .text_color(p.text_faint)
        .children(label.map(|label| div().flex_shrink_0().text_color(p.text_muted).child(label)))
        .child(div().min_w_0().flex_shrink(1.).child(text))
        .children(
            trailing
                .into_iter()
                .map(|element| div().flex_shrink_0().pt(px(4.)).child(element)),
        )
        .children(open.map(|open| div().flex_shrink_0().pt(px(5.)).child(chevron(open, p))));
    match click {
        Some(click) => row
            .cursor_pointer()
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if this.text_selected(&text_key, cx) {
                    return;
                }
                click(this, event, window, cx);
            }))
            .into_any_element(),
        None => row.into_any_element(),
    }
}

/// The first line of a thought, as plain text ("Thinking: <line>").
pub fn thinking_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .trim_matches(|c: char| matches!(c, '*' | '_' | '#' | '`') || c.is_whitespace())
        .trim_end_matches(':');
    let mut out: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
        out.push('…');
    }
    out
}

/// A tool call as one line: what it did and on what ("Read src/a.rs",
/// "Ran cargo test", "Apply patch src/a.rs", "Called get_issue").
pub fn call_line(name: &str, input: &Value) -> (String, String) {
    let field = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| input.get(*key).and_then(Value::as_str))
            .map(|text| text.lines().next().unwrap_or_default().trim().to_string())
    };
    let summary = || crate::chat::tool_summary(name, input);
    let (label, detail) = match base_name(name) {
        "Bash" => ("Ran", field(&["command"]).unwrap_or_else(summary)),
        "Read" | "NotebookRead" => (
            "Read",
            field(&["file_path", "notebook_path", "path"])
                .map(|path| display_path(&path))
                .unwrap_or_else(summary),
        ),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => (
            "Apply patch",
            field(&["file_path", "notebook_path"])
                .map(|path| display_path(&path))
                .unwrap_or_else(summary),
        ),
        "Grep" | "Glob" => ("Searched", summary()),
        "LS" => ("Listed", field(&["path"]).unwrap_or_else(summary)),
        "ToolSearch" => ("Searched tools for", summary()),
        "WebFetch" => ("Fetched", field(&["url"]).unwrap_or_else(summary)),
        "WebSearch" => (
            "Searched the web for",
            field(&["query"]).unwrap_or_else(summary),
        ),
        "Skill" => (
            "Loaded skill",
            field(&["skill", "name", "command"]).unwrap_or_else(summary),
        ),
        "Task" | "Agent" => (
            "Ran subagent",
            field(&["description"]).unwrap_or_else(summary),
        ),
        "TodoWrite" => ("Updated the plan", summary()),
        other => ("Called", other.to_string()),
    };
    (label.to_string(), detail)
}

/// "<title> (scout · Opus 4.1)": what an "Agent spawned:" line names.
pub fn agent_detail(title: &str, profile: Option<&str>, model: Option<&str>) -> String {
    let title = if title.trim().is_empty() {
        "Untitled"
    } else {
        title.trim()
    };
    let model = model.map(crate::turns::model_label);
    match (profile, model) {
        (Some(profile), Some(model)) => format!("{title} ({profile} \u{b7} {model})"),
        (Some(profile), None) => format!("{title} ({profile})"),
        (None, Some(model)) => format!("{title} ({model})"),
        (None, None) => title.to_string(),
    }
}

/// The label of a subagent's lifecycle line.
pub fn agent_label(
    status: Option<trama_domain::subagent::SubagentStatus>,
    spawned: bool,
) -> &'static str {
    use trama_domain::subagent::SubagentStatus;
    match (spawned, status) {
        (true, _) => "Agent spawned:",
        (false, Some(SubagentStatus::Failed)) => "Agent failed:",
        (false, _) => "Agent completed:",
    }
}

/// Thinking and tool calls: one line, or a folded run as one summary line
/// ("Thought 3 times, ran 2 commands, read 3 files ›") opening to a line
/// per step.
pub fn render_activity(
    range: Range<usize>,
    entries: &[ChatEntry],
    ctx: &mut DocCtx,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    if range.len() == 1 {
        return render_step(range.start, &entries[range.start], ctx, cx);
    }
    let p = ctx.p;
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
    let start = range.start;
    let open = ctx.state.open_groups.contains(&start);
    let key = format!("group-{start}");
    let summary = group_summary(slice);
    let text = ctx.text(&key, &summary, comments::KIND_ACTIVITY, cx);
    let mut trailing = Vec::new();
    if failed > 0 {
        trailing.push(
            div()
                .text_size(theme::text_xs())
                .text_color(p.danger)
                .child(format!("{failed} failed"))
                .into_any_element(),
        );
    }
    if running {
        trailing.push(Spinner::new().xsmall().color(p.accent).into_any_element());
    }
    let header = line(
        ("activity-group", start),
        None,
        text,
        key,
        trailing,
        Some(open),
        Some(Rc::new(move |this, _, _, cx| this.toggle_group(start, cx))),
        p,
        cx,
    );
    let body = open.then(|| {
        let mut list = v_flex()
            .w_full()
            .pl(theme::sp(3.))
            .ml(theme::sp(1.))
            .gap(theme::sp(0.5))
            .border_l_1()
            .border_color(p.border);
        for (offset, entry) in slice.iter().enumerate() {
            list = list.child(render_step(start + offset, entry, ctx, cx));
        }
        list
    });
    v_flex()
        .w_full()
        .gap(theme::sp(0.5))
        .child(header)
        .children(body)
        .into_any_element()
}

/// One step: "Thinking: <first line> ›", "Ran <command> ›", "Read <path> ›";
/// opening shows the thought, the command and its output, or the call's
/// input and output.
fn render_step(
    ix: usize,
    entry: &ChatEntry,
    ctx: &mut DocCtx,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let p = ctx.p;
    match entry {
        ChatEntry::Thinking { text } => {
            let open = ctx.state.open_thoughts.contains(&ix);
            let key = format!("thinking-line-{ix}");
            let first = thinking_line(text);
            let detail = ctx.text(&key, &first, comments::KIND_THINKING, cx);
            let header = line(
                ("thinking", ix),
                Some("Thinking:".into()),
                detail,
                key,
                Vec::new(),
                Some(open),
                Some(Rc::new(move |this, _, _, cx| this.toggle_thought(ix, cx))),
                p,
                cx,
            );
            let body = open.then(|| {
                let text = ctx.markdown(
                    &format!("thinking-{ix}"),
                    text.trim(),
                    comments::KIND_THINKING,
                    cx,
                );
                div()
                    .w_full()
                    .pl(theme::sp(3.))
                    .pb(theme::sp(1.))
                    .text_size(theme::text_ui())
                    .line_height(relative(1.55))
                    .text_color(p.text_muted)
                    .child(text)
            });
            v_flex()
                .w_full()
                .child(header)
                .children(body)
                .into_any_element()
        }
        ChatEntry::ToolCall {
            tool_use_id,
            name,
            input_json,
            finished,
            is_error,
            expanded,
            ..
        } => {
            let command = tool_kind(name) == ToolKind::Command;
            let (label, detail) = call_line(name, input_json);
            let key = format!("step-{tool_use_id}");
            let text = ctx.text(
                &key,
                &detail,
                if command {
                    comments::KIND_COMMAND
                } else {
                    comments::KIND_ACTIVITY
                },
                cx,
            );
            let mut trailing = Vec::new();
            if !*finished || *is_error {
                trailing.push(status_icon(*finished, *is_error, p));
            }
            let header = line(
                ("step", ix),
                Some(label),
                text,
                key,
                trailing,
                Some(*expanded),
                Some(Rc::new(move |this, _, _, cx| this.toggle_tool(ix, cx))),
                p,
                cx,
            );
            let body = expanded.then(|| {
                if command {
                    render_command_body(ix, entry, ctx, cx)
                } else {
                    render_call_body(entry, ctx, cx)
                }
            });
            v_flex()
                .w_full()
                .child(header)
                .children(body)
                .into_any_element()
        }
        _ => div().into_any_element(),
    }
}

/// An opened command: the command and the tail of its output as one
/// selectable block, with "Show N more lines" above it.
fn render_command_body(
    ix: usize,
    entry: &ChatEntry,
    ctx: &mut DocCtx,
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
    let p = ctx.p;
    let command = input_json
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let text = output.as_ref().map(output_text).unwrap_or_default();
    let (status, body) = command_result(*finished, *is_error, &text);
    let lines: Vec<String> = parse_ansi(&body).iter().map(line_text).collect();
    let show_all = ctx.state.full_output.contains(tool_use_id);
    let (first, hidden) = visible_tail(lines.len(), OUTPUT_TAIL, show_all);
    let end = lines.len().min(first + MAX_RENDERED_LINES);
    let mut block = format!("$ {command}");
    for line in &lines[first..end] {
        block.push('\n');
        block.push_str(line);
    }
    if end < lines.len() {
        block.push_str(&format!("\n… {} more lines not shown", lines.len() - end));
    }
    let duration = ctx
        .state
        .timings
        .get(tool_use_id)
        .and_then(ToolTiming::duration_ms)
        .map(format_duration);
    let status_text = match status {
        ExitStatus::Running => Some(("Running…".to_string(), p.accent_text)),
        ExitStatus::Success => None,
        ExitStatus::Code(code) => Some((format!("Exit code {code}"), p.danger)),
        ExitStatus::Failed => Some(("Failed".to_string(), p.danger)),
        ExitStatus::Interrupted => Some(("Interrupted".to_string(), p.warning)),
    };
    let id = tool_use_id.clone();
    let toggle = (hidden > 0 || show_all && lines.len() > OUTPUT_TAIL).then(|| {
        h_flex()
            .id(("command-more", ix))
            .gap(theme::sp(1.))
            .items_center()
            .cursor_pointer()
            .text_size(theme::text_xs())
            .text_color(p.text_muted)
            .hover(|style| style.text_color(p.text))
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
                    comments::plural(hidden, "more line", "more lines")
                )
            })
    });
    let code = ctx.markdown(
        &format!("command-{tool_use_id}"),
        &code_block(&block, ""),
        comments::KIND_COMMAND,
        cx,
    );
    v_flex()
        .w_full()
        .pl(theme::sp(3.))
        .py(theme::sp(1.))
        .gap(theme::sp(1.))
        .children(toggle)
        .child(div().w_full().text_size(theme::text_xs()).child(code))
        .when(status_text.is_some() || duration.is_some(), |this| {
            let (label, color) = status_text.unwrap_or_else(|| ("Done".into(), p.text_faint));
            this.child(
                h_flex()
                    .gap(theme::sp(2.))
                    .text_size(theme::text_xs())
                    .child(div().text_color(color).child(label))
                    .children(duration.map(|d| div().text_color(p.text_faint).child(d))),
            )
        })
        .into_any_element()
}

/// An opened call: its input and output as one selectable block.
fn render_call_body(
    entry: &ChatEntry,
    ctx: &mut DocCtx,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let ChatEntry::ToolCall {
        tool_use_id,
        input_json,
        is_error,
        output,
        ..
    } = entry
    else {
        return div().into_any_element();
    };
    let mut markdown = format!("Input\n\n{}", code_block(&pretty_json(input_json), "json"));
    if let Some(output) = output {
        let label = if *is_error { "Error" } else { "Output" };
        let text = output_text(output);
        let text = if text.len() > OUTPUT_LIMIT {
            let mut end = OUTPUT_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}\n… ({} more bytes)", &text[..end], text.len() - end)
        } else {
            text
        };
        markdown.push_str(&format!("\n\n{label}\n\n{}", code_block(&text, "")));
    }
    let view = ctx.markdown(
        &format!("call-{tool_use_id}"),
        &markdown,
        comments::KIND_TOOL,
        cx,
    );
    div()
        .w_full()
        .pl(theme::sp(3.))
        .py(theme::sp(1.))
        .text_size(theme::text_xs())
        .text_color(ctx.p.text_muted)
        .child(view)
        .into_any_element()
}

/// Bytes of a call's input or output shown when opened.
const OUTPUT_LIMIT: usize = 4000;

/// A tool input or result as readable text: strings as they are, other
/// JSON pretty-printed, capped.
pub fn pretty_json(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    if text.len() > OUTPUT_LIMIT {
        let mut end = OUTPUT_LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n… ({} more bytes)", &text[..end], text.len() - end)
    } else {
        text
    }
}

/// Diff lines picked with the mouse in one hunk of an edit shown in the
/// transcript. Typing while lines are picked starts a comment on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchPick {
    pub tool_use_id: String,
    pub hunk: usize,
    pub anchor: usize,
    pub cursor: usize,
}

impl PatchPick {
    pub fn lines(&self) -> std::ops::RangeInclusive<usize> {
        self.anchor.min(self.cursor)..=self.anchor.max(self.cursor)
    }

    pub fn contains(&self, tool_use_id: &str, hunk: usize, line: usize) -> bool {
        self.tool_use_id == tool_use_id && self.hunk == hunk && self.lines().contains(&line)
    }
}

/// One diff line as the comment quotes it: `+ text`, `- text`, `  text`.
pub fn quoted_line(line: &DiffLine) -> String {
    let sign = match line.kind {
        LineKind::Added => '+',
        LineKind::Removed => '-',
        _ => ' ',
    };
    format!("{sign} {}", line.text)
}

/// "Apply patch <path> ⌄" and, open, the edit's diff. Lines can be picked
/// (click, drag, shift-click) and commented on; `marked` holds the
/// (hunk, line) of lines with comments. `None` when the input is not an
/// edit this view can read.
pub fn render_patch(
    ix: usize,
    entry: &ChatEntry,
    ctx: &mut DocCtx,
    pick: Option<&PatchPick>,
    marked: &HashSet<(usize, usize)>,
    cx: &mut Context<ChatPanel>,
) -> Option<AnyElement> {
    let ChatEntry::ToolCall {
        tool_use_id,
        finished,
        is_error,
        output,
        ..
    } = entry
    else {
        return None;
    };
    let edit = ctx.state.file_edit(entry)?;
    let p = ctx.p;
    let lines = edit.line_count();
    let open = ctx
        .state
        .edit_open
        .get(tool_use_id)
        .copied()
        .unwrap_or(lines <= OPEN_EDIT_LINES && !is_error);
    let key = format!("patch-{tool_use_id}");
    let path = display_path(&edit.path);
    let text = ctx.text(&key, &path, comments::KIND_PATCH, cx);
    let mut trailing = vec![counts(edit.additions, edit.deletions, p).into_any_element()];
    if !*finished || *is_error {
        trailing.push(status_icon(*finished, *is_error, p));
    }
    let id = tool_use_id.clone();
    let header = line(
        ("patch", ix),
        Some("Apply patch".into()),
        text,
        key,
        trailing,
        (lines > 0).then_some(open),
        Some(Rc::new(move |this, _, _, cx| {
            this.toggle_edit(&id, open, cx)
        })),
        p,
        cx,
    );
    let error = is_error
        .then(|| output.as_ref().map(output_text))
        .flatten()
        .map(|text| comments::one_line(&strip_tags(&text), 160));
    let body = (open && lines > 0).then(|| {
        let mut column = v_flex()
            .flex_shrink_0()
            .min_w_full()
            .py(theme::sp(1.5))
            .font_family(theme::MONO_FONT)
            .text_size(theme::text_xs())
            .line_height(relative(1.6));
        let mut drawn = 0;
        for (hunk_ix, hunk) in edit.hunks.iter().enumerate() {
            if hunk_ix > 0 {
                column = column.child(div().w_full().h(theme::sp(3.)));
            }
            for (line_ix, diff_line) in hunk.lines.iter().enumerate() {
                if drawn == MAX_RENDERED_LINES {
                    break;
                }
                drawn += 1;
                let picked = pick.is_some_and(|pick| pick.contains(tool_use_id, hunk_ix, line_ix));
                let commented = marked.contains(&(hunk_ix, line_ix));
                let at = PatchLine {
                    tool_use_id: tool_use_id.clone(),
                    hunk: hunk_ix,
                    line: line_ix,
                };
                let drag_at = at.clone();
                column = column.child(
                    diff_row(diff_line, picked, commented, p)
                        .id(SharedString::from(format!(
                            "patch-line-{tool_use_id}-{hunk_ix}-{line_ix}"
                        )))
                        .cursor_text()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                this.press_patch_line(
                                    at.clone(),
                                    event.modifiers.shift,
                                    window,
                                    cx,
                                );
                            }),
                        )
                        .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                            if event.pressed_button == Some(MouseButton::Left) {
                                this.drag_patch_line(drag_at.clone(), cx);
                            }
                        })),
                );
            }
        }
        if lines > drawn {
            column = column.child(
                div()
                    .px(theme::sp(3.))
                    .py(theme::sp(1.))
                    .font_family(theme::UI_FONT)
                    .text_color(p.text_faint)
                    .child(format!("{} more lines not shown", lines - drawn)),
            );
        }
        h_flex()
            .id(("patch-body", ix))
            .w_full()
            .rounded(theme::radius())
            .border_1()
            .border_color(if *is_error {
                p.danger.opacity(0.4)
            } else {
                p.border
            })
            .bg(p.panel)
            .overflow_x_scroll()
            .child(column)
    });
    Some(
        v_flex()
            .w_full()
            .gap(theme::sp(1.))
            .child(header)
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .pl(theme::sp(3.))
                        .text_size(theme::text_xs())
                        .text_color(p.danger)
                        .child(error),
                )
            })
            .children(body)
            .into_any_element(),
    )
}

/// New-side line numbers an edit's diff shows.
pub fn new_lines(edit: &FileEdit) -> Vec<u32> {
    edit.hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter_map(|line| line.new_line)
        .collect()
}

/// (hunk, line) of the diff lines showing new-side lines `range`.
pub fn patch_lines_in(
    edit: &FileEdit,
    range: std::ops::RangeInclusive<u32>,
) -> HashSet<(usize, usize)> {
    let mut out = HashSet::new();
    for (hunk_ix, hunk) in edit.hunks.iter().enumerate() {
        for (line_ix, line) in hunk.lines.iter().enumerate() {
            if line.new_line.is_some_and(|number| range.contains(&number)) {
                out.insert((hunk_ix, line_ix));
            }
        }
    }
    out
}

/// (hunk, line) of the diff lines a comment quoted (`+ text`, `- text`),
/// when every quoted line is in one hunk of the edit; empty otherwise.
pub fn patch_lines_matching(edit: &FileEdit, quoted: &[&str]) -> HashSet<(usize, usize)> {
    if quoted.is_empty() {
        return HashSet::new();
    }
    for (hunk_ix, hunk) in edit.hunks.iter().enumerate() {
        let shown: Vec<String> = hunk.lines.iter().map(quoted_line).collect();
        let found: Option<HashSet<(usize, usize)>> = quoted
            .iter()
            .map(|wanted| {
                shown
                    .iter()
                    .position(|line| line.trim_end() == wanted.trim_end())
                    .map(|line_ix| (hunk_ix, line_ix))
            })
            .collect();
        if let Some(found) = found {
            return found;
        }
    }
    HashSet::new()
}

/// Where a pressed diff line is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchLine {
    pub tool_use_id: String,
    pub hunk: usize,
    pub line: usize,
}

fn diff_row(line: &DiffLine, picked: bool, commented: bool, p: &Palette) -> Div {
    let (bg, sign, sign_color, text_color) = match line.kind {
        LineKind::Added => (Some(p.diff_add_bg), "+", p.diff_add_text, p.text),
        LineKind::Removed => (Some(p.diff_del_bg), "-", p.diff_del_text, p.text),
        LineKind::Context => (None, " ", p.text_faint, p.text_muted),
        LineKind::NoNewline => (None, " ", p.text_faint, p.text_faint),
    };
    let bg = if picked {
        Some(p.accent.opacity(0.24))
    } else if commented {
        Some(comments::mark_color(p).opacity(0.2))
    } else {
        bg
    };
    h_flex()
        .w_full()
        .px(theme::sp(3.))
        .when_some(bg, |this, bg| this.bg(bg))
        .child(
            div()
                .w(px(16.))
                .flex_shrink_0()
                .text_color(sign_color)
                .child(sign),
        )
        .child(
            div()
                .flex_shrink_0()
                .pr(theme::sp(3.))
                .whitespace_nowrap()
                .text_color(text_color)
                .child(line.text.replace('\t', "    ")),
        )
}

/// A subagent line: "Agent spawned: <title> (scout · model) ›", "Agent
/// completed: <title> ›", "Message to agent: <title> ›". A click opens the
/// subagent next to this transcript (⌘-click: as the selected thread).
#[allow(clippy::too_many_arguments)]
pub fn render_agent(
    id: impl Into<ElementId>,
    label: &str,
    detail: &str,
    key: String,
    thread: Option<ThreadId>,
    trailing: Vec<AnyElement>,
    tooltip: Option<String>,
    ctx: &mut DocCtx,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let p = ctx.p;
    let text = ctx.text(&key, detail, comments::KIND_SUBAGENT, cx);
    let known = thread.is_some();
    let click: Option<LineClick> = thread.map(|thread| {
        Rc::new(
            move |this: &mut ChatPanel,
                  event: &ClickEvent,
                  _: &mut Window,
                  cx: &mut Context<ChatPanel>| {
                this.open_agent(thread.clone(), event.modifiers().platform, cx);
            },
        ) as LineClick
    });
    let tooltip: Option<SharedString> = tooltip.map(Into::into);
    div()
        .id(SharedString::from(format!("{key}-box")))
        .w_full()
        .px(theme::sp(2.))
        .rounded(theme::radius())
        .border_1()
        .border_color(p.border)
        .when(known, |this| this.hover(|style| style.bg(p.hover)))
        .when_some(tooltip, |this, tooltip| {
            this.tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
            })
        })
        .child(line(
            id,
            Some(label.to_string()),
            text,
            key,
            trailing,
            known.then_some(false),
            click,
            p,
            cx,
        ))
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
        assert_eq!(
            tool_kind("mcp__trama-workspace__spawn_subagent"),
            ToolKind::Subthread
        );
        assert_eq!(
            tool_kind("mcp__trama-workspace__send_to_thread"),
            ToolKind::Subthread
        );
        assert_ne!(tool_kind("mcp__other__spawn_subagent"), ToolKind::Subthread);
        assert_ne!(
            tool_kind("mcp__trama-workspace__list_threads"),
            ToolKind::Subthread
        );
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
                // A command folds with the thought after it.
                Item::Group(7..9),
                Item::Single(9),
            ]
        );
        // A lone thought keeps its own line ("Thinking: …").
        assert_eq!(
            layout_items(&[text("a"), thought(), text("b")]),
            vec![Item::Single(0), Item::Single(1), Item::Single(2)]
        );
    }

    #[test]
    fn activity_lines_read_like_delta() {
        assert_eq!(
            group_summary(&[
                thought(),
                call("Bash", json!({"command": "ls"})),
                call("Bash", json!({"command": "pwd"})),
                call("Read", json!({"file_path": "a.rs"})),
                thought(),
            ]),
            "Thought 2 times, ran 2 commands, read a file"
        );
        assert_eq!(
            thinking_line("\n**Patching approved plan**\n\nThen more."),
            "Patching approved plan"
        );
        assert_eq!(thinking_line("## Plan:\nx"), "Plan");
        assert!(thinking_line(&"word ".repeat(40)).ends_with('…'));
        assert_eq!(
            call_line("Bash", &json!({"command": "cargo test\n--all"})),
            ("Ran".to_string(), "cargo test".to_string())
        );
        assert_eq!(
            call_line("Read", &json!({"file_path": "src/main.rs"})),
            ("Read".to_string(), "src/main.rs".to_string())
        );
        assert_eq!(
            call_line("Edit", &json!({"file_path": "plans/user.md"})),
            ("Apply patch".to_string(), "plans/user.md".to_string())
        );
        assert_eq!(
            call_line("Skill", &json!({"skill": "trama"})),
            ("Loaded skill".to_string(), "trama".to_string())
        );
        assert_eq!(
            call_line("mcp__github__get_issue", &json!({"number": 3})),
            ("Called".to_string(), "get_issue".to_string())
        );
        assert_eq!(
            agent_detail(
                "Validate target identity",
                Some("scout"),
                Some("claude-opus-4-1")
            ),
            "Validate target identity (scout \u{b7} Opus 4.1)"
        );
        assert_eq!(agent_detail(" ", None, None), "Untitled");
        assert_eq!(agent_detail("T", Some("worker"), None), "T (worker)");
        use trama_domain::subagent::SubagentStatus;
        assert_eq!(agent_label(None, true), "Agent spawned:");
        assert_eq!(
            agent_label(Some(SubagentStatus::Completed), false),
            "Agent completed:"
        );
        assert_eq!(
            agent_label(Some(SubagentStatus::Failed), false),
            "Agent failed:"
        );
    }

    #[test]
    fn comments_find_their_lines_in_an_edit() {
        let edit = file_edit(
            "Edit",
            &json!({"file_path": "/repo/a.rs", "old_string": "a\nb\nc", "new_string": "a\nB\nc"}),
            None,
        )
        .unwrap();
        // " a", "-b", "+B", " c"
        assert_eq!(new_lines(&edit), vec![1, 2, 3]);
        let on_b: HashSet<(usize, usize)> = patch_lines_in(&edit, 2..=2);
        assert_eq!(on_b, HashSet::from([(0, 2)]));
        let quoted = patch_lines_matching(&edit, &["- b", "+ B"]);
        assert_eq!(quoted, HashSet::from([(0, 1), (0, 2)]));
        assert!(patch_lines_matching(&edit, &["- b", "+ nope"]).is_empty());
        assert!(patch_lines_matching(&edit, &[]).is_empty());
    }

    #[test]
    fn patch_picks_span_lines_of_one_hunk() {
        let pick = PatchPick {
            tool_use_id: "e1".into(),
            hunk: 0,
            anchor: 4,
            cursor: 2,
        };
        assert_eq!(pick.lines(), 2..=4);
        assert!(pick.contains("e1", 0, 3));
        assert!(!pick.contains("e1", 1, 3));
        assert!(!pick.contains("e2", 0, 3));
        let line = DiffLine {
            kind: LineKind::Removed,
            old_line: Some(3),
            new_line: None,
            text: "old".into(),
        };
        assert_eq!(quoted_line(&line), "- old");
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
        assert_eq!(multi.path, "a.rs");
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

        // The daemon's own MCP server (`trama-workspace`) answers in prose.
        let daemon = subthread_link(
            "mcp__trama-workspace__spawn_subagent",
            &json!({"title": "Scout the parser", "profile": "scout"}),
            Some(
                &json!([{"type": "text", "text": "Spawned scout subagent thread 01ABC (Scout the parser). It runs on its own."}]),
            ),
            &threads,
        );
        assert_eq!(daemon.thread, Some(ThreadId::from("01ABC")));
        assert!(!daemon.message);
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
