//! Right drawer and its Changes view.
//!
//! The workspace asks the daemon for `GetChanges` and feeds each repository's
//! [`RepoChanges`] in with [`ChangesPanel::set_changes`]. `files` is the
//! complete list (stats), `diff` provides hunks and may be truncated.

use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Sizable as _, checkbox::Checkbox, h_flex, scroll::ScrollableElement as _,
    tooltip::Tooltip, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::{CommentId, MessageId};
use trama_protocol::{ChangeBase, FileChange, FileChangeStatus, RepoChanges, ThreadId};

use crate::comments::{
    self, AnchorBadge, CardHandlers, CommentDraft, CommentGroup, CommentStore, Detached, DiffSlot,
};
use crate::diff::{DiffFile, DiffLine, FileStatus, LineKind, parse_unified_diff, totals};
use crate::theme::{self, Palette};

/// One repository's files, ready to render.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoView {
    /// Stable key (repository id) used to scope seen/collapsed state.
    pub key: String,
    pub label: String,
    pub base_revision: Option<String>,
    pub truncated: bool,
    pub files: Vec<DiffFile>,
    /// Paths listed in `files` whose hunks are not in the (truncated) diff.
    pub without_hunks: HashSet<String>,
}

impl RepoView {
    /// Map a daemon `RepoChanges`: stats and order come from `files`, hunks
    /// from the parsed diff. Files missing from the diff keep their stats.
    pub fn from_changes(repo: &RepoChanges) -> Self {
        let mut parsed: HashMap<String, DiffFile> = parse_unified_diff(&repo.diff)
            .into_iter()
            .map(|file| (file.path.clone(), file))
            .collect();
        let mut without_hunks = HashSet::new();
        let files = repo
            .files
            .iter()
            .map(|change| {
                let file = match parsed.remove(&change.path) {
                    Some(file) => file,
                    None => {
                        without_hunks.insert(change.path.clone());
                        stub_file(change)
                    }
                };
                apply_stats(file, change)
            })
            .collect();
        Self {
            key: repo.repository_id.to_string(),
            label: repo.title.clone(),
            base_revision: Some(repo.base_revision.clone()).filter(|rev| !rev.is_empty()),
            truncated: repo.truncated,
            files,
            without_hunks,
        }
    }

    fn from_diff(label: String, unified_diff: &str) -> Self {
        Self {
            key: label.clone(),
            label,
            base_revision: None,
            truncated: false,
            files: parse_unified_diff(unified_diff),
            without_hunks: HashSet::new(),
        }
    }
}

fn map_status(status: FileChangeStatus) -> FileStatus {
    match status {
        FileChangeStatus::Added => FileStatus::Added,
        FileChangeStatus::Deleted => FileStatus::Deleted,
        FileChangeStatus::Renamed | FileChangeStatus::Copied => FileStatus::Renamed,
        FileChangeStatus::Modified | FileChangeStatus::TypeChanged | FileChangeStatus::Unknown => {
            FileStatus::Modified
        }
    }
}

fn stub_file(change: &FileChange) -> DiffFile {
    DiffFile {
        path: change.path.clone(),
        old_path: None,
        status: FileStatus::Modified,
        binary: false,
        additions: 0,
        deletions: 0,
        hunks: Vec::new(),
    }
}

fn apply_stats(mut file: DiffFile, change: &FileChange) -> DiffFile {
    file.status = map_status(change.status);
    if change.old_path.is_some() {
        file.old_path = change.old_path.clone();
    }
    match (change.additions, change.deletions) {
        (Some(additions), Some(deletions)) => {
            file.additions = additions;
            file.deletions = deletions;
        }
        _ => file.binary = true,
    }
    file
}

/// Label for a diff base in the selector.
pub fn base_label(base: ChangeBase) -> &'static str {
    match base {
        ChangeBase::ThreadBase => "Thread base",
        ChangeBase::Head => "Last commit",
        ChangeBase::LastTurn => "Last turn",
    }
}

fn base_hint(base: ChangeBase) -> &'static str {
    match base {
        ChangeBase::ThreadBase => "since the thread started",
        ChangeBase::Head => "since the last commit",
        ChangeBase::LastTurn => "since the latest turn started",
    }
}

pub enum ChangesEvent {
    /// The user picked another base or asked for a refresh.
    Refresh,
    /// Show the transcript message that answered a comment.
    RevealMessage(MessageId),
}

/// Diff lines picked with the mouse, inside one hunk. Typing while lines
/// are picked starts a comment on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineSelection {
    repo: usize,
    file: usize,
    hunk: usize,
    anchor: usize,
    cursor: usize,
}

impl LineSelection {
    fn lines(&self) -> RangeInclusive<usize> {
        self.anchor.min(self.cursor)..=self.anchor.max(self.cursor)
    }

    fn in_hunk(&self, repo: usize, file: usize, hunk: usize) -> bool {
        (self.repo, self.file, self.hunk) == (repo, file, hunk)
    }
}

/// Where the open composer sits in the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DraftAt {
    /// Under the last picked line.
    Lines(LineSelection),
    /// Under a whole hunk.
    Hunk {
        repo: usize,
        file: usize,
        hunk: usize,
    },
    Reply(CommentId),
}

struct DiffDraft {
    at: DraftAt,
    draft: CommentDraft,
}

enum Content {
    /// No thread selected (or nothing requested yet).
    Idle,
    Loading,
    /// The daemon answered the request with an error.
    Failed(String),
    Repos(Vec<RepoView>),
}

