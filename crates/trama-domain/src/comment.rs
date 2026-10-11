use crate::ids::{CommentId, MessageId, RepositoryId, ThreadId};
use crate::message::Actor;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A comment anchored to code, designed to survive edits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAnchor {
    pub repository_id: RepositoryId,
    pub path: PathBuf,
    /// `FileRevision` number the comment was created against.
    pub revision: u64,
    pub start_line: u32,
    pub end_line: u32,
    /// Hash of the anchored text — used for relocation.
    pub text_hash: Option<String>,
    /// Hash of surrounding context lines (fuzzy matching fallback).
    pub context_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorStatus {
    /// Anchor relocated to a new position.
    Resolved { line: u32 },
    /// Ambiguous relocation; needs human confirmation.
    Fuzzy,
    /// Target gone (file deleted / text rewritten). Stay visible but
    /// detached — never silently reattach to the wrong line.
    Orphaned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum CommentTarget {
    MessageRange {
        message_id: MessageId,
        start_byte: usize,
        end_byte: usize,
    },
    Code {
        anchor: CodeAnchor,
    },
    DiffHunk {
        repository_id: RepositoryId,
        path: PathBuf,
        base_revision: String,
        hunk_header: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: CommentId,
    pub thread_id: ThreadId,
    pub author: Actor,
    pub target: CommentTarget,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;
/// Lines of preceding (or, if none, following) context hashed for fuzzy match.
const CONTEXT_RADIUS: usize = 3;

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn hash_text(text: &str) -> String {
    format!("{:016x}", fnv1a_64(text.as_bytes()))
}

fn join_lines(lines: &[&str]) -> String {
    lines.join("\n")
}

fn split_lines(content: &str) -> Vec<&str> {
    content.lines().collect()
}

#[cfg(test)]
fn line_range_text(lines: &[&str], start_line: u32, end_line: u32) -> Option<String> {
    if start_line == 0 || end_line < start_line {
        return None;
    }
    let start_idx = (start_line - 1) as usize;
    let end_idx = (end_line - 1) as usize;
    lines.get(start_idx..=end_idx).map(join_lines)
}

#[cfg(test)]
fn context_lines<'a>(lines: &[&'a str], start_line: u32, end_line: u32) -> Vec<&'a str> {
    if start_line == 0 {
        return Vec::new();
    }
    let start_idx = (start_line - 1) as usize;
    let end_idx = end_line.saturating_sub(1) as usize;
    if start_idx > 0 && start_idx <= lines.len() {
        let from = start_idx.saturating_sub(CONTEXT_RADIUS);
        return lines[from..start_idx].to_vec();
    }
    if end_idx < lines.len() && end_idx + 1 < lines.len() {
        let to = (end_idx + 1 + CONTEXT_RADIUS).min(lines.len());
        return lines[end_idx + 1..to].to_vec();
    }
    Vec::new()
}

fn find_hash_window(lines: &[&str], expected: &str, min_len: usize, max_len: usize) -> Option<u32> {
    if lines.is_empty() || min_len == 0 {
        return None;
    }
    let max_len = max_len.min(lines.len());
    if max_len < min_len {
        return None;
    }
    for len in min_len..=max_len {
        for start in 0..=lines.len() - len {
            if hash_text(&join_lines(&lines[start..start + len])) == expected {
                return Some((start as u32) + 1);
            }
        }
    }
    None
}

fn find_text_range(
    lines: &[&str],
    expected: &str,
    range_len: usize,
    original_start: u32,
) -> Option<u32> {
    if range_len == 0 || range_len > lines.len() {
        return None;
    }
    let mut best: Option<(u32, u32)> = None;
    for start in 0..=lines.len() - range_len {
        if hash_text(&join_lines(&lines[start..start + range_len])) == expected {
            let line = (start as u32) + 1;
            let dist = line.abs_diff(original_start);
            match best {
                Some((_, best_dist)) if dist >= best_dist => {}
                _ => best = Some((line, dist)),
            }
            if dist == 0 {
                break;
            }
        }
    }
    best.map(|(line, _)| line)
}

/// Relocate a code comment after `new_content` replaced the file.
///
/// 1. If any window of the original line-range length still hashes to
///    `text_hash`, return [`AnchorStatus::Resolved`] at the closest match.
/// 2. Else if `context_hash` matches any nearby window, [`AnchorStatus::Fuzzy`].
/// 3. Else [`AnchorStatus::Orphaned`].
pub fn relocate_anchor(anchor: &CodeAnchor, new_content: &str) -> AnchorStatus {
    let lines = split_lines(new_content);
    let range_len = if anchor.start_line == 0 || anchor.end_line < anchor.start_line {
        0
    } else {
        (anchor.end_line - anchor.start_line) as usize + 1
    };

    if let Some(text_hash) = anchor.text_hash.as_deref() {
        if let Some(line) = find_text_range(&lines, text_hash, range_len, anchor.start_line) {
            return AnchorStatus::Resolved { line };
        }
    }

    if let Some(context_hash) = anchor.context_hash.as_deref() {
        if find_hash_window(&lines, context_hash, 1, CONTEXT_RADIUS).is_some() {
            return AnchorStatus::Fuzzy;
        }
    }

    AnchorStatus::Orphaned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RepositoryId;

    fn anchor_for(content: &str, start_line: u32, end_line: u32) -> CodeAnchor {
        let lines = split_lines(content);
        let text_hash = line_range_text(&lines, start_line, end_line).map(|text| hash_text(&text));
        let ctx = context_lines(&lines, start_line, end_line);
        let context_hash = if ctx.is_empty() {
            None
        } else {
            Some(hash_text(&join_lines(&ctx)))
        };
        CodeAnchor {
            repository_id: RepositoryId::generate(),
            path: PathBuf::from("src/lib.rs"),
            revision: 1,
            start_line,
            end_line,
            text_hash,
            context_hash,
        }
    }

    const ORIGINAL: &str = "\
header
alpha
bravo
TARGET ONE
TARGET TWO
charlie
delta
footer
";

    #[test]
    fn hash_is_stable_fnv1a() {
        assert_eq!(hash_text("hello"), hash_text("hello"));
        assert_ne!(hash_text("hello"), hash_text("world"));
        // FNV-1a 64 of "hello"
        assert_eq!(hash_text("hello"), "a430d84680aabd0b");
    }

    #[test]
    fn exact_match_unmoved() {
        let anchor = anchor_for(ORIGINAL, 4, 5);
        assert_eq!(
            relocate_anchor(&anchor, ORIGINAL),
            AnchorStatus::Resolved { line: 4 }
        );
    }

    #[test]
    fn exact_match_after_nearby_insertions() {
        let anchor = anchor_for(ORIGINAL, 4, 5);
        let inserted = "\
inserted A
inserted B
header
alpha
bravo
TARGET ONE
TARGET TWO
charlie
delta
footer
";
        assert_eq!(
            relocate_anchor(&anchor, inserted),
            AnchorStatus::Resolved { line: 6 }
        );
    }

    #[test]
    fn moved_block_resolves_at_new_line() {
        let anchor = anchor_for(ORIGINAL, 4, 5);
        let moved = "\
header
alpha
bravo
charlie
delta
footer
TARGET ONE
TARGET TWO
";
        assert_eq!(
            relocate_anchor(&anchor, moved),
            AnchorStatus::Resolved { line: 7 }
        );
    }

    #[test]
    fn prefers_closest_duplicate_match() {
        let original = "\
keep
pad
TARGET
keep
";
        let anchor = anchor_for(original, 3, 3);
        let duplicated = "\
TARGET
keep
pad
TARGET
keep
later
TARGET
";
        assert_eq!(
            relocate_anchor(&anchor, duplicated),
            AnchorStatus::Resolved { line: 4 }
        );
    }

    #[test]
    fn deleted_text_is_orphaned() {
        let anchor = anchor_for(ORIGINAL, 4, 5);
        let deleted = "\
unrelated
content
only
";
        assert_eq!(relocate_anchor(&anchor, deleted), AnchorStatus::Orphaned);
    }

    #[test]
    fn rewrite_with_preserved_context_is_fuzzy() {
        let anchor = anchor_for(ORIGINAL, 4, 5);
        let rewritten = "\
header
alpha
bravo
REWRITTEN ONE
REWRITTEN TWO
charlie
delta
footer
";
        assert_eq!(relocate_anchor(&anchor, rewritten), AnchorStatus::Fuzzy);
    }

    #[test]
    fn full_rewrite_is_orphaned() {
        let anchor = anchor_for(ORIGINAL, 4, 5);
        let rewritten = "\
completely
different
file
";
        assert_eq!(relocate_anchor(&anchor, rewritten), AnchorStatus::Orphaned);
    }

    #[test]
    fn missing_hashes_are_orphaned() {
        let mut anchor = anchor_for(ORIGINAL, 4, 5);
        anchor.text_hash = None;
        anchor.context_hash = None;
        assert_eq!(relocate_anchor(&anchor, ORIGINAL), AnchorStatus::Orphaned);
    }

    #[test]
    fn following_context_used_when_anchor_starts_file() {
        let original = "\
TARGET
alpha
bravo
charlie
";
        let anchor = anchor_for(original, 1, 1);
        assert!(anchor.context_hash.is_some());
        let rewritten = "\
CHANGED
alpha
bravo
charlie
";
        assert_eq!(relocate_anchor(&anchor, rewritten), AnchorStatus::Fuzzy);
    }

    #[test]
    fn empty_file_is_orphaned() {
        let anchor = anchor_for(ORIGINAL, 4, 5);
        assert_eq!(relocate_anchor(&anchor, ""), AnchorStatus::Orphaned);
    }
}
