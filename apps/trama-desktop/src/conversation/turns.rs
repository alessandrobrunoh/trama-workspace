//! Turn rail: the transcript split into turns, the rail's dash geometry,
//! the hover card's text, and bookmarked turns.
//!
//! A turn is a prompt and everything the agent did to answer it. The rail
//! on the transcript's right edge stacks one short dash per turn, evenly
//! spaced; the turn being read gets a longer, brighter dash. Turn extents
//! work in content pixels (0 = top of the scroll content) so the current
//! turn and ⌥+scroll targets can be tested without a window.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;

use chrono::{DateTime, Local, Utc};
use trama_domain::ids::{MessageId, ThreadId};

use crate::chat::ChatEntry;
use crate::comments;

/// One prompt and the agent's answer, as a range of transcript entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub entries: Range<usize>,
    /// The stored prompt, once the daemon has persisted it.
    pub message_id: Option<MessageId>,
    /// The person's text (without a delivered `<review-comments>` block).
    pub question: String,
    /// First sentence of the turn's final answer.
    pub summary: Option<String>,
}

/// Split entries into turns: each prompt starts one. Agent entries before
/// the first prompt (a turn started elsewhere) form a turn of their own.
pub fn segment_turns(entries: &[ChatEntry]) -> Vec<Turn> {
    let mut starts: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| matches!(entry, ChatEntry::User { .. }))
        .map(|(index, _)| index)
        .collect();
    if !entries.is_empty() && starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    let mut turns = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(entries.len());
        let (question, message_id) = match &entries[start] {
            ChatEntry::User { text, message_id } => (
                comments::prompt_body(text).trim().to_string(),
                message_id.clone(),
            ),
            _ => (String::new(), None),
        };
        let summary = entries[start..end]
            .iter()
            .rev()
            .find_map(|entry| match entry {
                ChatEntry::Assistant { markdown, .. } => first_sentence(markdown, SUMMARY_LIMIT),
                _ => None,
            });
        turns.push(Turn {
            entries: start..end,
            message_id,
            question,
            summary,
        });
    }
    turns
}

const SUMMARY_LIMIT: usize = 140;

/// The question as shown on the card: its first lines, capped.
pub fn question_preview(question: &str, max_lines: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = question
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let mut out = lines
        .iter()
        .take(max_lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let mut cut = lines.len() > max_lines;
    if out.chars().count() > max_chars {
        out = out
            .chars()
            .take(max_chars)
            .collect::<String>()
            .trim_end()
            .to_string();
        cut = true;
    }
    if cut {
        out.push('…');
    }
    out
}

/// First sentence of a Markdown answer, as plain text capped at `limit`
/// characters. Prefers the first paragraph; falls back to a heading.
pub fn first_sentence(markdown: &str, limit: usize) -> Option<String> {
    let mut in_fence = false;
    let mut paragraph: Vec<String> = Vec::new();
    let mut heading: Option<String> = None;
    for raw in markdown.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            if !paragraph.is_empty() {
                break;
            }
            continue;
        }
        if in_fence {
            continue;
        }
        if line.is_empty() || line.starts_with('|') || line.chars().all(|c| "-*_=".contains(c)) {
            if !paragraph.is_empty() {
                break;
            }
            continue;
        }
        if let Some(text) = line.strip_prefix('#') {
            if !paragraph.is_empty() {
                break;
            }
            if heading.is_none() {
                heading = Some(text.trim_start_matches('#').trim().to_string());
            }
            continue;
        }
        paragraph.push(strip_block_marker(line).to_string());
    }
    let text = if paragraph.is_empty() {
        heading?
    } else {
        paragraph.join(" ")
    };
    let text = strip_inline_markdown(&text);
    let sentence = cut_sentence(&text);
    let sentence = sentence.trim();
    if sentence.is_empty() {
        return None;
    }
    let mut out: String = sentence.chars().take(limit).collect();
    if sentence.chars().count() > limit {
        out = out.trim_end().to_string();
        out.push('…');
    }
    Some(out)
}