pub struct ChangesPanel {
    base: ChangeBase,
    thread: Option<ThreadId>,
    content: Content,
    collapsed: HashSet<String>,
    /// File key → fingerprint of the change the user marked as seen.
    seen: HashMap<String, u64>,
    comments: Entity<CommentStore>,
    focus_handle: FocusHandle,
    selection: Option<LineSelection>,
    dragging: bool,
    draft: Option<DiffDraft>,
    /// Threads the user opened or closed, overriding the default.
    thread_open: HashMap<CommentId, bool>,
    _comments_observer: Subscription,
}

impl EventEmitter<ChangesEvent> for ChangesPanel {}

fn file_key(repo: &str, path: &str) -> String {
    format!("{repo}\u{0}{path}")
}

impl ChangesPanel {
    pub fn new(comments: Entity<CommentStore>, cx: &mut Context<Self>) -> Self {
        let observer = cx.observe(&comments, |_, _, cx| cx.notify());
        Self {
            base: ChangeBase::default(),
            thread: None,
            content: Content::Idle,
            collapsed: HashSet::new(),
            seen: HashMap::new(),
            comments,
            focus_handle: cx.focus_handle(),
            selection: None,
            dragging: false,
            draft: None,
            thread_open: HashMap::new(),
            _comments_observer: observer,
        }
    }

    /// An inline comment composer is open (Escape cancels it rather than
    /// closing the drawer).
    pub fn has_draft(&self) -> bool {
        self.draft.is_some()
    }

    fn forget_comment_ui(&mut self) {
        self.selection = None;
        self.dragging = false;
        self.draft = None;
    }

    pub fn base(&self) -> ChangeBase {
        self.base
    }

    /// A `GetChanges` for `thread` is in flight. Switching threads drops the
    /// previous thread's view and marks.
    pub fn begin_loading(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        if self.thread.as_ref() != Some(&thread) {
            self.collapsed.clear();
            self.seen.clear();
            self.thread_open.clear();
            self.forget_comment_ui();
            self.content = Content::Loading;
        } else if matches!(self.content, Content::Idle) {
            self.content = Content::Loading;
        }
        self.thread = Some(thread);
        cx.notify();
    }

    /// Apply a `Changes` reply. Returns false (and ignores it) when it is for
    /// another thread or base than the one shown.
    pub fn set_changes(
        &mut self,
        thread: &ThreadId,
        base: ChangeBase,
        repos: &[RepoChanges],
        cx: &mut Context<Self>,
    ) -> bool {
        if self.thread.as_ref() != Some(thread) || self.base != base {
            return false;
        }
        let views: Vec<RepoView> = repos.iter().map(RepoView::from_changes).collect();
        // Line indices may point elsewhere in the new diff.
        if self
            .draft
            .as_ref()
            .is_some_and(|open| !matches!(open.at, DraftAt::Reply(_)))
        {
            self.draft = None;
        }
        self.selection = None;
        self.replace(views);
        cx.notify();
        true
    }

    /// Show `unified_diff` (output of `git diff`) for one repository. Used by
    /// the dev hook; the daemon path goes through [`Self::set_changes`].
    pub fn set_diff(
        &mut self,
        repo_label: impl Into<String>,
        unified_diff: &str,
        cx: &mut Context<Self>,
    ) {
        self.replace(vec![RepoView::from_diff(repo_label.into(), unified_diff)]);
        cx.notify();
    }

    fn replace(&mut self, views: Vec<RepoView>) {
        let current: Vec<(String, u64)> = views
            .iter()
            .flat_map(|repo| {
                repo.files
                    .iter()
                    .map(|file| (file_key(&repo.key, &file.path), file.fingerprint()))
            })
            .collect();
        retain_seen(&mut self.seen, &current);
        let keys: HashSet<&str> = current.iter().map(|(key, _)| key.as_str()).collect();
        self.collapsed.retain(|key| keys.contains(key.as_str()));
        self.content = Content::Repos(views);
    }

    /// Errors are not correlated to requests; while a request is in flight
    /// the workspace hands the next daemon error here. Returns whether it
    /// was taken.
    pub fn fail_loading(&mut self, message: &str, cx: &mut Context<Self>) -> bool {
        if !matches!(self.content, Content::Loading) {
            return false;
        }
        self.content = Content::Failed(message.to_string());
        cx.notify();
        true
    }

    /// Back to the "no data" state, e.g. when no thread is selected.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.forget_comment_ui();
        self.thread = None;
        self.content = Content::Idle;
        self.collapsed.clear();
        self.seen.clear();
        cx.notify();
    }

    fn select_base(&mut self, base: ChangeBase, cx: &mut Context<Self>) {
        if self.base != base {
            self.base = base;
            self.forget_comment_ui();
            self.content = Content::Loading;
            self.seen.clear();
            self.collapsed.clear();
        }
        cx.emit(ChangesEvent::Refresh);
        cx.notify();
    }

    fn is_seen(&self, key: &str, file: &DiffFile) -> bool {
        self.seen.get(key) == Some(&file.fingerprint())
    }

    fn file(&self, repo: usize, index: usize) -> Option<(String, &DiffFile)> {
        let Content::Repos(repos) = &self.content else {
            return None;
        };
        let repo = repos.get(repo)?;
        let file = repo.files.get(index)?;
        Some((file_key(&repo.key, &file.path), file))
    }

    fn set_seen(&mut self, repo: usize, index: usize, seen: bool, cx: &mut Context<Self>) {
        let Some((key, file)) = self.file(repo, index) else {
            return;
        };
        let fingerprint = file.fingerprint();
        if seen {
            self.seen.insert(key.clone(), fingerprint);
            self.collapsed.insert(key);
        } else {
            self.seen.remove(&key);
            self.collapsed.remove(&key);
        }
        cx.notify();
    }

    fn toggle_collapsed(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        cx.notify();
    }
}

