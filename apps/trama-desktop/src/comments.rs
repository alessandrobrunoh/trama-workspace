//! Anchored review comments.
//!
//! [`CommentStore`] holds the selected thread's comments as the daemon
//! reports them (`Comments`, `CommentCreated`, `CommentUpdated`,
//! `CommentDeleted`) and sends the comment commands. The transcript and the
//! Changes drawer both read it. The pure helpers here group snapshots into
//! threads, place them on messages and diff lines, map text selections to
//! `Message::text()` byte ranges, and split the `<review-comments>` block the
//! daemon prepends to delivered prompts. The thread card and the inline
//! composer are shared by both views.

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Escape, InputEvent, Textarea, TextareaState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::{CommentId, MessageId, ThreadId};
use trama_protocol::{
    Actor, AnchorStatus, ClientMessage, CodeAnchor, CommentDelivery, CommentSnapshot, CommentState,
    CommentTarget, DaemonMessage,
};

use crate::diff::Hunk;
use crate::net::{DaemonClient, SendStatus};
use crate::theme::{self, Palette};

// -- store ----------------------------------------------------------------

/// The selected thread's comments, oldest first.
pub struct CommentStore {
    client: DaemonClient,
    thread: Option<ThreadId>,
    comments: Vec<CommentSnapshot>,
    /// Set when a command could not be sent; cleared by the next success.
    pub notice: Option<String>,
}

impl CommentStore {
    pub fn new(client: DaemonClient) -> Self {
        Self {
            client,
            thread: None,
            comments: Vec::new(),
            notice: None,
        }
    }

    /// Follow the selected thread. The daemon does not replay comments on
    /// subscribe; the net layer sends `ListComments` with every selection.
    pub fn set_thread(&mut self, thread: Option<ThreadId>, cx: &mut Context<Self>) {
        if self.thread != thread {
            self.thread = thread;
            self.comments.clear();
            self.notice = None;
            cx.notify();
        }
    }

    /// Apply a daemon message. Returns whether it was a comment message.
    pub fn apply(&mut self, message: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        let Some(thread) = self.thread.clone() else {
            return is_comment_message(message);
        };
        let handled = is_comment_message(message);
        if apply_comment_message(&mut self.comments, &thread, message) {
            cx.notify();
        }
        handled
    }

    /// Show these comments for `thread` without a daemon (the demo).
    pub fn load_demo(&mut self, thread: ThreadId, comments: Vec<CommentSnapshot>) {
        self.thread = Some(thread);
        self.comments = comments;
    }

    pub fn groups(&self) -> Vec<CommentGroup> {
        group_comments(&self.comments)
    }

    fn send(&mut self, message: ClientMessage, cx: &mut Context<Self>) -> SendStatus {
        let status = self.client.send(message);
        self.notice = match status {
            SendStatus::Enqueued => None,
            SendStatus::Rejected => {
                Some("The daemon is not connected; the comment change was not sent.".into())
            }
        };
        cx.notify();
        status
    }

    pub fn create(
        &mut self,
        target: CommentTarget,
        body: String,
        parent_comment_id: Option<CommentId>,
        cx: &mut Context<Self>,
    ) -> SendStatus {
        let Some(thread_id) = self.thread.clone() else {
            return SendStatus::Rejected;
        };
        self.send(
            ClientMessage::CreateComment {
                thread_id,
                target,
                body,
                parent_comment_id,
            },
            cx,
        )
    }

    pub fn resolve(&mut self, comment_id: CommentId, cx: &mut Context<Self>) {
        self.send(ClientMessage::ResolveComment { comment_id }, cx);
    }

    pub fn reopen(&mut self, comment_id: CommentId, cx: &mut Context<Self>) {
        self.send(ClientMessage::ReopenComment { comment_id }, cx);
    }

    pub fn delete(&mut self, comment_id: CommentId, cx: &mut Context<Self>) {
        self.send(ClientMessage::DeleteComment { comment_id }, cx);
    }

    /// Deliver the pending comments as a turn of their own.
    pub fn send_pending(&mut self, cx: &mut Context<Self>) -> SendStatus {
        let Some(thread_id) = self.thread.clone() else {
            return SendStatus::Rejected;
        };
        self.send(ClientMessage::SendComments { thread_id }, cx)
    }
}

fn is_comment_message(message: &DaemonMessage) -> bool {
    matches!(
        message,
        DaemonMessage::Comments { .. }
            | DaemonMessage::CommentCreated(_)
            | DaemonMessage::CommentUpdated(_)
            | DaemonMessage::CommentDeleted { .. }
    )
}

/// Fold one daemon message into `comments` (for `thread` only). Returns
/// whether anything changed.
pub fn apply_comment_message(
    comments: &mut Vec<CommentSnapshot>,
    thread: &ThreadId,
    message: &DaemonMessage,
) -> bool {
    match message {
        DaemonMessage::Comments {
            thread_id,
            comments: list,
        } if thread_id == thread => {
            *comments = list.clone();
            true
        }
        DaemonMessage::CommentCreated(snapshot) | DaemonMessage::CommentUpdated(snapshot)
            if &snapshot.thread_id == thread =>
        {
            match comments.iter_mut().find(|c| c.id == snapshot.id) {
                Some(existing) => *existing = snapshot.clone(),
                None => {
                    comments.push(snapshot.clone());
                    comments.sort_by_key(|comment| comment.created_at);
                }
            }
            true
        }
        DaemonMessage::CommentDeleted {
            thread_id,
            comment_ids,
        } if thread_id == thread => {
            let before = comments.len();
            comments.retain(|c| !comment_ids.contains(&c.id));
            comments.len() != before
        }
        _ => false,
    }
}

// -- grouping -------------------------------------------------------------

/// A root comment and its replies, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct CommentGroup {
    pub root: CommentSnapshot,
    pub replies: Vec<CommentSnapshot>,
}

/// Where a whole comment thread stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Something in it waits for the next prompt.
    Pending,
    /// Everything was sent; the latest delivery.
    Delivered(CommentDelivery),
    /// A person resolved the root; the latest delivery, if any.
    Resolved(Option<CommentDelivery>),
}

impl CommentGroup {
    pub fn id(&self) -> &CommentId {
        &self.root.id
    }

    pub fn all(&self) -> impl Iterator<Item = &CommentSnapshot> {
        std::iter::once(&self.root).chain(self.replies.iter())
    }

    pub fn len(&self) -> usize {
        1 + self.replies.len()
    }

    pub fn pending_count(&self) -> usize {
        if matches!(self.root.state, CommentState::Resolved { .. }) {
            return 0;
        }
        self.all()
            .filter(|c| c.state == CommentState::Pending)
            .count()
    }

    fn latest_delivery(&self) -> Option<CommentDelivery> {
        self.all()
            .filter_map(|c| match &c.state {
                CommentState::Delivered(delivery) => Some(delivery.clone()),
                CommentState::Resolved { delivery, .. } => delivery.clone(),
                CommentState::Pending => None,
            })
            .max_by_key(|delivery| delivery.turn)
    }

    pub fn phase(&self) -> Phase {
        if matches!(self.root.state, CommentState::Resolved { .. }) {
            return Phase::Resolved(self.latest_delivery());
        }
        if self.pending_count() > 0 {
            return Phase::Pending;
        }
        match self.latest_delivery() {
            Some(delivery) => Phase::Delivered(delivery),
            None => Phase::Pending,
        }
    }

    /// Open threads show as cards; delivered and resolved ones collapse
    /// into markers until expanded.
    pub fn open_by_default(&self) -> bool {
        self.phase() == Phase::Pending
    }
}

