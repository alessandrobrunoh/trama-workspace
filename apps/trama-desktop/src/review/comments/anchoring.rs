use super::*;

// -- transcript anchoring -------------------------------------------------

/// Map a selection inside a rendered message to a range of
/// `Message::text()`. `source` is the Markdown the view renders, which
/// starts `base` bytes into the message text (after a collapsed comment
/// block). Whitespace at either end is dropped and the ends are snapped
/// outward to UTF-8 character boundaries. `None` for an empty selection.
pub fn selection_to_message_range(
    source: &str,
    base: usize,
    selected: Range<usize>,
) -> Option<Range<usize>> {
    let mut start = selected.start.min(source.len());
    let mut end = selected.end.min(source.len());
    if start >= end {
        return None;
    }
    while !source.is_char_boundary(start) {
        start -= 1;
    }
    while !source.is_char_boundary(end) {
        end += 1;
    }
    let slice = &source[start..end];
    let trimmed_start = slice.len() - slice.trim_start().len();
    let trimmed = slice.trim();
    if trimmed.is_empty() {
        return None;
    }
    let start = start + trimmed_start;
    Some(base + start..base + start + trimmed.len())
}

/// A comment thread highlighted inside one rendered message.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageAnchor {
    /// Index into the groups.
    pub group: usize,
    /// Range of the rendered source (message text minus `base`).
    pub range: Range<usize>,
}

/// Threads anchored to `message_id`, in group order, and the ranges of the
/// rendered `source` (which starts `base` bytes into the message text) they
/// highlight. A range that falls outside the rendered source, or off a
/// character boundary, is kept as a thread without a highlight.
pub fn message_anchors(
    groups: &[CommentGroup],
    message_id: &MessageId,
    base: usize,
    source: &str,
) -> (Vec<usize>, Vec<MessageAnchor>) {
    let mut threads = Vec::new();
    let mut anchors = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        let CommentTarget::MessageRange {
            message_id: target,
            start_byte,
            end_byte,
        } = &group.root.target
        else {
            continue;
        };
        // A comment on a block that is not a message is stored on the
        // turn's answer; it is placed under its own block.
        if target != message_id || parse_proxy(&group.root.body).is_some() {
            continue;
        }
        threads.push(index);
        let (Some(start), Some(end)) = (start_byte.checked_sub(base), end_byte.checked_sub(base))
        else {
            continue;
        };
        if start < end
            && end <= source.len()
            && source.is_char_boundary(start)
            && source.is_char_boundary(end)
        {
            anchors.push(MessageAnchor {
                group: index,
                range: start..end,
            });
        }
    }
    anchors.sort_by_key(|anchor| (anchor.range.start, anchor.range.end));
    (threads, anchors)
}

// -- comments on blocks that are not messages -----------------------------

/// Kinds of transcript blocks that are not stored messages. A comment on
/// one is stored on a message of its turn with the passage quoted in its
/// body, headed by the kind: `> Thinking: <passage>`.
pub const KIND_ACTIVITY: &str = "Activity";
pub const KIND_THINKING: &str = "Thinking";
pub const KIND_COMMAND: &str = "Command output";
pub const KIND_TOOL: &str = "Tool output";
pub const KIND_PATCH: &str = "Patch";
pub const KIND_SUBAGENT: &str = "Subagent";
pub const PROXY_KINDS: &[&str] = &[
    KIND_ACTIVITY,
    KIND_THINKING,
    KIND_COMMAND,
    KIND_TOOL,
    KIND_PATCH,
    KIND_SUBAGENT,
];

/// Longest passage quoted in a comment body, in characters.
pub(super) const PROXY_EXCERPT_LIMIT: usize = 600;

/// The body of a comment on a `kind` block: the quoted passage, a blank
/// line, then what the person wrote. The agent reads the quote with the
/// comment; the transcript parses it back with [`parse_proxy`].
pub fn proxy_body(kind: &str, excerpt: &str, comment: &str) -> String {
    let excerpt = excerpt.trim();
    let mut quoted: String = excerpt.chars().take(PROXY_EXCERPT_LIMIT).collect();
    if excerpt.chars().count() > PROXY_EXCERPT_LIMIT {
        quoted.push('…');
    }
    let mut out = format!("> {kind}: ");
    for (index, line) in quoted.lines().enumerate() {
        if index > 0 {
            out.push_str("\n> ");
        }
        out.push_str(line.trim_end());
    }
    out.push_str("\n\n");
    out.push_str(comment.trim());
    out
}

/// A comment on a block that is not a message, read back from its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    pub kind: &'static str,
    pub excerpt: String,
    pub comment: String,
}

pub fn parse_proxy(body: &str) -> Option<Proxy> {
    let (head, comment) = body.split_once("\n\n")?;
    let mut lines = head.lines();
    let first = lines.next()?.strip_prefix("> ")?;
    let kind = PROXY_KINDS.iter().copied().find(|kind| {
        first
            .strip_prefix(kind)
            .is_some_and(|rest| rest.starts_with(": "))
    })?;
    let mut excerpt = first[kind.len() + 2..].to_string();
    for line in lines {
        let line = line.strip_prefix("> ").or_else(|| line.strip_prefix('>'))?;
        excerpt.push('\n');
        excerpt.push_str(line);
    }
    Some(Proxy {
        kind,
        excerpt,
        comment: comment.trim().to_string(),
    })
}