// -- comments -------------------------------------------------------------

impl ChangesPanel {
    fn repo_file(&self, repo: usize, file: usize) -> Option<(&RepoView, &DiffFile)> {
        let Content::Repos(repos) = &self.content else {
            return None;
        };
        let view = repos.get(repo)?;
        Some((view, view.files.get(file)?))
    }

    /// Comments need the daemon's view of the repository: its id and base.
    fn commentable(&self, repo: usize) -> bool {
        self.thread.is_some()
            && self
                .repo_file(repo, 0)
                .is_some_and(|(view, _)| view.base_revision.is_some())
    }

    /// Pick a line (shift extends within the hunk) and take keyboard focus,
    /// so typing comments on the pick.
    fn press_line(
        &mut self,
        at: (usize, usize, usize, usize),
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (repo, file, hunk, line) = at;
        if !self.commentable(repo) {
            return;
        }
        self.focus_handle.focus(window, cx);
        self.selection = match self.selection {
            Some(mut picked) if extend && picked.in_hunk(repo, file, hunk) => {
                picked.cursor = line;
                Some(picked)
            }
            _ => Some(LineSelection {
                repo,
                file,
                hunk,
                anchor: line,
                cursor: line,
            }),
        };
        self.dragging = true;
        cx.notify();
    }

    fn drag_line(&mut self, at: (usize, usize, usize, usize), cx: &mut Context<Self>) {
        let (repo, file, hunk, line) = at;
        if let Some(picked) = self.selection.as_mut()
            && self.dragging
            && picked.in_hunk(repo, file, hunk)
            && picked.cursor != line
        {
            picked.cursor = line;
            cx.notify();
        }
    }

    /// ⌘⌥M with lines picked.
    pub fn has_selection(&self) -> bool {
        self.selection.is_some() && self.draft.is_none()
    }

    pub fn comment_on_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picked) = self.selection {
            self.open_lines_draft(picked, None, window, cx);
        }
    }

    fn type_to_comment(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picked) = self.selection else {
            return;
        };
        if self
            .draft
            .as_ref()
            .is_some_and(|open| open.draft.is_focused(window, cx))
        {
            return;
        }
        if event.keystroke.key == "escape" {
            self.selection = None;
            cx.notify();
            return;
        }
        let Some(text) = comments::typed_text(&event.keystroke) else {
            return;
        };
        cx.stop_propagation();
        window.prevent_default();
        self.open_lines_draft(picked, Some(text), window, cx);
    }

    fn open_lines_draft(
        &mut self,
        picked: LineSelection,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((view, file)) = self.repo_file(picked.repo, picked.file) else {
            return;
        };
        let (Some(base), Some(hunk)) = (view.base_revision.as_deref(), file.hunks.get(picked.hunk))
        else {
            return;
        };
        let target = comments::diff_target(&view.key, &file.path, base, hunk, picked.lines());
        self.open_draft(DraftAt::Lines(picked), target, None, initial, window, cx);
    }

    fn open_hunk_draft(
        &mut self,
        (repo, file, hunk): (usize, usize, usize),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((view, diff)) = self.repo_file(repo, file) else {
            return;
        };
        let (Some(base), Some(header)) = (
            view.base_revision.clone(),
            diff.hunks.get(hunk).map(|hunk| hunk.header.clone()),
        ) else {
            return;
        };
        let target = trama_protocol::CommentTarget::DiffHunk {
            repository_id: view.key.as_str().into(),
            path: diff.path.clone().into(),
            base_revision: base,
            hunk_header: header,
        };
        self.selection = None;
        self.open_draft(
            DraftAt::Hunk { repo, file, hunk },
            target,
            None,
            None,
            window,
            cx,
        );
    }

    fn open_reply(&mut self, root: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .comments
            .read(cx)
            .groups()
            .into_iter()
            .find(|group| group.id() == &root)
            .map(|group| group.root.target);
        if let Some(target) = target {
            self.thread_open.insert(root.clone(), true);
            self.open_draft(
                DraftAt::Reply(root.clone()),
                target,
                Some(root),
                None,
                window,
                cx,
            );
        }
    }

    fn open_draft(
        &mut self,
        at: DraftAt,
        target: trama_protocol::CommentTarget,
        parent: Option<CommentId>,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dragging = false;
        let draft = CommentDraft::new(
            target,
            parent,
            initial,
            window,
            cx,
            |this: &mut Self, window, cx| this.submit_draft(window, cx),
        );
        self.draft = Some(DiffDraft { at, draft });
        cx.notify();
    }

    fn submit_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.draft else {
            return;
        };
        if open.draft.submit(&self.comments, cx) {
            self.draft = None;
            self.selection = None;
            self.focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    fn cancel_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft = None;
        self.selection = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn toggle_thread(&mut self, id: CommentId, cx: &mut Context<Self>) {
        let default = self
            .comments
            .read(cx)
            .groups()
            .iter()
            .find(|group| group.id() == &id)
            .is_some_and(CommentGroup::open_by_default);
        let open = self.thread_open.get(&id).copied().unwrap_or(default);
        self.thread_open.insert(id, !open);
        cx.notify();
    }

    fn card_handlers(&self, cx: &mut Context<Self>) -> CardHandlers {
        let toggle = cx.entity().downgrade();
        let reply = toggle.clone();
        let answer = toggle.clone();
        CardHandlers {
            store: self.comments.clone(),
            on_toggle: Rc::new(move |id, _, cx| {
                let _ = toggle.update(cx, |this, cx| this.toggle_thread(id, cx));
            }),
            on_reply: Rc::new(move |id, window, cx| {
                let _ = reply.update(cx, |this, cx| this.open_reply(id, window, cx));
            }),
            on_answer: Rc::new(move |id, _, cx| {
                let _ = answer.update(cx, |_, cx| cx.emit(ChangesEvent::RevealMessage(id)));
            }),
        }
    }

    fn render_open_draft(
        &self,
        open: &DiffDraft,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let submit = cx.listener(|this, _, window, cx| this.submit_draft(window, cx));
        let weak = cx.entity().downgrade();
        let cancel: comments::CancelDraft = Rc::new(move |window, cx| {
            let _ = weak.update(cx, |this, cx| this.cancel_draft(window, cx));
        });
        comment_slot(comments::render_draft(
            "diff-draft",
            &open.draft,
            submit,
            cancel,
            p,
            cx,
        ))
    }

    /// A thread in the diff: its card (with a reply composer) or a row.
    #[allow(clippy::too_many_arguments)]
    fn thread_element(
        &self,
        group: &CommentGroup,
        badge: Option<AnchorBadge>,
        show_excerpt: bool,
        handlers: &CardHandlers,
        now: i64,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let badge = badge.or_else(|| AnchorBadge::for_group(group));
        let open = self
            .thread_open
            .get(group.id())
            .copied()
            .unwrap_or_else(|| group.open_by_default());
        if !open {
            return comment_slot(comments::render_collapsed(
                format!("diff-thread-row-{}", group.id()),
                group,
                badge,
                handlers,
                p,
            ));
        }
        let reply = self
            .draft
            .as_ref()
            .filter(|open| open.at == DraftAt::Reply(group.id().clone()))
            .map(|open| self.render_open_draft(open, p, cx));
        comment_slot(comments::render_card(
            format!("diff-thread-{}", group.id()),
            group,
            show_excerpt,
            badge,
            reply,
            handlers,
            now,
            p,
        ))
    }
}