/// Group snapshots into threads: replies under their root (a reply to a
/// reply joins the same root), roots in creation order. A reply whose
/// parent is unknown stands alone rather than disappearing.
pub fn group_comments(comments: &[CommentSnapshot]) -> Vec<CommentGroup> {
    let root_of = |comment: &CommentSnapshot| -> Option<CommentId> {
        let mut current = comment;
        let mut hops = 0;
        while let Some(parent_id) = &current.parent_comment_id {
            current = comments.iter().find(|c| &c.id == parent_id)?;
            hops += 1;
            if hops > comments.len() {
                return None;
            }
        }
        Some(current.id.clone())
    };
    let mut groups: Vec<CommentGroup> = Vec::new();
    for comment in comments {
        let root = if comment.parent_comment_id.is_some() {
            root_of(comment).filter(|root| root != &comment.id)
        } else {
            None
        };
        match root.and_then(|root| groups.iter_mut().find(|g| g.root.id == root)) {
            Some(group) => group.replies.push(comment.clone()),
            None => groups.push(CommentGroup {
                root: comment.clone(),
                replies: Vec::new(),
            }),
        }
    }
    groups
}

/// Pending comments across all threads (roots and replies).
pub fn pending_total(groups: &[CommentGroup]) -> usize {
    groups.iter().map(CommentGroup::pending_count).sum()
}

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
const PROXY_EXCERPT_LIMIT: usize = 600;

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
    const LIMIT: usize = 200;
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
            let rest = rest.to_string_lossy().to_string();
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

const BLOCK_OPEN: &str = "<review-comments>\n";
const BLOCK_CLOSE: &str = "\n</review-comments>";

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

// -- type to comment ------------------------------------------------------

/// The text a key press types, when it should start a comment on the
/// current selection: printable characters without Command, Control or Fn.
pub fn typed_text(keystroke: &Keystroke) -> Option<String> {
    let modifiers = &keystroke.modifiers;
    if modifiers.platform || modifiers.control || modifiers.function {
        return None;
    }
    let text = match &keystroke.key_char {
        Some(text) => text.clone(),
        None if keystroke.key == "space" => " ".into(),
        None if keystroke.key.chars().count() == 1 => {
            if modifiers.shift {
                keystroke.key.to_uppercase()
            } else {
                keystroke.key.clone()
            }
        }
        None => return None,
    };
    (!text.is_empty() && !text.chars().any(char::is_control)).then_some(text)
}

/// Where the Markdown block holding `offset` ends: the start of the first
/// blank line after it, outside fenced code, or the end of `source`. The
/// transcript splits a message there to show a thread right under its
/// passage; offsets stay exact because the parts are contiguous slices.
pub fn block_end(source: &str, offset: usize) -> usize {
    let mut fence: Option<&str> = None;
    let mut line_start = 0;
    for line in source.split_inclusive('\n') {
        let line_end = line_start + line.len();
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim_start();
        let indent = content.len() - trimmed.len();
        let marker = ["```", "~~~"]
            .into_iter()
            .find(|marker| indent <= 3 && trimmed.starts_with(marker));
        match (fence, marker) {
            (None, Some(marker)) => fence = Some(marker),
            (Some(open), Some(marker)) if open == marker => fence = None,
            _ => {}
        }
        if fence.is_none() && content.trim().is_empty() && line_start >= offset && line_start > 0 {
            return line_start;
        }
        line_start = line_end;
    }
    source.len()
}

// -- diff placement -------------------------------------------------------

/// Where a code anchor sits in the current checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodePlacement {
    /// The anchored range now ends on `end_line`.
    Lines {
        start_line: u32,
        end_line: u32,
        /// Relocated to another line than it was created on.
        moved: bool,
        /// Ambiguous relocation: shown at the original lines.
        fuzzy: bool,
    },
    /// The text is gone. Never attached to a line.
    Orphaned,
}

pub fn code_placement(anchor: &CodeAnchor, status: Option<AnchorStatus>) -> CodePlacement {
    let span = anchor.end_line.saturating_sub(anchor.start_line);
    match status {
        Some(AnchorStatus::Orphaned) => CodePlacement::Orphaned,
        Some(AnchorStatus::Resolved { line }) => CodePlacement::Lines {
            start_line: line,
            end_line: line + span,
            moved: line != anchor.start_line,
            fuzzy: false,
        },
        Some(AnchorStatus::Fuzzy) => CodePlacement::Lines {
            start_line: anchor.start_line,
            end_line: anchor.end_line,
            moved: false,
            fuzzy: true,
        },
        None => CodePlacement::Lines {
            start_line: anchor.start_line,
            end_line: anchor.end_line,
            moved: false,
            fuzzy: false,
        },
    }
}

/// Why a thread about a file shows above its diff instead of on a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detached {
    /// Code anchor whose text is gone.
    Orphaned,
    /// Lines not shown in this diff (unchanged and outside the context).
    OutsideDiff,
    /// Hunk header no longer in the diff.
    Outdated,
}

/// Where a thread renders in one file's diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSlot {
    /// After line `line` of hunk `hunk`.
    Line {
        hunk: usize,
        line: usize,
    },
    /// After the last line of hunk `hunk`.
    Hunk(usize),
    Detached(Detached),
}

/// The slot of `group` in the diff of `path` in `repository_id`, or `None`
/// when the thread is not about this file.
pub fn diff_slot(
    group: &CommentGroup,
    repository_id: &str,
    path: &str,
    hunks: &[Hunk],
) -> Option<DiffSlot> {
    match &group.root.target {
        CommentTarget::Code { anchor } => {
            if anchor.repository_id.0.as_str() != repository_id || anchor.path.as_path() != path {
                return None;
            }
            match code_placement(anchor, group.root.anchor_status) {
                CodePlacement::Orphaned => Some(DiffSlot::Detached(Detached::Orphaned)),
                CodePlacement::Lines { end_line, .. } => {
                    for (hunk_ix, hunk) in hunks.iter().enumerate() {
                        if let Some(line_ix) = hunk
                            .lines
                            .iter()
                            .position(|line| line.new_line == Some(end_line))
                        {
                            return Some(DiffSlot::Line {
                                hunk: hunk_ix,
                                line: line_ix,
                            });
                        }
                    }
                    Some(DiffSlot::Detached(Detached::OutsideDiff))
                }
            }
        }
        CommentTarget::DiffHunk {
            repository_id: repo,
            path: target_path,
            hunk_header,
            ..
        } => {
            if repo.0 != repository_id || target_path.as_path() != path {
                return None;
            }
            let header = hunk_header.trim();
            match hunks
                .iter()
                .position(|hunk| header == hunk.header || header.starts_with(&hunk.header))
            {
                Some(index) => Some(DiffSlot::Hunk(index)),
                None => Some(DiffSlot::Detached(Detached::Outdated)),
            }
        }
        CommentTarget::MessageRange { .. } => None,
    }
}

/// The target for lines `lines` (indices into `hunk.lines`) picked in a
/// diff: code lines of the new side when the pick has any (the daemon
/// fills the anchor hashes), else the hunk (removed lines only).
pub fn diff_target(
    repository_id: &str,
    path: &str,
    base_revision: &str,
    hunk: &Hunk,
    lines: std::ops::RangeInclusive<usize>,
) -> CommentTarget {
    let new_lines: Vec<u32> = hunk
        .lines
        .get(lines)
        .unwrap_or_default()
        .iter()
        .filter_map(|line| line.new_line)
        .collect();
    match (new_lines.iter().min(), new_lines.iter().max()) {
        (Some(&start_line), Some(&end_line)) => CommentTarget::Code {
            anchor: CodeAnchor {
                repository_id: repository_id.into(),
                path: path.into(),
                revision: 0,
                start_line,
                end_line,
                text_hash: None,
                context_hash: None,
            },
        },
        _ => CommentTarget::DiffHunk {
            repository_id: repository_id.into(),
            path: path.into(),
            base_revision: base_revision.to_string(),
            hunk_header: hunk.header.clone(),
        },
    }
}