/// `- item`, `> quote`, `1. step` → the text after the marker.
fn strip_block_marker(line: &str) -> &str {
    let line = line.trim_start_matches('>').trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return rest.trim_start();
        }
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = line[digits..]
            .strip_prefix(". ")
            .or_else(|| line[digits..].strip_prefix(") "))
    {
        return rest.trim_start();
    }
    line
}

/// Drop emphasis markers and backticks; `[text](url)` → `text`.
fn strip_inline_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '`' => {}
            '_' if chars.peek() == Some(&'_') => {
                chars.next();
            }
            '[' => {
                let mut label = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == ']' {
                        closed = true;
                        break;
                    }
                    label.push(next);
                }
                out.push_str(&label);
                if closed && chars.peek() == Some(&'(') {
                    for next in chars.by_ref() {
                        if next == ')' {
                            break;
                        }
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Text up to and including the first `.`, `!` or `?` followed by a space.
/// A period inside a word (`main.rs`, `v1.2`) or after an abbreviation
/// (`e.g.`) or an initial does not end it.
fn cut_sentence(text: &str) -> &str {
    let bytes = text.as_bytes();
    for (index, c) in text.char_indices() {
        if matches!(c, '.' | '!' | '?' | ':')
            && bytes.get(index + 1).is_some_and(|next| *next == b' ')
        {
            // "e.g. " and single initials are not sentence ends.
            let before = &text[..index];
            let word = before.rsplit(' ').next().unwrap_or_default();
            let abbreviation =
                !word.is_empty() && word.split('.').all(|part| part.chars().count() <= 1);
            if c == '.' && abbreviation {
                continue;
            }
            if c == ':' {
                return before;
            }
            return &text[..index + c.len_utf8()];
        }
    }
    text
}

// -- rail geometry --------------------------------------------------------

/// A vertical extent in content pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub top: f32,
    pub bottom: f32,
}

impl Span {
    pub fn new(top: f32, bottom: f32) -> Self {
        Self { top, bottom }
    }

    fn overlaps(self, other: Span) -> bool {
        self.bottom > other.top && self.top < other.bottom
    }
}

/// One dash on the rail, in rail pixels from the rail's top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dash {
    pub top: f32,
    /// The turn being read: drawn longer and brighter.
    pub current: bool,
    /// Part of the turn is on screen.
    pub in_view: bool,
}

/// Distance between consecutive dashes when the rail has room.
pub const DASH_PITCH: f32 = 8.;
/// Closest the dashes get on a crowded rail.
pub const MIN_DASH_PITCH: f32 = 3.;
/// Dash lengths: every turn, and the turn being read.
pub const DASH_LENGTH: f32 = 8.;
pub const CURRENT_DASH_LENGTH: f32 = 16.;
/// Dash thickness.
pub const DASH_THICKNESS: f32 = 2.;

/// Turn extents from where each turn starts and where the content ends.
pub fn turn_spans(starts: &[f32], content_end: f32) -> Vec<Span> {
    starts
        .iter()
        .enumerate()
        .map(|(index, &top)| {
            let bottom = starts.get(index + 1).copied().unwrap_or(content_end);
            Span::new(top, bottom.max(top))
        })
        .collect()
}

/// One dash per turn, evenly spaced whatever the turn's length, stacked
/// in the middle of a rail `rail_height` tall. The pitch shrinks (down to
/// [`MIN_DASH_PITCH`]) when the turns would not fit; past that the stack
/// is clipped at the rail's ends. `view` is the visible part of the
/// content and `current` the turn being read.
pub fn rail_dashes(
    spans: &[Span],
    view: Span,
    current: Option<usize>,
    rail_height: f32,
) -> Vec<Dash> {
    let count = spans.len();
    if count == 0 {
        return Vec::new();
    }
    let pitch = if count > 1 {
        (rail_height / (count - 1) as f32).clamp(MIN_DASH_PITCH, DASH_PITCH)
    } else {
        DASH_PITCH
    };
    let stack = pitch * (count - 1) as f32 + DASH_THICKNESS;
    let first = ((rail_height - stack) / 2.).max(0.);
    spans
        .iter()
        .enumerate()
        .map(|(index, span)| Dash {
            top: first + pitch * index as f32,
            current: current == Some(index),
            in_view: span.overlaps(view),
        })
        .collect()
}