/// Comment UI inside the monospace diff column: UI type, and no wider
/// than the drawer even when long lines widen the column.
fn comment_slot(element: AnyElement) -> AnyElement {
    div()
        .w_full()
        .max_w(theme::drawer_width() - px(44.))
        .px(theme::sp(2.))
        .py(theme::sp(1.5))
        .font_family(theme::UI_FONT)
        .text_size(theme::text_ui())
        .child(element)
        .into_any_element()
}

#[derive(Debug, Clone, Copy)]
struct FileFlags {
    collapsed: bool,
    seen: bool,
    hunks_missing: bool,
}

/// Keep only seen marks whose file still has the identical change.
fn retain_seen(seen: &mut HashMap<String, u64>, current: &[(String, u64)]) {
    let current: HashMap<&str, u64> = current
        .iter()
        .map(|(key, fingerprint)| (key.as_str(), *fingerprint))
        .collect();
    seen.retain(|key, fingerprint| current.get(key.as_str()) == Some(fingerprint));
}

impl Render for ChangesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let base = self.base;
        let selector = h_flex()
            .flex_shrink_0()
            .px(theme::sp(2.))
            .py(theme::sp(1.5))
            .gap(theme::sp(1.))
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .children(
                [
                    ChangeBase::ThreadBase,
                    ChangeBase::Head,
                    ChangeBase::LastTurn,
                ]
                .into_iter()
                .map(|option| {
                    let active = option == base;
                    div()
                        .id(SharedString::from(format!(
                            "changes-base-{}",
                            base_label(option)
                        )))
                        .px(theme::sp(2.))
                        .py(theme::sp(0.5))
                        .rounded(theme::radius())
                        .text_size(theme::text_xs())
                        .cursor_pointer()
                        .when(active, |this| this.bg(p.selected).text_color(p.text))
                        .when(!active, |this| {
                            this.text_color(p.text_faint)
                                .hover(|style| style.bg(p.hover).text_color(p.text))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.select_base(option, cx)))
                        .child(base_label(option))
                }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("changes-refresh")
                    .p(theme::sp(1.))
                    .rounded(theme::radius())
                    .cursor_pointer()
                    .hover(|style| style.bg(p.hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_base(base, cx)))
                    .child(
                        Icon::new(IconName::RefreshCw)
                            .with_size(px(12.))
                            .text_color(p.text_muted),
                    ),
            );

        let body: AnyElement = match &self.content {
            Content::Idle => empty_state(
                &p,
                "No changes to show",
                "Select a thread to see what its agent changed.",
            )
            .into_any_element(),
            Content::Loading => {
                empty_state(&p, "Loading changes…", "Asking the daemon for a diff.")
                    .into_any_element()
            }
            Content::Failed(message) => {
                empty_state(&p, "Could not load changes", message.clone()).into_any_element()
            }
            Content::Repos(repos) if repos.is_empty() => empty_state(
                &p,
                "No repositories",
                "This thread has no checkouts to diff.",
            )
            .into_any_element(),
            Content::Repos(repos) if repos.iter().all(|repo| repo.files.is_empty()) => empty_state(
                &p,
                "No changes",
                format!(
                    "{} {}.",
                    repos
                        .iter()
                        .map(|repo| repo.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    match repos.len() {
                        1 => format!("has no changes {}", base_hint(base)),
                        _ => format!("have no changes {}", base_hint(base)),
                    }
                ),
            )
            .into_any_element(),
            Content::Repos(repos) => {
                let groups = self.comments.read(cx).groups();
                let handlers = self.card_handlers(cx);
                let now = comments::now_secs();
                let shown: HashSet<(String, String)> = repos
                    .iter()
                    .flat_map(|repo| {
                        repo.files
                            .iter()
                            .map(|file| (repo.key.clone(), file.path.clone()))
                    })
                    .collect();
                let elsewhere: Vec<&CommentGroup> = groups
                    .iter()
                    .filter(|group| {
                        comments::target_file(&group.root.target)
                            .is_some_and(|file| !shown.contains(&file))
                    })
                    .collect();
                let mut list = v_flex()
                    .id("changes-files")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .p(theme::sp(2.))
                    .gap(theme::sp(2.));
                if !elsewhere.is_empty() {
                    list = list.child(
                        div()
                            .px(theme::sp(1.))
                            .pt(theme::sp(1.))
                            .text_size(theme::text_sm())
                            .text_color(p.text_muted)
                            .child("Comments on files without changes here"),
                    );
                    for group in elsewhere {
                        list = list.child(self.thread_element(
                            group,
                            Some(AnchorBadge::Detached(Detached::OutsideDiff)),
                            true,
                            &handlers,
                            now,
                            &p,
                            cx,
                        ));
                    }
                }
                for (repo_index, repo) in repos.iter().enumerate() {
                    if repo.files.is_empty() && repos.len() > 1 {
                        list = list.child(repo_header(repo, 0, &p));
                        continue;
                    }
                    let seen_count = repo
                        .files
                        .iter()
                        .filter(|file| self.is_seen(&file_key(&repo.key, &file.path), file))
                        .count();
                    list = list.child(repo_header(repo, seen_count, &p));
                    if repo.truncated {
                        list = list.child(
                            h_flex()
                                .gap(theme::sp(1.5))
                                .px(theme::sp(2.))
                                .text_size(theme::text_xs())
                                .text_color(p.warning)
                                .child(Icon::new(IconName::TriangleAlert).with_size(px(12.)))
                                .child(
                                    "The diff was cut at 8 MiB. Files past the cut show stats only.",
                                ),
                        );
                    }
                    for (index, file) in repo.files.iter().enumerate() {
                        let key = file_key(&repo.key, &file.path);
                        let collapsed = self.collapsed.contains(&key);
                        let seen = self.is_seen(&key, file);
                        let hunks_missing = repo.without_hunks.contains(&file.path);
                        list = list.child(self.render_file(
                            (repo_index, index),
                            repo,
                            key,
                            file,
                            FileFlags {
                                collapsed,
                                seen,
                                hunks_missing,
                            },
                            &groups,
                            &handlers,
                            now,
                            &p,
                            cx,
                        ));
                    }
                }
                list.into_any_element()
            }
        };

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.type_to_comment(event, window, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.dragging {
                        this.dragging = false;
                        cx.notify();
                    }
                }),
            )
            .child(selector)
            .child(body)
    }
}

