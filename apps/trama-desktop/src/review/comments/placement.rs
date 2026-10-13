use super::*;

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

    pub(super) fn label(self) -> String {
        match self {
            Self::Moved(line) => format!("Moved to line {line}"),
            Self::Fuzzy => "May have moved".into(),
            Self::Detached(Detached::Orphaned) => "Detached: code changed".into(),
            Self::Detached(Detached::OutsideDiff) => "Outside the shown diff".into(),
            Self::Detached(Detached::Outdated) => "Outdated hunk".into(),
        }
    }

    pub(super) fn color(self, p: &Palette) -> Hsla {
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
