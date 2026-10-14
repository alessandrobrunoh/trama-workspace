//! Selectable text for the transcript blocks that are not stored messages:
//! activity lines, thinking, command and tool output, subagent rows and
//! diff text. Everything in the document reads as text and can be
//! selected and commented on; stored messages keep their own views (their
//! comments anchor to byte ranges of the message), these blocks share one
//! cache keyed by a stable block key.
//!
//! A comment on one of these blocks cannot anchor to it (the protocol only
//! anchors to messages, code and diff hunks), so it is stored on the
//! turn's answer with the selected passage quoted in its body
//! ([`crate::review::comments::proxy_body`]). Marks find that passage again in the
//! block's text to highlight it and to place the comment under the block.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use gpui_kit::component::text::{RangeHighlight, RenderedText, TextView, TextViewState};
use gpui_kit::*;

/// A passage to highlight in a block, and the comment thread it belongs
/// to (`None` for the passage of the comment being written).
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub group: Option<usize>,
    /// The block kind the comment was written on ([`crate::review::comments::proxy_body`]).
    pub kind: &'static str,
    pub excerpt: String,
    pub color: Hsla,
}

/// A block drawn in the last frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drawn {
    pub key: String,
    pub kind: &'static str,
    pub turn: usize,
}

/// A selection inside one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSelection {
    pub block: Drawn,
    /// The selected text as shown.
    pub text: String,
}

struct BlockText {
    markdown: String,
    /// The text a person reads, for finding quoted passages before the
    /// view has parsed.
    plain: String,
    state: Entity<TextViewState>,
    applied: Option<(RenderedText, Vec<(Range<usize>, Hsla)>)>,
}

#[derive(Default)]
pub struct BlockTexts {
    texts: HashMap<String, BlockText>,
    drawn: Vec<Drawn>,
    /// Comment threads placed this frame (each under one block).
    claimed: HashSet<usize>,
    /// Threads placed since the last [`Self::take_claimed`].
    fresh: Vec<usize>,
}

impl BlockTexts {
    pub fn clear(&mut self) {
        self.texts.clear();
        self.drawn.clear();
        self.claimed.clear();
        self.fresh.clear();
    }

    /// Start a frame: forget what was drawn and claimed.
    pub fn begin_frame(&mut self) {
        self.drawn.clear();
        self.claimed.clear();
        self.fresh.clear();
    }