/// The file a code or diff comment is about.
pub fn target_file(target: &CommentTarget) -> Option<(String, String)> {
    match target {
        CommentTarget::Code { anchor } => Some((
            anchor.repository_id.to_string(),
            anchor.path.to_string_lossy().to_string(),
        )),
        CommentTarget::DiffHunk {
            repository_id,
            path,
            ..
        } => Some((
            repository_id.to_string(),
            path.to_string_lossy().to_string(),
        )),
        CommentTarget::MessageRange { .. } => None,
    }
}

// -- labels ---------------------------------------------------------------

pub fn author_label(actor: &Actor) -> &'static str {
    match actor {
        Actor::Human { .. } => "You",
        Actor::Agent { .. } => "Claude",
        Actor::System => "System",
    }
}

/// Only your own pending comments can be deleted.
pub fn can_delete(comment: &CommentSnapshot) -> bool {
    matches!(comment.author, Actor::Human { .. }) && comment.state == CommentState::Pending
}

/// "just now", "5m ago", "3h ago", "2d ago", "4w ago".
pub fn relative_time(then_secs: i64, now_secs: i64) -> String {
    let delta = (now_secs - then_secs).max(0);
    match delta {
        0..=44 => "just now".into(),
        45..=3_599 => format!("{}m ago", (delta / 60).max(1)),
        3_600..=86_399 => format!("{}h ago", delta / 3_600),
        86_400..=604_799 => format!("{}d ago", delta / 86_400),
        _ => format!("{}w ago", delta / 604_800),
    }
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

pub fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// One line of `text`, capped at `limit` characters.
pub fn one_line(text: &str, limit: usize) -> String {
    let line = text.trim().lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(limit).collect();
    if line.chars().count() > limit || text.trim().lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

/// A status badge next to a thread whose anchor is not where it was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorBadge {
    Moved(u32),
    Fuzzy,
    Detached(Detached),
}

impl AnchorBadge {
    pub fn for_group(group: &CommentGroup) -> Option<Self> {
        let CommentTarget::Code { anchor } = &group.root.target else {
            return None;
        };
        match code_placement(anchor, group.root.anchor_status) {
            CodePlacement::Orphaned => Some(Self::Detached(Detached::Orphaned)),
            CodePlacement::Lines {
                moved: true,
                start_line,
                ..
            } => Some(Self::Moved(start_line)),
            CodePlacement::Lines { fuzzy: true, .. } => Some(Self::Fuzzy),
            CodePlacement::Lines { .. } => None,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Moved(line) => format!("Moved to line {line}"),
            Self::Fuzzy => "May have moved".into(),
            Self::Detached(Detached::Orphaned) => "Detached: code changed".into(),
            Self::Detached(Detached::OutsideDiff) => "Outside the shown diff".into(),
            Self::Detached(Detached::Outdated) => "Outdated hunk".into(),
        }
    }

    fn color(self, p: &Palette) -> Hsla {
        match self {
            Self::Moved(_) => p.accent_text,
            Self::Fuzzy => p.warning,
            Self::Detached(Detached::Orphaned) => p.danger,
            Self::Detached(_) => p.text_muted,
        }
    }
}

/// Where a thread's anchor is, for chips and detached rows.
pub fn target_label(target: &CommentTarget) -> String {
    match target {
        CommentTarget::MessageRange { .. } => "Transcript".into(),
        CommentTarget::Code { anchor } => {
            let path = anchor.path.to_string_lossy();
            if anchor.start_line == anchor.end_line {
                format!("{path}:{}", anchor.start_line)
            } else {
                format!("{path}:{}-{}", anchor.start_line, anchor.end_line)
            }
        }
        CommentTarget::DiffHunk {
            path, hunk_header, ..
        } => format!("{} {}", path.to_string_lossy(), hunk_header.trim()),
    }
}

// -- inline composer ------------------------------------------------------

/// An open comment or reply composer.
pub struct CommentDraft {
    pub target: CommentTarget,
    pub parent: Option<CommentId>,
    pub input: Entity<TextareaState>,
    /// A comment on a block that is not a message: its kind and passage,
    /// quoted in the body on submit ([`proxy_body`]).
    quote: Option<(&'static str, String)>,
    _enter: Subscription,
}

impl CommentDraft {
    /// Create the input, focus it, and call `on_submit` on Enter. `initial`
    /// is text already typed (type-to-comment starts with the first key).
    pub fn new<V: 'static>(
        target: CommentTarget,
        parent: Option<CommentId>,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<V>,
        mut on_submit: impl FnMut(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let placeholder = if parent.is_some() {
            "Reply…"
        } else {
            "Comment on this passage…"
        };
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(placeholder)
                .rows(2)
                .submit_on_enter(true)
        });
        let enter = cx.subscribe_in(&input, window, move |this, _, event, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                on_submit(this, window, cx);
            }
        });
        if let Some(initial) = initial.filter(|text| !text.is_empty()) {
            input.update(cx, |input, cx| input.insert(initial, window, cx));
        }
        input.focus_handle(cx).focus(window, cx);
        Self {
            target,
            parent,
            input,
            quote: None,
            _enter: enter,
        }
    }

    /// Quote `excerpt` of a `kind` block in the body on submit.
    pub fn with_quote(mut self, kind: &'static str, excerpt: String) -> Self {
        self.quote = Some((kind, excerpt));
        self
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.focus_handle(cx).is_focused(window)
    }

    pub fn body(&self, cx: &App) -> Option<String> {
        let value = self.input.read(cx).value().to_string();
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// Send it through `store`. Returns whether it left the client.
    pub fn submit(&self, store: &Entity<CommentStore>, cx: &mut App) -> bool {
        let Some(body) = self.body(cx) else {
            return false;
        };
        let body = match &self.quote {
            Some((kind, excerpt)) => proxy_body(kind, excerpt, &body),
            None => body,
        };
        let target = self.target.clone();
        let parent = self.parent.clone();
        store.update(cx, |store, cx| store.create(target, body, parent, cx)) == SendStatus::Enqueued
    }
}

/// Cancels an open composer (its button and Escape).
pub type CancelDraft = Rc<dyn Fn(&mut Window, &mut App)>;

pub fn render_draft(
    id: impl Into<SharedString>,
    draft: &CommentDraft,
    on_submit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_cancel: CancelDraft,
    p: &Palette,
    cx: &App,
) -> AnyElement {
    let id: SharedString = id.into();
    let empty = draft.body(cx).is_none();
    let escape = on_cancel.clone();
    v_flex()
        .id(id.clone())
        .capture_action(move |_: &Escape, window, cx| escape(window, cx))
        .w_full()
        .gap(theme::sp(1.5))
        .px(theme::sp(2.5))
        .py(theme::sp(2.))
        .rounded(theme::radius())
        .border_1()
        .border_color(p.accent.opacity(0.45))
        .bg(p.elevated)
        .child(
            div()
                .w_full()
                .text_size(theme::text_ui())
                .child(Textarea::new(&draft.input).appearance(false).h(px(44.))),
        )
        .child(
            h_flex()
                .w_full()
                .gap(theme::sp(1.5))
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_size(theme::text_xs())
                        .text_color(p.text_faint)
                        .child("Enter to comment · Shift+Enter new line · Esc to cancel"),
                )
                .child(
                    Button::new(SharedString::from(format!("{id}-cancel")))
                        .ghost()
                        .xsmall()
                        .label("Cancel")
                        .on_click(move |_, window, cx| on_cancel(window, cx)),
                )
                .child(
                    Button::new(SharedString::from(format!("{id}-submit")))
                        .primary()
                        .xsmall()
                        .label(if draft.parent.is_some() {
                            "Reply"
                        } else {
                            "Comment"
                        })
                        .disabled(empty)
                        .on_click(on_submit),
                ),
        )
        .into_any_element()
}