/// The turn being read: the one under a reading line near the top of the
/// view, or the last turn once the view reaches the end of the content.
pub fn current_turn(spans: &[Span], view: Span, content_height: f32) -> Option<usize> {
    if spans.is_empty() {
        return None;
    }
    if view.bottom >= content_height - 1. {
        let last = spans.len() - 1;
        if spans[last].top < view.bottom {
            return Some(last);
        }
    }
    let reading = view.top + ((view.bottom - view.top) * 0.25).min(120.);
    Some(
        spans
            .iter()
            .rposition(|span| span.top <= reading)
            .unwrap_or(0),
    )
}

/// The turn ⌥+scroll moves to: the next turn starting below the top of
/// the view, or the previous one starting above it.
pub fn marker_target(spans: &[Span], view_top: f32, down: bool) -> Option<usize> {
    const SLACK: f32 = 4.;
    if down {
        spans.iter().position(|span| span.top > view_top + SLACK)
    } else {
        spans.iter().rposition(|span| span.top < view_top - SLACK)
    }
}

// -- hover card text ------------------------------------------------------

/// `claude-sonnet-5-5` → `Sonnet 5.5`; unknown ids come back unchanged.
pub fn model_label(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    let id = lower.strip_prefix("claude-").unwrap_or(&lower);
    let parts: Vec<&str> = id.split(['-', '.']).collect();
    let Some(family) = parts
        .iter()
        .find(|part| matches!(**part, "opus" | "sonnet" | "haiku" | "fable"))
    else {
        return model.to_string();
    };
    // Versions are short numbers; long ones are release dates.
    let version: Vec<&str> = parts
        .iter()
        .filter(|part| part.len() <= 2 && part.chars().all(|c| c.is_ascii_digit()))
        .copied()
        .collect();
    let mut name = String::new();
    let mut letters = family.chars();
    if let Some(first) = letters.next() {
        name.push(first.to_ascii_uppercase());
        name.extend(letters);
    }
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    name
}

/// "Oct 12, 2026, 14:05" in local time.
pub fn absolute_time(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local)
        .format("%b %-d, %Y, %H:%M")
        .to_string()
}

// -- bookmarks ------------------------------------------------------------

/// Bookmarked turns per thread, by the turn's prompt message id. Kept in
/// a small JSON file next to the daemon's data (desktop-only state).
pub struct Bookmarks {
    path: Option<PathBuf>,
    by_thread: BTreeMap<String, BTreeSet<String>>,
}

const BOOKMARKS_FILE: &str = "desktop-bookmarks.json";

impl Bookmarks {
    pub fn load() -> Self {
        let path = trama_protocol::default_app_dir().map(|dir| dir.join(BOOKMARKS_FILE));
        let by_thread = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| parse_bookmarks(&text))
            .unwrap_or_default();
        Self { path, by_thread }
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            by_thread: BTreeMap::new(),
        }
    }

    pub fn contains(&self, thread: &ThreadId, message: &MessageId) -> bool {
        self.by_thread
            .get(&thread.to_string())
            .is_some_and(|set| set.contains(&message.to_string()))
    }

    /// Flip a bookmark and save. Returns whether it is now set.
    pub fn toggle(&mut self, thread: &ThreadId, message: &MessageId) -> bool {
        let set = self.by_thread.entry(thread.to_string()).or_default();
        let key = message.to_string();
        let now_set = if set.remove(&key) {
            false
        } else {
            set.insert(key);
            true
        };
        if set.is_empty() {
            self.by_thread.remove(&thread.to_string());
        }
        self.save();
        now_set
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, bookmarks_json(&self.by_thread));
    }
}