fn repo_header(repo: &RepoView, seen_count: usize, p: &Palette) -> impl IntoElement {
    let (count, additions, deletions) = totals(&repo.files);
    h_flex()
        .flex_shrink_0()
        .px(theme::sp(1.))
        .pt(theme::sp(1.))
        .gap(theme::sp(2.))
        .items_center()
        .text_size(theme::text_sm())
        .child(
            Icon::new(IconName::FolderGit2)
                .with_size(px(13.))
                .text_color(p.text_muted),
        )
        .child(
            div()
                .text_color(p.text)
                .font_weight(FontWeight::MEDIUM)
                .child(repo.label.clone()),
        )
        .when_some(repo.base_revision.clone(), |this, rev| {
            this.child(
                div()
                    .font_family(theme::MONO_FONT)
                    .text_size(theme::text_xs())
                    .text_color(p.text_faint)
                    .child(rev.chars().take(8).collect::<String>()),
            )
        })
        .child(
            div()
                .text_color(p.text_faint)
                .child(format!("{count} file{}", if count == 1 { "" } else { "s" })),
        )
        .child(counts(additions, deletions, p))
        .child(div().flex_1())
        .when(count > 0, |this| {
            this.child(
                div()
                    .text_color(p.text_faint)
                    .child(format!("{seen_count}/{count} seen")),
            )
        })
}

fn empty_state(p: &Palette, title: &str, body: impl Into<SharedString>) -> impl IntoElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(theme::sp(2.))
        .px(theme::sp(8.))
        .child(
            div()
                .size(px(36.))
                .rounded(theme::radius_lg())
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(IconName::FileDiff)
                        .with_size(px(16.))
                        .text_color(p.text_muted),
                ),
        )
        .child(
            div()
                .text_size(theme::text_ui())
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.text)
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(theme::text_sm())
                .text_color(p.text_muted)
                .text_center()
                .child(body.into()),
        )
}

fn counts(additions: u32, deletions: u32, p: &Palette) -> impl IntoElement {
    h_flex()
        .gap(theme::sp(1.5))
        .font_family(theme::MONO_FONT)
        .text_size(theme::text_xs())
        .child(
            div()
                .text_color(p.diff_add_text)
                .child(format!("+{additions}")),
        )
        .child(
            div()
                .text_color(p.diff_del_text)
                .child(format!("\u{2212}{deletions}")),
        )
}

fn status_color(status: FileStatus, p: &Palette) -> Hsla {
    match status {
        FileStatus::Added => p.success,
        FileStatus::Deleted => p.danger,
        FileStatus::Modified => p.warning,
        FileStatus::Renamed => p.accent_text,
    }
}

fn split_path(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name.to_string()),
        None => (String::new(), path.to_string()),
    }
}