// -- thread card ----------------------------------------------------------

type Callback<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;

/// What a thread card can do; each view supplies its own reply composer and
/// answer navigation.
#[derive(Clone)]
pub struct CardHandlers {
    pub store: Entity<CommentStore>,
    pub on_toggle: Callback<CommentId>,
    pub on_reply: Callback<CommentId>,
    /// Scroll the transcript to the message that answered the comment.
    pub on_answer: Callback<MessageId>,
}

/// The transcript's author avatar, at comment size.
fn avatar(actor: &Actor, p: &Palette) -> impl IntoElement {
    let (icon, bg, fg) = match actor {
        Actor::Human { .. } => (IconName::User, p.selected, p.text_muted),
        _ => (IconName::Bot, p.accent_soft, p.accent_text),
    };
    div()
        .size(px(16.))
        .flex_shrink_0()
        .rounded_full()
        .bg(bg)
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon).with_size(px(10.)).text_color(fg))
}

fn quote(text: &str, p: &Palette) -> impl IntoElement {
    div()
        .w_full()
        .pl(theme::sp(2.))
        .border_l_2()
        .border_color(p.accent.opacity(0.5))
        .text_size(theme::text_sm())
        .line_height(relative(1.45))
        .text_color(p.text_muted)
        .line_clamp(3)
        .child(text.trim().to_string())
}

fn badge(badge: AnchorBadge, p: &Palette) -> impl IntoElement {
    let color = badge.color(p);
    div()
        .flex_shrink_0()
        .px(theme::sp(1.5))
        .py(px(1.))
        .rounded(theme::radius_sm())
        .bg(color.opacity(0.12))
        .text_size(theme::text_xs())
        .text_color(color)
        .child(badge.label())
}

fn phase_color(phase: &Phase, p: &Palette) -> Hsla {
    match phase {
        Phase::Pending => p.accent,
        Phase::Delivered(_) => p.success,
        Phase::Resolved(_) => p.text_faint,
    }
}

fn phase_summary(group: &CommentGroup) -> String {
    match group.phase() {
        Phase::Pending => "Sent with your next message".into(),
        Phase::Delivered(delivery) if delivery.answer_message_id.is_some() => {
            format!("Answered in turn {}", delivery.turn)
        }
        Phase::Delivered(delivery) => {
            format!("Sent in turn {} · waiting for the answer", delivery.turn)
        }
        Phase::Resolved(_) => "Resolved".into(),
    }
}

/// A collapsed thread: one row that expands on click.
pub fn render_collapsed(
    id: impl Into<SharedString>,
    group: &CommentGroup,
    badge_for: Option<AnchorBadge>,
    handlers: &CardHandlers,
    p: &Palette,
) -> AnyElement {
    let phase = group.phase();
    let color = phase_color(&phase, p);
    let comment_id = group.id().clone();
    let on_toggle = handlers.on_toggle.clone();
    h_flex()
        .id(id.into())
        .w_full()
        .h(px(24.))
        .px(theme::sp(2.))
        .gap(theme::sp(1.5))
        .items_center()
        .rounded(theme::radius())
        .cursor_pointer()
        .text_size(theme::text_xs())
        .text_color(p.text_muted)
        .hover(|style| style.bg(p.hover))
        .on_click(move |_, window, cx| on_toggle(comment_id.clone(), window, cx))
        .child(
            Icon::new(IconName::MessageSquare)
                .with_size(px(12.))
                .text_color(color),
        )
        .child(div().flex_shrink_0().text_color(p.text).child(plural(
            group.len(),
            "comment",
            "comments",
        )))
        .child(
            div()
                .flex_shrink_0()
                .text_color(p.text_faint)
                .child(phase_summary(group)),
        )
        .children(badge_for.map(|b| badge(b, p)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(p.text_faint)
                .child(one_line(&group.root.body, 80)),
        )
        .into_any_element()
}

/// An expanded thread: quote, comments, status and actions.
#[allow(clippy::too_many_arguments)]
pub fn render_card(
    id: impl Into<SharedString>,
    group: &CommentGroup,
    show_excerpt: bool,
    badge_for: Option<AnchorBadge>,
    reply: Option<AnyElement>,
    handlers: &CardHandlers,
    now: i64,
    p: &Palette,
) -> AnyElement {
    let id: SharedString = id.into();
    let phase = group.phase();
    let color = phase_color(&phase, p);
    let root_id = group.id().clone();

    let comment = |comment: &CommentSnapshot| {
        let delete = can_delete(comment).then(|| {
            let store = handlers.store.clone();
            let comment_id = comment.id.clone();
            Button::new(SharedString::from(format!("{id}-delete-{}", comment.id)))
                .ghost()
                .xsmall()
                .icon(IconName::Delete)
                .tooltip("Delete this pending comment")
                .on_click(move |_, _, cx| {
                    store.update(cx, |store, cx| store.delete(comment_id.clone(), cx));
                })
        });
        v_flex()
            .w_full()
            .gap(theme::sp(0.5))
            .child(
                h_flex()
                    .w_full()
                    .gap(theme::sp(1.5))
                    .items_center()
                    .text_size(theme::text_xs())
                    .child(avatar(&comment.author, p))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text)
                            .child(author_label(&comment.author)),
                    )
                    .child(
                        div()
                            .text_color(p.text_faint)
                            .child(relative_time(comment.created_at.timestamp(), now)),
                    )
                    .when(comment.state == CommentState::Pending, |this| {
                        this.child(
                            div()
                                .px(theme::sp(1.5))
                                .rounded(theme::radius_sm())
                                .bg(p.accent_soft)
                                .text_color(p.accent_text)
                                .child("Pending"),
                        )
                    })
                    .child(div().flex_1())
                    .children(delete),
            )
            .child(
                div()
                    .w_full()
                    .text_size(theme::text_ui())
                    .line_height(relative(1.5))
                    .text_color(p.text)
                    .child(comment.body.clone()),
            )
    };

    let status: AnyElement = match &phase {
        Phase::Delivered(delivery) | Phase::Resolved(Some(delivery))
            if delivery.answer_message_id.is_some() =>
        {
            let answer = delivery
                .answer_message_id
                .clone()
                .unwrap_or_else(|| delivery.message_id.clone());
            let on_answer = handlers.on_answer.clone();
            let resolved = matches!(phase, Phase::Resolved(_));
            h_flex()
                .id(SharedString::from(format!("{id}-answer")))
                .gap(theme::sp(1.))
                .items_center()
                .cursor_pointer()
                .text_color(p.accent_text)
                .hover(|style| style.text_color(p.accent))
                .on_click(move |_, window, cx| on_answer(answer.clone(), window, cx))
                .child(if resolved {
                    format!("Resolved · answered in turn {}", delivery.turn)
                } else {
                    format!("Answered in turn {}", delivery.turn)
                })
                .child(Icon::new(IconName::ArrowRight).with_size(px(11.)))
                .into_any_element()
        }
        _ => div()
            .text_color(p.text_faint)
            .child(phase_summary(group))
            .into_any_element(),
    };

    let resolved = matches!(phase, Phase::Resolved(_));
    let store = handlers.store.clone();
    let toggle_resolved = {
        let root_id = root_id.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let root_id = root_id.clone();
            store.update(cx, |store, cx| {
                if resolved {
                    store.reopen(root_id, cx)
                } else {
                    store.resolve(root_id, cx)
                }
            });
        }
    };
    let on_reply = handlers.on_reply.clone();
    let on_toggle = handlers.on_toggle.clone();
    let reply_id = root_id.clone();
    let toggle_id = root_id.clone();

    v_flex()
        .w_full()
        .px(theme::sp(3.))
        .py(theme::sp(2.5))
        .gap(theme::sp(2.))
        .rounded(theme::radius())
        .border_1()
        .border_color(match phase {
            Phase::Pending => p.accent.opacity(0.4),
            _ => p.border,
        })
        .bg(p.elevated)
        .child(
            h_flex()
                .id(SharedString::from(format!("{id}-head")))
                .w_full()
                .gap(theme::sp(1.5))
                .items_center()
                .cursor_pointer()
                .text_size(theme::text_xs())
                .on_click(move |_, window, cx| on_toggle(toggle_id.clone(), window, cx))
                .child(div().size(px(6.)).rounded_full().bg(color))
                .child(div().text_color(p.text_muted).child(plural(
                    group.len(),
                    "comment",
                    "comments",
                )))
                .children(badge_for.map(|b| badge(b, p)))
                .child(div().flex_1())
                .child(
                    Icon::new(IconName::ChevronDown)
                        .with_size(px(12.))
                        .text_color(p.text_faint),
                ),
        )
        .when_some(
            group.root.excerpt.clone().filter(|_| show_excerpt),
            |this, excerpt| this.child(quote(&excerpt, p)),
        )
        .children(group.all().map(comment))
        .children(reply)
        .child(
            h_flex()
                .w_full()
                .gap(theme::sp(1.))
                .items_center()
                .text_size(theme::text_xs())
                .child(status)
                .child(div().flex_1())
                .when(!resolved, |this| {
                    this.child(
                        Button::new(SharedString::from(format!("{id}-reply")))
                            .ghost()
                            .xsmall()
                            .label("Reply")
                            .on_click(move |_, window, cx| on_reply(reply_id.clone(), window, cx)),
                    )
                })
                .child(
                    Button::new(SharedString::from(format!("{id}-resolve")))
                        .ghost()
                        .xsmall()
                        .icon(if resolved {
                            IconName::Undo2
                        } else {
                            IconName::Check
                        })
                        .label(if resolved { "Reopen" } else { "Resolve" })
                        .on_click(toggle_resolved),
                ),
        )
        .into_any_element()
}