/// `{"thread": ["message", …]}`; anything else reads as no bookmarks.
pub fn parse_bookmarks(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    serde_json::from_str(text).unwrap_or_default()
}

pub fn bookmarks_json(map: &BTreeMap<String, BTreeSet<String>>) -> String {
    serde_json::to_string_pretty(map).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str, id: Option<&str>) -> ChatEntry {
        ChatEntry::User {
            text: text.into(),
            message_id: id.map(MessageId::from),
        }
    }

    fn answer(text: &str) -> ChatEntry {
        ChatEntry::Assistant {
            markdown: text.into(),
            streaming: false,
            message_id: None,
        }
    }

    fn tool(name: &str) -> ChatEntry {
        ChatEntry::ToolCall {
            tool_use_id: format!("t-{name}"),
            name: name.into(),
            input_json: serde_json::json!({}),
            finished: true,
            is_error: false,
            expanded: false,
            output: None,
        }
    }

    #[test]
    fn turns_start_at_each_prompt_and_summarize_the_final_answer() {
        let entries = vec![
            answer("Picked up a review run. Nothing else."),
            user("Fix the login bug\nIt fails on Safari", Some("m1")),
            answer("Looking at it."),
            tool("Read"),
            answer("Fixed the cookie path in `auth.rs`. Tests pass."),
            user(
                "<review-comments>\n[1] x\n</review-comments>\n\nThanks",
                None,
            ),
        ];
        let turns = segment_turns(&entries);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].entries, 0..1);
        assert_eq!(turns[0].question, "");
        assert_eq!(turns[0].summary.as_deref(), Some("Picked up a review run."));
        assert_eq!(turns[1].entries, 1..5);
        assert_eq!(turns[1].message_id, Some(MessageId::from("m1")));
        assert_eq!(turns[1].question, "Fix the login bug\nIt fails on Safari");
        assert_eq!(
            turns[1].summary.as_deref(),
            Some("Fixed the cookie path in auth.rs.")
        );
        assert_eq!(turns[2].entries, 5..6);
        assert_eq!(turns[2].question, "Thanks");
        assert_eq!(turns[2].summary, None);
        assert!(segment_turns(&[]).is_empty());
    }

    #[test]
    fn summary_skips_code_and_headings_and_strips_markdown() {
        let md = "## Plan\n\n```rust\nfn x() {}\n```\n\nI updated **[the parser](src/diff.rs)** so\nit handles e.g. renames. Then more.";
        assert_eq!(
            first_sentence(md, 140).as_deref(),
            Some("I updated the parser so it handles e.g. renames.")
        );
        assert_eq!(
            first_sentence("# Only a title", 140).as_deref(),
            Some("Only a title")
        );
        assert_eq!(
            first_sentence("- Bumped v1.2 in main.rs and done", 140).as_deref(),
            Some("Bumped v1.2 in main.rs and done")
        );
        assert_eq!(
            first_sentence("Here is what changed: three files.", 140).as_deref(),
            Some("Here is what changed")
        );
        assert_eq!(first_sentence("```\ncode\n```", 140), None);
        let long = first_sentence(&"word ".repeat(60), 20).unwrap();
        assert!(long.ends_with('…'));
        assert!(long.chars().count() <= 21);
    }

    #[test]
    fn question_preview_keeps_the_first_lines() {
        assert_eq!(question_preview("a\n\nb\nc\nd", 3, 100), "a\nb\nc…");
        assert_eq!(question_preview("short", 3, 100), "short");
        assert_eq!(question_preview("abcdef", 3, 3), "abc…");
    }

    #[test]
    fn dashes_are_evenly_spaced_and_mark_the_current_turn() {
        let spans = turn_spans(&[0., 100., 400.], 1000.);
        assert_eq!(
            spans,
            vec![
                Span::new(0., 100.),
                Span::new(100., 400.),
                Span::new(400., 1000.)
            ]
        );
        // Same pitch whatever the turn lengths, centered on the rail.
        let dashes = rail_dashes(&spans, Span::new(350., 650.), Some(1), 200.);
        assert_eq!(dashes.len(), 3);
        let pitches: Vec<f32> = dashes.windows(2).map(|w| w[1].top - w[0].top).collect();
        assert_eq!(pitches, vec![DASH_PITCH, DASH_PITCH]);
        let stack = 2. * DASH_PITCH + DASH_THICKNESS;
        assert_eq!(dashes[0].top, (200. - stack) / 2.);
        assert_eq!(
            dashes.iter().map(|dash| dash.current).collect::<Vec<_>>(),
            vec![false, true, false]
        );
        assert_eq!(
            dashes.iter().map(|dash| dash.in_view).collect::<Vec<_>>(),
            vec![false, true, true]
        );
        // A crowded rail tightens the pitch, but never below the minimum.
        let many: Vec<Span> = (0..100)
            .map(|n| Span::new(n as f32 * 10., n as f32 * 10. + 10.))
            .collect();
        let crowded = rail_dashes(&many, Span::new(0., 10.), None, 300.);
        let pitch = crowded[1].top - crowded[0].top;
        assert!(pitch < DASH_PITCH && pitch >= MIN_DASH_PITCH);
        assert!(crowded.iter().all(|dash| !dash.current));
        let packed = rail_dashes(&many, Span::new(0., 10.), None, 50.);
        assert_eq!(packed[1].top - packed[0].top, MIN_DASH_PITCH);
        assert_eq!(packed[0].top, 0.);
        assert!(rail_dashes(&[], Span::new(0., 1.), None, 100.).is_empty());
    }

    #[test]
    fn current_turn_follows_the_reading_line_and_the_end() {
        let spans = turn_spans(&[0., 500., 1200., 1300.], 1400.);
        assert_eq!(current_turn(&spans, Span::new(0., 400.), 1400.), Some(0));
        assert_eq!(current_turn(&spans, Span::new(450., 850.), 1400.), Some(1));
        // At the bottom the short last turn is current.
        assert_eq!(
            current_turn(&spans, Span::new(1000., 1400.), 1400.),
            Some(3)
        );
        assert_eq!(current_turn(&[], Span::new(0., 10.), 10.), None);
    }

    #[test]
    fn option_scroll_moves_one_marker_at_a_time() {
        let spans = turn_spans(&[0., 500., 1200.], 2000.);
        assert_eq!(marker_target(&spans, 0., true), Some(1));
        assert_eq!(marker_target(&spans, 500., true), Some(2));
        assert_eq!(marker_target(&spans, 1200., true), None);
        assert_eq!(marker_target(&spans, 600., false), Some(1));
        assert_eq!(marker_target(&spans, 500., false), Some(0));
        assert_eq!(marker_target(&spans, 0., false), None);
    }

    #[test]
    fn model_ids_read_as_names() {
        assert_eq!(model_label("claude-sonnet-5-5"), "Sonnet 5.5");
        assert_eq!(model_label("claude-opus-4-1-20250805"), "Opus 4.1");
        assert_eq!(model_label("claude-3-5-haiku-20241022"), "Haiku 3.5");
        assert_eq!(model_label("gpt-5"), "gpt-5");
    }

    #[test]
    fn bookmarks_toggle_and_round_trip() {
        let mut marks = Bookmarks::in_memory();
        let thread = ThreadId::from("t1");
        let message = MessageId::from("m1");
        assert!(!marks.contains(&thread, &message));
        assert!(marks.toggle(&thread, &message));
        assert!(marks.contains(&thread, &message));
        let json = bookmarks_json(&marks.by_thread);
        assert_eq!(parse_bookmarks(&json), marks.by_thread);
        assert!(!marks.toggle(&thread, &message));
        assert!(marks.by_thread.is_empty());
        assert!(parse_bookmarks("not json").is_empty());
    }
}