impl ChangesPanel {
    #[allow(clippy::too_many_arguments)]
    fn render_file(
        &self,
        (repo, index): (usize, usize),
        view: &RepoView,
        key: String,
        file: &DiffFile,
        flags: FileFlags,
        groups: &[CommentGroup],
        handlers: &CardHandlers,
        now: i64,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let FileFlags {
            collapsed,
            seen,
            hunks_missing,
        } = flags;
        let (dir, name) = split_path(&file.path);
        let id = SharedString::from(format!("diff-file-{repo}-{index}"));
        let slots: Vec<(&CommentGroup, DiffSlot)> = groups
            .iter()
            .filter_map(|group| {
                comments::diff_slot(group, &view.key, &file.path, &file.hunks)
                    .map(|slot| (group, slot))
            })
            .collect();
        let thread_count = slots.len();
        let commentable = view.base_revision.is_some() && self.thread.is_some();
        let header = h_flex()
            .id(id)
            .w_full()
            .gap(theme::sp(2.))
            .px(theme::sp(2.))
            .py(theme::sp(1.5))
            .items_center()
            .cursor_pointer()
            .hover(|style| style.bg(p.hover))
            .when(!collapsed, |this| this.border_b_1().border_color(p.border))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_collapsed(key.clone(), cx)))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .with_size(px(12.))
                .text_color(p.text_faint),
            )
            .child(
                div()
                    .w(px(14.))
                    .flex_shrink_0()
                    .font_family(theme::MONO_FONT)
                    .text_size(theme::text_xs())
                    .font_weight(FontWeight::BOLD)
                    .text_color(status_color(file.status, p))
                    .child(file.status.letter()),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_size(theme::text_sm())
                    .child(
                        div()
                            .flex_shrink(1.)
                            .min_w_0()
                            .truncate()
                            .text_color(p.text_faint)
                            .child(dir),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(if seen { p.text_muted } else { p.text })
                            .child(name),
                    )
                    .when_some(file.old_path.clone(), |this, old| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .pl(theme::sp(1.5))
                                .text_color(p.text_faint)
                                .child(format!("\u{2190} {old}")),
                        )
                    }),
            )
            .when(thread_count > 0, |this| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        .gap(theme::sp(0.5))
                        .items_center()
                        .text_size(theme::text_xs())
                        .text_color(p.accent_text)
                        .child(Icon::new(IconName::MessageSquare).with_size(px(11.)))
                        .child(thread_count.to_string()),
                )
            })
            .child(counts(file.additions, file.deletions, p))
            .child(
                Checkbox::new(SharedString::from(format!("seen-{repo}-{index}")))
                    .label("Seen")
                    .checked(seen)
                    .small()
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_seen(repo, index, *checked, cx);
                    })),
            );

        let body = (!collapsed).then(|| {
            let detached: Vec<AnyElement> = slots
                .iter()
                .filter_map(|(group, slot)| match slot {
                    DiffSlot::Detached(reason) => Some(self.thread_element(
                        group,
                        Some(match reason {
                            Detached::Orphaned => AnchorBadge::Detached(Detached::Orphaned),
                            other => AnchorBadge::Detached(*other),
                        }),
                        true,
                        handlers,
                        now,
                        p,
                        cx,
                    )),
                    _ => None,
                })
                .collect();
            let notice = |text: &'static str| {
                div()
                    .px(theme::sp(3.))
                    .py(theme::sp(2.))
                    .text_size(theme::text_sm())
                    .text_color(p.text_muted)
                    .child(text)
                    .into_any_element()
            };
            if hunks_missing {
                return v_flex()
                    .children(detached)
                    .child(notice("Diff not included (past the size limit)."))
                    .into_any_element();
            }
            if file.binary {
                return v_flex()
                    .children(detached)
                    .child(notice("Binary file not shown."))
                    .into_any_element();
            }
            if file.hunks.is_empty() {
                return v_flex()
                    .children(detached)
                    .child(notice("No content changes (mode or rename only)."))
                    .into_any_element();
            }

            let mut column = v_flex()
                .flex_shrink_0()
                .min_w_full()
                .font_family(theme::MONO_FONT)
                .text_size(theme::text_xs());
            for (hunk_ix, hunk) in file.hunks.iter().enumerate() {
                let hunk_group = SharedString::from(format!("hunk-{repo}-{index}-{hunk_ix}"));
                column = column.child(
                    h_flex()
                        .id(SharedString::from(format!(
                            "diff-hunk-{repo}-{index}-{hunk_ix}"
                        )))
                        .w_full()
                        .group(hunk_group.clone())
                        .px(theme::sp(2.))
                        .py(theme::sp(0.5))
                        .gap(theme::sp(2.))
                        .bg(p.diff_hunk_bg)
                        .text_color(p.text_faint)
                        .whitespace_nowrap()
                        .child(format!("{} {}", hunk.header, hunk.section))
                        .when(commentable, |this| {
                            this.child(
                                h_flex()
                                    .id(SharedString::from(format!(
                                        "diff-hunk-comment-{repo}-{index}-{hunk_ix}"
                                    )))
                                    .gap(theme::sp(1.))
                                    .px(theme::sp(1.5))
                                    .items_center()
                                    .rounded(theme::radius_sm())
                                    .cursor_pointer()
                                    .font_family(theme::UI_FONT)
                                    .text_color(p.accent_text)
                                    .opacity(0.)
                                    .group_hover(hunk_group.clone(), |style| style.opacity(1.))
                                    .hover(|style| style.bg(p.accent_soft))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_hunk_draft((repo, index, hunk_ix), window, cx);
                                    }))
                                    .child(Icon::new(IconName::Plus).with_size(px(10.)))
                                    .child("Comment on hunk"),
                            )
                        }),
                );
                for (line_ix, line) in hunk.lines.iter().enumerate() {
                    let at = (repo, index, hunk_ix, line_ix);
                    let picked = self.selection.is_some_and(|picked| {
                        picked.in_hunk(repo, index, hunk_ix) && picked.lines().contains(&line_ix)
                    });
                    let line_group =
                        SharedString::from(format!("diff-line-{repo}-{index}-{hunk_ix}-{line_ix}"));
                    let plus = div()
                        .id(SharedString::from(format!(
                            "diff-line-comment-{repo}-{index}-{hunk_ix}-{line_ix}"
                        )))
                        .w(px(16.))
                        .h(px(16.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(theme::radius_sm())
                        .bg(p.accent)
                        .cursor_pointer()
                        .opacity(0.)
                        .when(commentable, |this| {
                            this.group_hover(line_group.clone(), |style| style.opacity(1.))
                        })
                        .tooltip(|window, cx| {
                            Tooltip::new(
                                "Comment on this line. Tip: select lines and start typing.",
                            )
                            .build(window, cx)
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                let picked = LineSelection {
                                    repo,
                                    file: index,
                                    hunk: hunk_ix,
                                    anchor: line_ix,
                                    cursor: line_ix,
                                };
                                this.selection = Some(picked);
                                this.open_lines_draft(picked, None, window, cx);
                            }),
                        )
                        .child(
                            Icon::new(IconName::Plus)
                                .with_size(px(11.))
                                .text_color(gpui_kit::white()),
                        );
                    column = column.child(
                        render_line(line, picked, p)
                            .relative()
                            .id(line_group.clone())
                            .group(line_group)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    this.press_line(at, event.modifiers.shift, window, cx);
                                }),
                            )
                            .on_mouse_move(cx.listener(
                                move |this, event: &MouseMoveEvent, _, cx| {
                                    if event.pressed_button == Some(MouseButton::Left) {
                                        this.drag_line(at, cx);
                                    }
                                },
                            ))
                            .child(div().absolute().left(px(1.)).top(px(1.)).child(plus)),
                    );
                    for (group, slot) in &slots {
                        if *slot
                            == (DiffSlot::Line {
                                hunk: hunk_ix,
                                line: line_ix,
                            })
                        {
                            column = column.child(
                                self.thread_element(group, None, false, handlers, now, p, cx),
                            );
                        }
                    }
                    if let Some(open) = &self.draft
                        && let DraftAt::Lines(picked) = open.at
                        && picked.in_hunk(repo, index, hunk_ix)
                        && *picked.lines().end() == line_ix
                    {
                        column = column.child(self.render_open_draft(open, p, cx));
                    }
                }
                for (group, slot) in &slots {
                    if *slot == DiffSlot::Hunk(hunk_ix) {
                        column = column
                            .child(self.thread_element(group, None, true, handlers, now, p, cx));
                    }
                }
                if let Some(open) = &self.draft
                    && open.at
                        == (DraftAt::Hunk {
                            repo,
                            file: index,
                            hunk: hunk_ix,
                        })
                {
                    column = column.child(self.render_open_draft(open, p, cx));
                }
            }
            // Long lines scroll sideways instead of being clipped.
            v_flex()
                .w_full()
                .children(detached)
                .child(
                    h_flex()
                        .id(SharedString::from(format!("diff-body-{repo}-{index}")))
                        .w_full()
                        .overflow_x_scroll()
                        .child(column),
                )
                .into_any_element()
        });

        v_flex()
            .w_full()
            .flex_shrink_0()
            .rounded(theme::radius())
            .border_1()
            .border_color(p.border)
            .bg(p.bg)
            .overflow_hidden()
            .child(header)
            .children(body)
    }
}