    /// The selectable view of `markdown` (whose plain reading is `plain`)
    /// for block `key`. Marks of this `kind` whose passage the text holds
    /// are highlighted, and their threads claimed for this block.
    #[allow(clippy::too_many_arguments)]
    pub fn view<V: 'static>(
        &mut self,
        key: &str,
        markdown: &str,
        plain: &str,
        kind: &'static str,
        turn: usize,
        marks: &[Mark],
        cx: &mut Context<V>,
    ) -> AnyElement {
        let text = self
            .texts
            .entry(key.to_string())
            .or_insert_with(|| BlockText {
                markdown: markdown.to_string(),
                plain: plain.to_string(),
                state: cx.new(|cx| TextViewState::markdown(markdown, cx).selectable(true)),
                applied: None,
            });
        if text.markdown != markdown {
            text.markdown = markdown.to_string();
            text.state
                .update(cx, |state, cx| state.set_text(markdown, cx));
            text.applied = None;
        }
        text.plain = plain.to_string();
        self.drawn.push(Drawn {
            key: key.to_string(),
            kind,
            turn,
        });

        let rendered = text.state.read(cx).rendered_text();
        let mut ranges: Vec<(Range<usize>, Hsla)> = Vec::new();
        for mark in marks.iter().filter(|mark| mark.kind == kind) {
            let needle = mark.excerpt.trim();
            if needle.is_empty() {
                continue;
            }
            if let Some(group) = mark.group
                && self.claimed.contains(&group)
            {
                continue;
            }
            let found = find_passage(rendered.as_str(), needle);
            if found.is_none() && !text.plain.contains(needle) {
                continue;
            }
            if let Some(group) = mark.group {
                self.claimed.insert(group);
                self.fresh.push(group);
            }
            if let Some(range) = found
                && !ranges
                    .iter()
                    .any(|(seen, _)| seen.start < range.end && range.start < seen.end)
            {
                ranges.push((range, mark.color));
            }
        }
        ranges.sort_by_key(|(range, _)| range.start);
        let key = (rendered.clone(), ranges);
        if text.applied.as_ref() != Some(&key) {
            let highlights: Vec<RangeHighlight> = key
                .1
                .iter()
                .map(|(range, color)| RangeHighlight::new(range.clone(), *color))
                .collect();
            text.state.update(cx, |state, cx| {
                let _ = state.set_range_highlights(highlights, cx);
            });
            text.applied = Some(key);
        }
        TextView::new(&text.state)
            .selectable(true)
            .into_any_element()
    }

    /// Whether block `key` has selected text (a click on it then selects
    /// rather than toggles).
    pub fn has_selection(&self, key: &str, cx: &App) -> bool {
        self.texts
            .get(key)
            .and_then(|text| text.state.read(cx).selected_source_range())
            .is_some_and(|range| range.start < range.end)
    }

    /// Selections in the blocks drawn last frame.
    pub fn selections(&self, cx: &App) -> Vec<BlockSelection> {
        self.drawn
            .iter()
            .filter_map(|block| {
                let state = self.texts.get(&block.key)?.state.read(cx);
                let range = state.selected_source_range()?;
                if range.start >= range.end {
                    return None;
                }
                let text = state.selected_text();
                let text = text.trim();
                (!text.is_empty()).then(|| BlockSelection {
                    block: block.clone(),
                    text: text.to_string(),
                })
            })
            .collect()
    }

    pub fn clear_selection(&self, key: &str, cx: &mut App) {
        if let Some(text) = self.texts.get(key) {
            text.state.update(cx, |state, cx| state.clear_selection(cx));
        }
    }

    /// How many blocks were drawn so far this frame.
    pub fn drawn_len(&self) -> usize {
        self.drawn.len()
    }

    /// Whether block `key` was drawn after the first `from` blocks.
    pub fn drawn_since(&self, from: usize, key: &str) -> bool {
        self.drawn
            .get(from..)
            .is_some_and(|drawn| drawn.iter().any(|block| block.key == key))
    }

    /// Threads placed under blocks since the last call.
    pub fn take_claimed(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.fresh)
    }
}

/// The range of `needle` in `text`, also when the quoted passage's line
/// breaks or runs of spaces differ from the text's.
pub fn find_passage(text: &str, needle: &str) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    if let Some(start) = text.find(needle) {
        return Some(start..start + needle.len());
    }
    // Compare with whitespace runs collapsed, mapping back to `text`.
    let words: Vec<&str> = needle.split_whitespace().collect();
    let first = *words.first()?;
    let mut from = 0;
    while let Some(offset) = text[from..].find(first) {
        let start = from + offset;
        let mut at = start + first.len();
        let mut matched = true;
        for word in &words[1..] {
            let rest = &text[at..];
            let skipped = rest.len() - rest.trim_start().len();
            if skipped == 0 || !rest[skipped..].starts_with(word) {
                matched = false;
                break;
            }
            at += skipped + word.len();
        }
        if matched {
            return Some(start..at);
        }
        from = start + first.len();
    }
    None
}

/// Escape `text` so Markdown shows it as written (every ASCII punctuation
/// character may be backslash-escaped). Line breaks stay hard breaks.
pub fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            out.push_str("  \n");
        }
        for c in line.chars() {
            if c.is_ascii_punctuation() {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}

/// A fenced code block holding `text` verbatim, with a fence longer than
/// any backtick run inside it.
pub fn code_block(text: &str, info: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{info}\n{}\n{fence}", text.trim_end_matches('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn passages_are_found_across_line_breaks_and_spaces() {
        let text = "Before I write code,\nplease accept   ADR-5 now.";
        assert_eq!(find_passage(text, "please accept"), Some(21..34));
        let range = find_passage(text, "code, please accept ADR-5").unwrap();
        assert_eq!(&text[range], "code,\nplease accept   ADR-5");
        assert_eq!(find_passage(text, "missing words"), None);
        assert_eq!(find_passage(text, ""), None);
    }

    #[test]
    fn plain_text_survives_markdown() {
        assert_eq!(escape_markdown("a *b* [c](d)"), "a \\*b\\* \\[c\\]\\(d\\)");
        assert_eq!(escape_markdown("# one\ntwo"), "\\# one  \ntwo");
        assert_eq!(code_block("ls\nok", "text"), "```text\nls\nok\n```");
        assert_eq!(code_block("a ```` b", ""), "`````\na ```` b\n`````");
    }
}