/// The color of comment marks: the left bar of a comment, the highlight
/// behind its passage.
pub fn mark_color(p: &Palette) -> Hsla {
    p.danger
}

/// Highlight behind a passage commented in the document: strong while the
/// comment waits, faint once delivered, none when resolved.
pub fn passage_color(phase: &Phase, p: &Palette) -> Option<Hsla> {
    match phase {
        Phase::Pending => Some(mark_color(p).opacity(0.22)),
        Phase::Delivered(_) => Some(mark_color(p).opacity(0.12)),
        Phase::Resolved(_) => None,
    }
}

/// A comment author's round avatar: the person's initials (a collaborator
/// in a shared thread), a person glyph for you, the agent's icon.
pub fn author_avatar(actor: &Actor, me: Option<&str>, size: f32, p: &Palette) -> AnyElement {
    match crate::collab::Speaker::of(actor) {
        crate::collab::Speaker::Remote(name) => {
            crate::presence::avatar(p, &name, size, false).into_any_element()
        }
        crate::collab::Speaker::Me if me.is_some() => {
            crate::presence::avatar(p, me.unwrap_or_default(), size, false).into_any_element()
        }
        speaker => {
            let (icon, bg, fg) = match speaker {
                crate::collab::Speaker::Me => (IconName::User, p.selected, p.text_muted),
                _ => (IconName::Bot, p.accent_soft, p.accent_text),
            };
            div()
                .size(px(size))
                .flex_shrink_0()
                .rounded_full()
                .bg(bg)
                .flex()
                .items_center()
                .justify_center()
                .child(Icon::new(icon).with_size(px(size * 0.6)).text_color(fg))
                .into_any_element()
        }
    }
}