fn render_line(line: &DiffLine, picked: bool, p: &Palette) -> Div {
    let (bg, sign, text_color) = match line.kind {
        LineKind::Added => (Some(p.diff_add_bg), "+", p.text),
        LineKind::Removed => (Some(p.diff_del_bg), "-", p.text),
        LineKind::Context => (None, " ", p.text_muted),
        LineKind::NoNewline => (None, " ", p.text_faint),
    };
    let sign_color = match line.kind {
        LineKind::Added => p.diff_add_text,
        LineKind::Removed => p.diff_del_text,
        _ => p.text_faint,
    };
    let number = |n: Option<u32>| {
        div()
            .w(px(34.))
            .flex_shrink_0()
            .pr(theme::sp(1.5))
            .text_right()
            .text_color(p.text_faint)
            .child(n.map(|n| n.to_string()).unwrap_or_default())
    };
    h_flex()
        .w_full()
        .when_some(bg, |this, bg| this.bg(bg))
        .when(picked, |this| this.bg(p.accent.opacity(0.22)))
        .child(number(line.old_line))
        .child(number(line.new_line))
        .child(
            div()
                .w(px(14.))
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

/// Tabs of the right drawer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DrawerTab {
    #[default]
    Changes,
    Copies,
}

impl DrawerTab {
    fn label(self) -> &'static str {
        match self {
            DrawerTab::Changes => "Changes",
            DrawerTab::Copies => "Copies",
        }
    }
}