/// What a comment says, without the quote a block comment carries.
pub fn display_body(body: &str) -> String {
    match parse_proxy(body) {
        Some(proxy) => proxy.comment,
        None => body.to_string(),
    }
}

/// The passage of `text` a block comment is stored on: its first line
/// with text, capped at a character boundary. `None` for empty text.
pub fn proxy_anchor(text: &str) -> Option<Range<usize>> {
    pub(super) const LIMIT: usize = 200;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let start = offset + (line.len() - line.trim_start().len());
            let mut end = start + trimmed.len().min(LIMIT);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            return (start < end).then_some(start..end);
        }
        offset += line.len();
    }
    None
}

/// The checkout-relative path of `path` (absolute, or relative to the
/// only checkout) and its repository. `checkouts` holds each repository's
/// checkout root.
pub fn repo_relative(
    checkouts: &[(String, std::path::PathBuf)],
    path: &str,
) -> Option<(String, String)> {
    let path = std::path::Path::new(path);
    if path.is_relative() {
        return match checkouts {
            [(repo, _)] => Some((repo.clone(), path.to_string_lossy().to_string())),
            _ => None,
        };
    }
    checkouts
        .iter()
        .filter_map(|(repo, root)| {
            let rest = path.strip_prefix(root).ok()?;
            // Repository-relative paths are Git-style (`/`) on every OS.
            let rest = rest.to_string_lossy().replace('\\', "/");
            (!rest.is_empty()).then(|| (root.as_os_str().len(), repo.clone(), rest))
        })
        // The deepest root wins (a checkout inside another).
        .max_by_key(|(depth, _, _)| *depth)
        .map(|(_, repo, rest)| (repo, rest))
}

/// The target for diff lines `lines` (indices into `hunk.lines`) picked in
/// an edit of `path` shown in the transcript: the new-side code lines when
/// the pick has any and the file is in a known checkout. `None` otherwise
/// (removed lines only, or a file outside the checkouts): such a comment
/// is stored as a block comment quoting the lines.
pub fn patch_target(
    checkouts: &[(String, std::path::PathBuf)],
    path: &str,
    hunk: &Hunk,
    lines: std::ops::RangeInclusive<usize>,
) -> Option<CommentTarget> {
    let (repo, relative) = repo_relative(checkouts, path)?;
    match diff_target(&repo, &relative, "", hunk, lines) {
        target @ CommentTarget::Code { .. } => Some(target),
        _ => None,
    }
}

/// Whether a code comment is about lines an edit of `path` wrote
/// (`new_lines`, line numbers of the new side).
pub fn code_on_edit(anchor: &CodeAnchor, path: &str, new_lines: &[u32]) -> bool {
    let anchored = anchor.path.to_string_lossy();
    let same_file = path == anchored || path.ends_with(&format!("/{anchored}"));
    same_file
        && new_lines
            .iter()
            .any(|line| (anchor.start_line..=anchor.end_line).contains(line))
}

// -- prompt block ---------------------------------------------------------

pub(super) const BLOCK_OPEN: &str = "<review-comments>\n";
pub(super) const BLOCK_CLOSE: &str = "\n</review-comments>";

/// The `<review-comments>` block at the start of a delivered user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptBlock {
    /// Byte range of the block, tags included.
    pub block: Range<usize>,
    /// Where the person's own text starts (after the blank line).
    pub body_start: usize,
    /// Commented passages (`[n]` entries) in the block.
    pub entries: usize,
}

/// Find the block by its first and last lines. The closing tag must stand
/// on its own line and be followed by a blank line or the end of the text,
/// so a tag quoted inside the user's own text is not taken for it.
pub fn split_prompt_block(text: &str) -> Option<PromptBlock> {
    if !text.starts_with(BLOCK_OPEN) {
        return None;
    }
    let mut from = BLOCK_OPEN.len() - 1;
    while let Some(found) = text[from..].find(BLOCK_CLOSE) {
        let close = from + found;
        let end = close + BLOCK_CLOSE.len();
        let rest = &text[end..];
        if rest.is_empty() || rest.starts_with("\n\n") {
            let body_start = if rest.is_empty() { end } else { end + 2 };
            let inner = &text[BLOCK_OPEN.len()..close];
            let mut entries = 0;
            for line in inner.lines() {
                if line.starts_with(&format!("[{}] ", entries + 1)) {
                    entries += 1;
                }
            }
            return Some(PromptBlock {
                block: 0..end,
                body_start,
                entries,
            });
        }
        from = close + 1;
    }
    None
}

/// The person's own text of a stored user message.
pub fn prompt_body(text: &str) -> &str {
    match split_prompt_block(text) {
        Some(block) => &text[block.body_start..],
        None => text,
    }
}