/// A comment thread right under its passage, as in a document: a card
/// with a bar on its left, each comment as an avatar and its text, the
/// agent's answer as a "↩ answer" line, and the reply composer. Reply,
/// resolve and delete show on hover.
#[allow(clippy::too_many_arguments)]
pub fn render_inline(
    id: impl Into<SharedString>,
    group: &CommentGroup,
    answer: Option<(MessageId, String)>,
    reply: Option<AnyElement>,
    me: Option<&str>,
    handlers: &CardHandlers,
    p: &Palette,
) -> AnyElement {
    let id: SharedString = id.into();
    let phase = group.phase();
    let resolved = matches!(phase, Phase::Resolved(_));
    let bar = match phase {
        Phase::Pending => mark_color(p),
        Phase::Delivered(_) => mark_color(p).opacity(0.55),
        Phase::Resolved(_) => p.border_strong,
    };
    let hover_group: SharedString = format!("{id}-card").into();
    let action = |name: &str, icon: IconName, tip: &'static str| {
        Button::new(SharedString::from(format!("{id}-{name}")))
            .ghost()
            .xsmall()
            .icon(icon)
            .tooltip(tip)
    };
    let actions = {
        let store = handlers.store.clone();
        let root_id = group.id().clone();
        let on_reply = handlers.on_reply.clone();
        let reply_id = root_id.clone();
        h_flex()
            .flex_shrink_0()
            .gap(theme::sp(0.5))
            .opacity(0.)
            .group_hover(hover_group.clone(), |style| style.opacity(1.))
            .when(!resolved, |this| {
                this.child(
                    action("reply", IconName::Undo2, "Reply")
                        .on_click(move |_, window, cx| on_reply(reply_id.clone(), window, cx)),
                )
            })
            .child(
                action(
                    "resolve",
                    if resolved {
                        IconName::Undo2
                    } else {
                        IconName::Check
                    },
                    if resolved { "Reopen" } else { "Resolve" },
                )
                .on_click(move |_, _, cx| {
                    let root_id = root_id.clone();
                    store.update(cx, |store, cx| {
                        if resolved {
                            store.reopen(root_id, cx)
                        } else {
                            store.resolve(root_id, cx)
                        }
                    });
                }),
            )
    };
    let comment = |index: usize, comment: &CommentSnapshot| {
        let delete = can_delete(comment).then(|| {
            let store = handlers.store.clone();
            let comment_id = comment.id.clone();
            action(
                &format!("delete-{index}"),
                IconName::Delete,
                "Delete this pending comment",
            )
            .on_click(move |_, _, cx| {
                store.update(cx, |store, cx| store.delete(comment_id.clone(), cx));
            })
        });
        let state = match &comment.state {
            CommentState::Pending => Some(("Pending", p.text_muted)),
            _ => None,
        };
        h_flex()
            .w_full()
            .gap(theme::sp(2.))
            .items_start()
            .child(
                div()
                    .pt(px(1.))
                    .child(author_avatar(&comment.author, me, 18., p)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme::text_ui())
                    .line_height(relative(1.5))
                    .text_color(if resolved { p.text_muted } else { p.text })
                    .child(display_body(&comment.body)),
            )
            .children(state.map(|(label, color)| {
                div()
                    .flex_shrink_0()
                    .pt(px(2.))
                    .text_size(theme::text_xs())
                    .text_color(color)
                    .child(label)
            }))
            .children(delete)
    };
    let answer = answer.map(|(message_id, excerpt)| {
        let on_answer = handlers.on_answer.clone();
        h_flex()
            .id(SharedString::from(format!("{id}-answer")))
            .w_full()
            .gap(theme::sp(1.5))
            .items_center()
            .cursor_pointer()
            .text_size(theme::text_sm())
            .text_color(p.text_muted)
            .hover(|style| style.text_color(p.text))
            .on_click(move |_, window, cx| on_answer(message_id.clone(), window, cx))
            .child(
                Icon::new(IconName::Undo2)
                    .with_size(px(12.))
                    .text_color(p.text_faint),
            )
            .child(div().flex_1().min_w_0().truncate().child(excerpt))
    });
    let status = match &phase {
        Phase::Delivered(delivery) if delivery.answer_message_id.is_none() => Some(format!(
            "Sent in turn {} · waiting for the answer",
            delivery.turn
        )),
        Phase::Resolved(_) => Some("Resolved".to_string()),
        _ => None,
    };
    h_flex()
        .id(id.clone())
        .group(hover_group)
        .w_full()
        .items_stretch()
        .rounded(theme::radius())
        .border_1()
        .border_color(p.border)
        .bg(p.elevated)
        .overflow_hidden()
        .child(div().w(px(2.)).flex_shrink_0().bg(bar))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .px(theme::sp(2.5))
                .py(theme::sp(2.))
                .gap(theme::sp(1.5))
                .child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap(theme::sp(1.))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(theme::sp(1.5))
                                .children(group.all().enumerate().map(|(n, c)| comment(n, c))),
                        )
                        .child(actions),
                )
                .children(status.map(|status| {
                    div()
                        .text_size(theme::text_xs())
                        .text_color(p.text_faint)
                        .child(status)
                }))
                .children(answer)
                .children(reply),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    // `super::*` brings GPUI's `test` attribute; use the standard one.
    use crate::diff::parse_unified_diff;
    use core::prelude::v1::test;
    use serde_json::{Value, json};

    const TIME: &str = "2026-10-12T10:00:00Z";

    /// Snapshots come from JSON so the tests read like the wire format.
    fn snap(id: &str, parent: Option<&str>, target: Value, state: Value) -> CommentSnapshot {
        serde_json::from_value(json!({
            "id": id, "thread_id": "t", "parent_comment_id": parent,
            "author": {"kind": "human", "id": "user"},
            "target": target, "body": format!("body {id}"), "excerpt": null,
            "created_at": TIME, "state": state, "anchor_status": null,
        }))
        .unwrap()
    }

    fn on_message(id: &str, start: usize, end: usize) -> Value {
        json!({"target": "message_range", "message_id": id, "start_byte": start, "end_byte": end})
    }

    fn on_code(path: &str, start: u32, end: u32) -> Value {
        json!({"target": "code", "anchor": {"repository_id": "r", "path": path,
            "revision": 0, "start_line": start, "end_line": end,
            "text_hash": null, "context_hash": null}})
    }

    fn on_hunk(path: &str, header: &str) -> Value {
        json!({"target": "diff_hunk", "repository_id": "r", "path": path,
            "base_revision": "abc", "hunk_header": header})
    }

    fn pending() -> Value {
        json!({"kind": "pending"})
    }

    fn delivered(turn: u32, answer: Option<&str>) -> Value {
        json!({"kind": "delivered", "message_id": "u1", "turn": turn,
            "delivered_at": TIME, "answer_message_id": answer})
    }

    fn resolved() -> Value {
        json!({"kind": "resolved", "resolved_at": TIME, "delivery": null})
    }

    #[test]
    fn selection_maps_to_message_bytes_with_base_and_trimming() {
        let source = "  Hello world  ";
        assert_eq!(selection_to_message_range(source, 0, 0..15), Some(2..13));
        assert_eq!(
            selection_to_message_range(source, 100, 2..7),
            Some(102..107),
            "offset by the hidden comment block"
        );
        assert_eq!(selection_to_message_range(source, 0, 0..2), None);
        assert_eq!(selection_to_message_range(source, 0, 5..5), None);
        assert_eq!(selection_to_message_range(source, 0, 9..3), None);
        assert_eq!(
            selection_to_message_range(source, 0, 8..400),
            Some(8..13),
            "clamped to the source"
        );
    }

    #[test]
    fn selection_mapping_is_utf8_safe() {
        // "é" is 2 bytes, "🦀" 4 bytes, "日本" 3 bytes each.
        let source = "café 🦀 日本語";
        let crab = source.find('🦀').unwrap();
        assert_eq!(
            selection_to_message_range(source, 0, crab..crab + 4),
            Some(crab..crab + 4)
        );
        // Ends inside a character snap outward to whole characters.
        assert_eq!(
            selection_to_message_range(source, 0, crab + 1..crab + 3),
            Some(crab..crab + 4)
        );
        let e = source.find('é').unwrap();
        let range = selection_to_message_range(source, 7, e + 1..e + 2).unwrap();
        assert_eq!(range, 7 + e..7 + e + 2);
        let ja = source.find('日').unwrap();
        let range = selection_to_message_range(source, 0, ja..source.len()).unwrap();
        assert_eq!(&source[range], "日本語");
        // The mapped range always slices the message text cleanly.
        let message = format!("{}{source}", "x".repeat(7));
        for start in 0..source.len() {
            for end in start..=source.len() {
                if let Some(range) = selection_to_message_range(source, 7, start..end) {
                    assert!(message.is_char_boundary(range.start));
                    assert!(message.is_char_boundary(range.end));
                    assert!(!message[range].trim().is_empty());
                }
            }
        }
    }

    const BLOCK: &str = "<review-comments>\n\
The user left comments on specific passages. Each quotes the passage it is about; address every comment.\n\
\n\
[1] On this passage of your reply:\n\
> use a map\n\
Comment: why not a vec?\n\
\n\
[2] On src/lib.rs line 3:\n\
```rust\n\
let x = 1;\n\
```\n\
Comment: rename\n\
Reply: and document it\n\
</review-comments>";

    #[test]
    fn prompt_block_splits_off_the_delivered_comments() {
        let text = format!("{BLOCK}\n\nPlease continue.");
        let block = split_prompt_block(&text).unwrap();
        assert_eq!(block.entries, 2);
        assert_eq!(&text[block.block.clone()], BLOCK);
        assert_eq!(&text[block.body_start..], "Please continue.");
        assert_eq!(prompt_body(&text), "Please continue.");

        // Comments only (`SendComments`): no body.
        let only = split_prompt_block(BLOCK).unwrap();
        assert_eq!(only.body_start, BLOCK.len());
        assert_eq!(prompt_body(BLOCK), "");

        assert_eq!(split_prompt_block("plain prompt"), None);
        assert_eq!(prompt_body("plain prompt"), "plain prompt");
        assert_eq!(
            split_prompt_block("<review-comments>\nunterminated"),
            None,
            "an unclosed block is ordinary text"
        );
    }

    #[test]
    fn prompt_block_ignores_a_closing_tag_quoted_inside_a_comment() {
        let tricky = "<review-comments>\nintro\n\n[1] On this passage:\n> x\nComment: the tag\n</review-comments> appears here\n[2] On y:\nComment: z\n</review-comments>\n\nbody text";
        let block = split_prompt_block(tricky).unwrap();
        assert_eq!(block.entries, 2);
        assert_eq!(&tricky[block.body_start..], "body text");
    }

    #[test]
    fn groups_put_replies_under_roots_and_rank_phases() {
        let comments = vec![
            snap("a", None, on_message("m1", 0, 3), delivered(2, Some("m9"))),
            snap("b", None, on_code("src/a.rs", 3, 3), pending()),
            snap("a1", Some("a"), on_message("m1", 0, 3), pending()),
            snap("a2", Some("a1"), on_message("m1", 0, 3), delivered(2, None)),
            snap("c", None, on_message("m2", 0, 1), resolved()),
            snap("c1", Some("c"), on_message("m2", 0, 1), pending()),
            snap(
                "x1",
                Some("missing"),
                on_message("m3", 0, 1),
                delivered(1, None),
            ),
        ];
        let groups = group_comments(&comments);
        let ids: Vec<&str> = groups.iter().map(|g| g.id().0.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "x1"], "roots in creation order");
        let replies: Vec<&str> = groups[0].replies.iter().map(|c| c.id.0.as_str()).collect();
        assert_eq!(replies, ["a1", "a2"], "a reply to a reply joins its root");

        assert_eq!(groups[0].phase(), Phase::Pending, "a pending reply reopens");
        assert!(groups[0].open_by_default());
        assert_eq!(groups[1].phase(), Phase::Pending);
        assert_eq!(groups[2].phase(), Phase::Resolved(None));
        assert_eq!(
            groups[2].pending_count(),
            0,
            "resolved threads are never sent"
        );
        assert!(!groups[2].open_by_default());
        match groups[3].phase() {
            Phase::Delivered(delivery) => {
                assert_eq!(delivery.turn, 1);
                assert_eq!(delivery.answer_message_id, None);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(pending_total(&groups), 2);

        let answered = group_comments(&[snap(
            "d",
            None,
            on_message("m1", 0, 1),
            delivered(4, Some("m7")),
        )]);
        assert!(matches!(
            answered[0].phase(),
            Phase::Delivered(CommentDelivery { turn: 4, .. })
        ));
        assert!(!answered[0].open_by_default());
    }

    #[test]
    fn message_anchors_are_relative_to_the_rendered_source() {
        let text = "<review-comments>\n[1] x\n</review-comments>\n\nnaïve plan here";
        let base = split_prompt_block(text).unwrap().body_start;
        let source = &text[base..];
        let plan = base + source.find("plan").unwrap();
        let comments = vec![
            snap("late", None, on_message("m1", plan, plan + 4), pending()),
            snap("early", None, on_message("m1", base, base + 6), pending()),
            snap("in-block", None, on_message("m1", 1, 5), pending()),
            snap("other", None, on_message("m2", 0, 4), pending()),
            snap(
                "past-end",
                None,
                on_message("m1", base, text.len() + 3),
                pending(),
            ),
            snap("code", None, on_code("a.rs", 1, 1), pending()),
        ];
        let groups = group_comments(&comments);
        let (threads, anchors) = message_anchors(&groups, &MessageId::from("m1"), base, source);
        assert_eq!(
            threads,
            vec![0, 1, 2, 4],
            "every thread on the message is listed"
        );
        assert_eq!(
            anchors,
            vec![
                MessageAnchor {
                    group: 1,
                    range: 0..6
                },
                MessageAnchor {
                    group: 0,
                    range: plan - base..plan - base + 4
                },
            ],
            "sorted, and only ranges inside the rendered source"
        );
        assert_eq!(&source[anchors[0].range.clone()], "naïve");
    }

    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 one
-two
+TWO
+three
 four
@@ -20 +21 @@ fn tail()
-old
+new
";

    #[test]
    fn diff_slots_place_code_and_hunk_comments() {
        let files = parse_unified_diff(DIFF);
        let hunks = &files[0].hunks;
        let slot = |snapshot: CommentSnapshot| {
            diff_slot(&group_comments(&[snapshot])[0], "r", "src/a.rs", hunks)
        };

        // Lines 2-3 (new side) end on "+three": hunk 0, line 3.
        assert_eq!(
            slot(snap("c", None, on_code("src/a.rs", 2, 3), pending())),
            Some(DiffSlot::Line { hunk: 0, line: 3 })
        );
        // Relocated by the daemon: placed at the new line, badge "moved".
        let mut moved = snap("m", None, on_code("src/a.rs", 7, 7), pending());
        moved.anchor_status = Some(AnchorStatus::Resolved { line: 21 });
        let group = &group_comments(&[moved.clone()])[0];
        assert_eq!(AnchorBadge::for_group(group), Some(AnchorBadge::Moved(21)));
        assert_eq!(slot(moved), Some(DiffSlot::Line { hunk: 1, line: 1 }));
        // Orphaned anchors are never attached to a line.
        let mut orphan = snap("o", None, on_code("src/a.rs", 2, 2), pending());
        orphan.anchor_status = Some(AnchorStatus::Orphaned);
        assert_eq!(
            slot(orphan.clone()),
            Some(DiffSlot::Detached(Detached::Orphaned))
        );
        assert_eq!(
            AnchorBadge::for_group(&group_comments(&[orphan])[0]),
            Some(AnchorBadge::Detached(Detached::Orphaned))
        );
        // Fuzzy stays at the original lines with a warning badge.
        let mut fuzzy = snap("f", None, on_code("src/a.rs", 1, 1), pending());
        fuzzy.anchor_status = Some(AnchorStatus::Fuzzy);
        assert_eq!(
            AnchorBadge::for_group(&group_comments(&[fuzzy.clone()])[0]),
            Some(AnchorBadge::Fuzzy)
        );
        assert_eq!(slot(fuzzy), Some(DiffSlot::Line { hunk: 0, line: 0 }));
        assert_eq!(
            slot(snap("far", None, on_code("src/a.rs", 90, 90), pending())),
            Some(DiffSlot::Detached(Detached::OutsideDiff))
        );

        assert_eq!(
            slot(snap(
                "h",
                None,
                on_hunk("src/a.rs", "@@ -20 +21 @@"),
                pending()
            )),
            Some(DiffSlot::Hunk(1))
        );
        assert_eq!(
            slot(snap(
                "h2",
                None,
                on_hunk("src/a.rs", "@@ -20 +21 @@ fn tail()"),
                pending()
            )),
            Some(DiffSlot::Hunk(1)),
            "a header with its section text still matches"
        );
        assert_eq!(
            slot(snap(
                "gone",
                None,
                on_hunk("src/a.rs", "@@ -5,2 +5,2 @@"),
                pending()
            )),
            Some(DiffSlot::Detached(Detached::Outdated))
        );
        assert_eq!(
            slot(snap("x", None, on_code("src/b.rs", 1, 1), pending())),
            None
        );
        assert_eq!(
            slot(snap("y", None, on_message("m", 0, 1), pending())),
            None
        );
    }

    #[test]
    fn comment_messages_upsert_replace_and_delete() {
        let thread = ThreadId::from("t");
        let mut comments = Vec::new();
        let a = snap("a", None, on_message("m1", 0, 1), pending());
        assert!(apply_comment_message(
            &mut comments,
            &thread,
            &DaemonMessage::CommentCreated(a.clone())
        ));
        let mut updated = a.clone();
        updated.state = serde_json::from_value(delivered(1, None)).unwrap();
        assert!(apply_comment_message(
            &mut comments,
            &thread,
            &DaemonMessage::CommentUpdated(updated.clone())
        ));
        assert_eq!(comments, vec![updated]);

        let mut foreign = snap("z", None, on_message("m1", 0, 1), pending());
        foreign.thread_id = ThreadId::from("other");
        assert!(!apply_comment_message(
            &mut comments,
            &thread,
            &DaemonMessage::CommentCreated(foreign)
        ));
        assert_eq!(comments.len(), 1);

        let b = snap("b", None, on_message("m1", 0, 1), pending());
        assert!(apply_comment_message(
            &mut comments,
            &thread,
            &DaemonMessage::Comments {
                thread_id: thread.clone(),
                comments: vec![a.clone(), b.clone()],
            }
        ));
        assert_eq!(comments.len(), 2);
        assert!(apply_comment_message(
            &mut comments,
            &thread,
            &DaemonMessage::CommentDeleted {
                thread_id: thread.clone(),
                comment_ids: vec![CommentId::from("a")],
            }
        ));
        assert_eq!(comments, vec![b]);
    }

    #[test]
    fn block_end_splits_after_the_anchored_paragraph() {
        let source = "First paragraph, café.\nStill first.\n\nSecond 🦀 one.\n\n```rust\nlet a = 1;\n\nlet b = 2;\n```\n\nTail";
        let second = source.find("Second").unwrap();
        let first_end = block_end(source, 3);
        assert_eq!(
            &source[..first_end],
            "First paragraph, café.\nStill first.\n"
        );
        assert!(
            source[first_end..].starts_with('\n'),
            "the blank line opens the next part"
        );
        let crab = source.find('🦀').unwrap();
        let end = block_end(source, crab + 4);
        assert_eq!(&source[second..end], "Second 🦀 one.\n");
        // A blank line inside fenced code does not split the block.
        let code = source.find("let a").unwrap();
        let end = block_end(source, code);
        assert!(source[..end].ends_with("```\n"));
        assert_eq!(block_end(source, source.len() - 1), source.len());
        assert_eq!(block_end("one line", 0), "one line".len());
        // Every cut is a character boundary.
        for offset in 0..=source.len() {
            assert!(source.is_char_boundary(block_end(source, offset)));
            assert!(block_end(source, offset) >= offset.min(source.len()));
        }
    }

    #[test]
    fn typing_a_printable_key_starts_a_comment() {
        let key = |text: &str| typed_text(&Keystroke::parse(text).unwrap());
        assert_eq!(key("a"), Some("a".into()));
        assert_eq!(key("shift-a"), Some("A".into()));
        assert_eq!(key("space"), Some(" ".into()));
        assert_eq!(key("alt-e->é"), Some("é".into()));
        assert_eq!(key("cmd-c"), None, "copy stays copy");
        assert_eq!(key("ctrl-a"), None);
        assert_eq!(key("escape"), None);
        assert_eq!(key("enter"), None);
        assert_eq!(key("up"), None);
        assert_eq!(key("f5"), None);
    }

    #[test]
    fn picked_diff_lines_become_code_or_hunk_targets() {
        let files = parse_unified_diff(DIFF);
        let hunk = &files[0].hunks[0];
        match diff_target("r", "src/a.rs", "abc", hunk, 1..=3) {
            CommentTarget::Code { anchor } => {
                assert_eq!((anchor.start_line, anchor.end_line), (2, 3));
                assert_eq!(anchor.path, std::path::PathBuf::from("src/a.rs"));
                assert_eq!(anchor.repository_id.0, "r");
            }
            other => panic!("expected code, got {other:?}"),
        }
        assert_eq!(
            diff_target("r", "src/a.rs", "abc", hunk, 1..=1),
            CommentTarget::DiffHunk {
                repository_id: "r".into(),
                path: "src/a.rs".into(),
                base_revision: "abc".into(),
                hunk_header: "@@ -1,3 +1,4 @@".into(),
            },
            "a removed line has no new-side line: the hunk is the target"
        );
    }

    #[test]
    fn block_comments_quote_their_passage_and_read_back() {
        let body = proxy_body(KIND_THINKING, " line one\n\nline three ", " fix this ");
        assert_eq!(body, "> Thinking: line one\n> \n> line three\n\nfix this");
        assert_eq!(
            parse_proxy(&body),
            Some(Proxy {
                kind: KIND_THINKING,
                excerpt: "line one\n\nline three".into(),
                comment: "fix this".into(),
            })
        );
        assert_eq!(display_body(&body), "fix this");
        assert_eq!(display_body("plain comment"), "plain comment");
        assert_eq!(parse_proxy("plain comment"), None);
        assert_eq!(parse_proxy("> Unknown: x\n\ny"), None);
        assert_eq!(parse_proxy("> Thinking: no blank line"), None);
        let patch = proxy_body(KIND_PATCH, "- old\n+ new", "why?");
        assert_eq!(parse_proxy(&patch).unwrap().excerpt, "- old\n+ new");
        let long = proxy_body(KIND_COMMAND, &"x".repeat(700), "c");
        assert!(parse_proxy(&long).unwrap().excerpt.ends_with('…'));
    }

    #[test]
    fn block_comments_anchor_to_the_turn_message_but_not_its_text() {
        // Stored on the first line of the answer...
        let text = "\n\n  Hello world\nmore";
        let range = proxy_anchor(text).unwrap();
        assert_eq!(&text[range], "Hello world");
        assert_eq!(proxy_anchor(" \n "), None);
        let wide = "é".repeat(150);
        let range = proxy_anchor(&wide).unwrap();
        assert!(range.end <= 200 && wide.is_char_boundary(range.end));
        // ...but neither highlighted nor placed there.
        let mut on_block = snap("b", None, on_message("m", 0, 5), pending());
        on_block.body = proxy_body(KIND_THINKING, "plan", "hmm");
        let groups = group_comments(&[on_block, snap("a", None, on_message("m", 6, 9), pending())]);
        let (threads, anchors) = message_anchors(&groups, &MessageId::from("m"), 0, "Hello world");
        assert_eq!(threads, vec![1]);
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].group, 1);
    }

    #[test]
    fn transcript_diff_lines_anchor_to_code_in_a_known_checkout() {
        let checkouts = vec![
            ("r1".to_string(), std::path::PathBuf::from("/w/app")),
            (
                "r2".to_string(),
                std::path::PathBuf::from("/w/app/vendor/lib"),
            ),
        ];
        assert_eq!(
            repo_relative(&checkouts, "/w/app/vendor/lib/x.rs"),
            Some(("r2".into(), "x.rs".into()))
        );
        assert_eq!(
            repo_relative(&checkouts, "/w/app/src/y.rs"),
            Some(("r1".into(), "src/y.rs".into()))
        );
        assert_eq!(repo_relative(&checkouts, "/elsewhere/z.rs"), None);
        assert_eq!(repo_relative(&checkouts, "src/a.rs"), None, "ambiguous");
        assert_eq!(
            repo_relative(&checkouts[..1], "src/a.rs"),
            Some(("r1".into(), "src/a.rs".into()))
        );

        let files = parse_unified_diff(DIFF);
        let hunk = &files[0].hunks[0];
        match patch_target(&checkouts, "/w/app/src/a.rs", hunk, 1..=3) {
            Some(CommentTarget::Code { anchor }) => {
                assert_eq!(anchor.repository_id.0, "r1");
                assert_eq!(anchor.path, std::path::PathBuf::from("src/a.rs"));
                assert_eq!((anchor.start_line, anchor.end_line), (2, 3));
                assert!(code_on_edit(&anchor, "/w/app/src/a.rs", &[1, 2]));
                assert!(!code_on_edit(&anchor, "/w/app/src/a.rs", &[7]));
                assert!(!code_on_edit(&anchor, "/w/app/src/b.rs", &[2]));
                assert!(!code_on_edit(&anchor, "/w/app/xsrc/a.rs", &[2]));
            }
            other => panic!("expected code, got {other:?}"),
        }
        assert_eq!(
            patch_target(&checkouts, "/w/app/src/a.rs", hunk, 1..=1),
            None,
            "removed lines only: a block comment quoting them"
        );
        assert_eq!(patch_target(&checkouts, "/tmp/a.rs", hunk, 1..=3), None);
    }

    #[test]
    fn labels_are_short_and_human() {
        assert_eq!(relative_time(100, 110), "just now");
        assert_eq!(relative_time(0, 300), "5m ago");
        assert_eq!(relative_time(0, 7_200), "2h ago");
        assert_eq!(relative_time(0, 3 * 86_400), "3d ago");
        assert_eq!(relative_time(0, 15 * 86_400), "2w ago");
        assert_eq!(relative_time(500, 100), "just now", "clock skew");
        assert_eq!(plural(1, "comment", "comments"), "1 comment");
        assert_eq!(plural(3, "comment", "comments"), "3 comments");
        assert_eq!(one_line("first\nsecond", 80), "first…");
        let own = snap("a", None, on_message("m", 0, 1), pending());
        assert!(can_delete(&own));
        let mut sent = own.clone();
        sent.state = serde_json::from_value(delivered(1, None)).unwrap();
        assert!(!can_delete(&sent));
    }
}