/// Drawer chrome: tab strip and close button around the active view.
pub fn render_drawer(
    active: DrawerTab,
    body: AnyView,
    on_tab: impl Fn(DrawerTab, &mut Window, &mut App) + 'static,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    use gpui_kit::component::{
        Disableable as _,
        button::{Button, ButtonVariants as _},
    };
    let p = theme::palette(cx);
    let on_tab = std::rc::Rc::new(on_tab);
    let tab = |tab: DrawerTab| {
        let is_active = tab == active;
        let on_tab = on_tab.clone();
        div()
            .id(SharedString::from(format!("drawer-tab-{}", tab.label())))
            .px(theme::sp(2.5))
            .py(theme::sp(1.))
            .rounded(theme::radius())
            .text_size(theme::text_sm())
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .when(is_active, |this| this.bg(p.selected).text_color(p.text))
            .when(!is_active, |this| {
                this.text_color(p.text_faint)
                    .hover(|style| style.bg(p.hover))
            })
            .on_click(move |_, window, cx| on_tab(tab, window, cx))
            .child(tab.label())
    };
    v_flex()
        .w(theme::drawer_width())
        .h_full()
        .flex_shrink_0()
        .bg(p.panel)
        .border_l_1()
        .border_color(p.border)
        .child(
            h_flex()
                .flex_shrink_0()
                .h(px(40.))
                .px(theme::sp(2.))
                .gap(theme::sp(1.))
                .items_center()
                .border_b_1()
                .border_color(p.border)
                .child(tab(DrawerTab::Changes))
                .child(tab(DrawerTab::Copies))
                .child(div().flex_1())
                .child(
                    Button::new("drawer-close")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Close drawer (\u{2318}R)")
                        .disabled(false)
                        .on_click(on_close),
                ),
        )
        .child(div().flex_1().min_h_0().child(body))
}

#[cfg(test)]
mod tests {
    use super::{HashMap, RepoView, file_key, parse_unified_diff, retain_seen, split_path};
    use crate::diff::FileStatus;
    use trama_protocol::{FileChange, FileChangeStatus, RepoChanges, RepositoryId};

    const DIFF: &str = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-x
+y
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1 +1 @@
-x
+z
";

    fn fingerprints(diff: &str) -> Vec<(String, u64)> {
        parse_unified_diff(diff)
            .iter()
            .map(|file| (file_key("repo", &file.path), file.fingerprint()))
            .collect()
    }

    #[test]
    fn seen_marks_survive_only_identical_changes() {
        let mut seen: HashMap<String, u64> = fingerprints(DIFF).into_iter().collect();
        seen.insert(file_key("repo", "deleted.rs"), 1);

        retain_seen(&mut seen, &fingerprints(&DIFF.replace("+z", "+zz")));
        assert!(
            seen.contains_key(&file_key("repo", "a.rs")),
            "unchanged file stays seen"
        );
        assert!(
            !seen.contains_key(&file_key("repo", "b.rs")),
            "changed file is unseen again"
        );
        assert!(
            !seen.contains_key(&file_key("repo", "deleted.rs")),
            "gone file is dropped"
        );
    }

    fn change(path: &str, status: FileChangeStatus, stats: Option<(u32, u32)>) -> FileChange {
        FileChange {
            path: path.into(),
            old_path: None,
            status,
            additions: stats.map(|s| s.0),
            deletions: stats.map(|s| s.1),
        }
    }

    #[test]
    fn repo_changes_take_stats_from_files_and_hunks_from_the_diff() {
        let repo = RepoChanges {
            repository_id: RepositoryId::from("r1"),
            title: "backend".into(),
            base_revision: "0123456789abcdef".into(),
            diff: DIFF.into(),
            truncated: true,
            files: vec![
                change("b.rs", FileChangeStatus::Modified, Some((1, 1))),
                change("a.rs", FileChangeStatus::Modified, Some((1, 1))),
                change("logo.png", FileChangeStatus::Added, None),
                change("late.rs", FileChangeStatus::Deleted, Some((0, 40))),
            ],
        };
        let view = RepoView::from_changes(&repo);
        assert_eq!(view.key, "r1");
        assert_eq!(view.label, "backend");
        assert!(view.truncated);
        let paths: Vec<&str> = view.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            paths,
            ["b.rs", "a.rs", "logo.png", "late.rs"],
            "order follows files"
        );
        assert_eq!(view.files[0].hunks.len(), 1);
        assert!(view.files[2].binary, "missing line stats mean binary");
        assert_eq!(view.files[2].status, FileStatus::Added);
        assert_eq!(view.files[3].status, FileStatus::Deleted);
        assert_eq!(
            view.files[3].deletions, 40,
            "stats survive a truncated diff"
        );
        assert!(view.without_hunks.contains("late.rs"));
        assert!(view.without_hunks.contains("logo.png"));
        assert!(!view.without_hunks.contains("a.rs"));
    }

    #[test]
    fn copies_and_renames_keep_their_old_path() {
        let mut renamed = change("new.rs", FileChangeStatus::Copied, Some((0, 0)));
        renamed.old_path = Some("old.rs".into());
        let repo = RepoChanges {
            repository_id: RepositoryId::from("r1"),
            title: "backend".into(),
            base_revision: String::new(),
            diff: String::new(),
            truncated: false,
            files: vec![renamed],
        };
        let view = RepoView::from_changes(&repo);
        assert_eq!(view.base_revision, None);
        assert_eq!(view.files[0].status, FileStatus::Renamed);
        assert_eq!(view.files[0].old_path.as_deref(), Some("old.rs"));
    }

    #[test]
    fn split_path_separates_directory_and_name() {
        assert_eq!(
            split_path("src/ui/app.rs"),
            ("src/ui/".to_string(), "app.rs".to_string())
        );
        assert_eq!(split_path("README"), (String::new(), "README".to_string()));
    }
}
