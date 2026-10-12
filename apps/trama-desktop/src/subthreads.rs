//! Subthreads: kinds and verdict badges for the sidebar, pull and land
//! results, and the bar shown above a subthread's transcript (pull changes,
//! review verdict).

use std::collections::{HashMap, HashSet};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement as _,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::ThreadId;
use trama_protocol::{
    ClientMessage, LandOutcome, PullFileOutcome, RepoPullResult, ReviewVerdict, ThreadKind,
    ThreadSnapshot,
};

use crate::net::{DaemonClient, SendStatus};
use crate::sidebar::{SidebarPanel, nest_threads};
use crate::theme::{self, Palette};

pub fn kind_icon(kind: ThreadKind) -> Option<IconName> {
    match kind {
        ThreadKind::Top => None,
        ThreadKind::Sub | ThreadKind::Fork => Some(IconName::Split),
        ThreadKind::Review => Some(IconName::MessageSquareDiff),
        ThreadKind::Land => Some(IconName::PlaneLanding),
    }
}

pub fn kind_label(kind: ThreadKind) -> &'static str {
    match kind {
        ThreadKind::Top => "Thread",
        ThreadKind::Sub => "Subthread",
        ThreadKind::Fork => "Fork",
        ThreadKind::Review => "Review",
        ThreadKind::Land => "Land",
    }
}

/// Badge for a review subthread's latest verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictBadge {
    Approved,
    ChangesRequested,
}

impl VerdictBadge {
    pub fn from_verdict(verdict: Option<&ReviewVerdict>) -> Option<Self> {
        match verdict? {
            ReviewVerdict::Approve => Some(Self::Approved),
            ReviewVerdict::RequestChanges { .. } => Some(Self::ChangesRequested),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Approved => "Approved",
            Self::ChangesRequested => "Changes requested",
        }
    }

    pub fn color(self, p: &Palette) -> Hsla {
        match self {
            Self::Approved => p.success,
            Self::ChangesRequested => p.warning,
        }
    }
}

/// One sidebar row: which snapshot, how deep, and its decorations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub index: usize,
    pub depth: usize,
    pub kind: ThreadKind,
    pub verdict: Option<VerdictBadge>,
}

/// Threads in display order with subthreads under their parent.
pub fn thread_rows(threads: &[ThreadSnapshot]) -> Vec<ThreadRow> {
    let pairs: Vec<(ThreadId, Option<ThreadId>)> = threads
        .iter()
        .map(|thread| (thread.id.clone(), thread.parent_thread_id.clone()))
        .collect();
    nest_threads(&pairs)
        .into_iter()
        .map(|(index, depth)| {
            let thread = &threads[index];
            ThreadRow {
                index,
                depth,
                kind: thread.kind,
                verdict: VerdictBadge::from_verdict(thread.review_verdict.as_ref()),
            }
        })
        .collect()
}

/// The top-level thread new subthreads hang from: the selection itself, or
/// its parent when a subthread is selected (subthreads do not nest).
pub fn subthread_parent(threads: &[ThreadSnapshot], selected: &ThreadId) -> Option<ThreadId> {
    let thread = threads.iter().find(|thread| &thread.id == selected)?;
    match &thread.parent_thread_id {
        Some(parent) => Some(parent.clone()),
        None => Some(thread.id.clone()),
    }
}

/// Short title for a subthread started from a draft.
pub fn title_from_prompt(prompt: &str) -> String {
    let line = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let line = line.trim();
    let mut title: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        title.push('…');
    }
    title
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PullSummary {
    pub applied: usize,
    pub merged: usize,
    pub unchanged: usize,
    pub conflicts: usize,
}

impl PullSummary {
    pub fn from_repos(repos: &[RepoPullResult]) -> Self {
        let mut summary = Self::default();
        for file in repos.iter().flat_map(|repo| &repo.files) {
            match file.outcome {
                PullFileOutcome::Applied => summary.applied += 1,
                PullFileOutcome::Merged => summary.merged += 1,
                PullFileOutcome::Unchanged => summary.unchanged += 1,
                PullFileOutcome::Conflict { .. } => summary.conflicts += 1,
            }
        }
        summary
    }

    pub fn text(&self) -> String {
        if self.applied + self.merged + self.unchanged + self.conflicts == 0 {
            return "Nothing to pull: the subthread has no changes.".into();
        }
        let mut parts = Vec::new();
        if self.applied > 0 {
            parts.push(format!("{} applied", self.applied));
        }
        if self.merged > 0 {
            parts.push(format!("{} merged", self.merged));
        }
        if self.unchanged > 0 {
            parts.push(format!("{} unchanged", self.unchanged));
        }
        if self.conflicts > 0 {
            parts.push(format!(
                "{} conflict{}",
                self.conflicts,
                if self.conflicts == 1 { "" } else { "s" }
            ));
        }
        parts.join(", ")
    }
}

pub fn outcome_label(outcome: &PullFileOutcome) -> &'static str {
    match outcome {
        PullFileOutcome::Applied => "Applied",
        PullFileOutcome::Merged => "Merged",
        PullFileOutcome::Unchanged => "Unchanged",
        PullFileOutcome::Conflict {
            markers_written: true,
            ..
        } => "Conflict (markers written)",
        PullFileOutcome::Conflict { .. } => "Conflict (left as is)",
    }
}

pub fn land_label(outcome: LandOutcome) -> &'static str {
    match outcome {
        LandOutcome::Landed => "Landed",
        LandOutcome::Failed => "Land failed",
        LandOutcome::Proposed => "Land skill proposed",
    }
}

/// What the bar shows for the selected thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarModel {
    pub thread_id: ThreadId,
    pub kind: ThreadKind,
    pub parent_id: ThreadId,
    pub parent_title: String,
    pub verdict: Option<ReviewVerdict>,
}

/// `None` for top-level threads: the bar only exists inside subthreads.
pub fn bar_model(threads: &[ThreadSnapshot], selected: Option<&ThreadId>) -> Option<BarModel> {
    let thread = threads.iter().find(|thread| Some(&thread.id) == selected)?;
    let parent_id = thread.parent_thread_id.clone()?;
    let parent_title = threads
        .iter()
        .find(|candidate| candidate.id == parent_id)
        .map(|parent| parent.title.clone())
        .unwrap_or_else(|| "parent thread".into());
    Some(BarModel {
        thread_id: thread.id.clone(),
        kind: thread.kind,
        parent_id,
        parent_title,
        verdict: thread.review_verdict.clone(),
    })
}

pub struct SubthreadBar {
    client: DaemonClient,
    sidebar: Entity<SidebarPanel>,
    pulls: HashMap<ThreadId, Vec<RepoPullResult>>,
    pulling: HashSet<ThreadId>,
    lands: HashMap<ThreadId, (LandOutcome, String)>,
    verdict_pending: HashSet<ThreadId>,
    requesting_changes: bool,
    summary: Entity<InputState>,
    clear_summary: bool,
    error: Option<String>,
    _summary_enter: Subscription,
}

impl SubthreadBar {
    pub fn new(
        client: DaemonClient,
        sidebar: Entity<SidebarPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let summary = cx.new(|cx| {
            InputState::new(window, cx).placeholder("What should change? (sent to the parent)")
        });
        let summary_enter = cx.subscribe_in(&summary, window, |this, _, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.submit_request_changes(cx);
            }
        });
        Self {
            client,
            sidebar,
            pulls: HashMap::new(),
            pulling: HashSet::new(),
            lands: HashMap::new(),
            verdict_pending: HashSet::new(),
            requesting_changes: false,
            summary,
            clear_summary: false,
            error: None,
            _summary_enter: summary_enter,
        }
    }

    fn model(&self, cx: &App) -> Option<BarModel> {
        let onboarding = &self.sidebar.read(cx).onboarding;
        bar_model(&onboarding.threads, onboarding.selected_thread.as_ref())
    }

    /// The selection changed: drop transient form state.
    pub fn reset_form(&mut self, cx: &mut Context<Self>) {
        self.requesting_changes = false;
        self.clear_summary = true;
        self.error = None;
        cx.notify();
    }

    pub fn pull(&mut self, subthread: ThreadId, cx: &mut Context<Self>) {
        if self.pulling.contains(&subthread) {
            return;
        }
        let message = ClientMessage::PullChanges {
            subthread_id: subthread.clone(),
        };
        if self.client.send(message) == SendStatus::Rejected {
            self.error = Some("The daemon is not connected; nothing was pulled.".into());
        } else {
            self.error = None;
            self.pulling.insert(subthread);
        }
        cx.notify();
    }

    pub fn apply_pull(
        &mut self,
        subthread: ThreadId,
        repos: Vec<RepoPullResult>,
        cx: &mut Context<Self>,
    ) {
        self.pulling.remove(&subthread);
        self.pulls.insert(subthread, repos);
        cx.notify();
    }

    pub fn apply_land(
        &mut self,
        land_thread: ThreadId,
        outcome: LandOutcome,
        summary: String,
        cx: &mut Context<Self>,
    ) {
        self.lands.insert(land_thread, (outcome, summary));
        cx.notify();
    }

    pub fn apply_verdict(&mut self, review_thread: &ThreadId, cx: &mut Context<Self>) {
        self.verdict_pending.remove(review_thread);
        if self.model(cx).map(|model| model.thread_id).as_ref() == Some(review_thread) {
            self.requesting_changes = false;
            self.clear_summary = true;
        }
        cx.notify();
    }

    /// A daemon error ends whatever this bar was waiting for.
    pub fn note_error(&mut self, cx: &mut Context<Self>) {
        if !self.pulling.is_empty() || !self.verdict_pending.is_empty() {
            self.pulling.clear();
            self.verdict_pending.clear();
            cx.notify();
        }
    }

    fn submit_verdict(&mut self, verdict: ReviewVerdict, cx: &mut Context<Self>) {
        let Some(model) = self.model(cx) else {
            return;
        };
        if model.kind != ThreadKind::Review || self.verdict_pending.contains(&model.thread_id) {
            return;
        }
        let message = ClientMessage::SubmitReviewVerdict {
            review_thread_id: model.thread_id.clone(),
            verdict,
        };
        if self.client.send(message) == SendStatus::Rejected {
            self.error = Some("The daemon is not connected; the verdict was not sent.".into());
        } else {
            self.error = None;
            self.verdict_pending.insert(model.thread_id);
        }
        cx.notify();
    }

    fn submit_request_changes(&mut self, cx: &mut Context<Self>) {
        let summary = self.summary.read(cx).value().trim().to_string();
        if summary.is_empty() {
            self.error = Some("Describe what should change before sending.".into());
            cx.notify();
            return;
        }
        self.submit_verdict(ReviewVerdict::RequestChanges { summary }, cx);
    }

    fn repo_title(&self, id: &trama_protocol::RepositoryId, cx: &App) -> String {
        self.sidebar
            .read(cx)
            .onboarding
            .repositories
            .iter()
            .find(|repo| &repo.id == id)
            .map(|repo| repo.title.clone())
            .unwrap_or_else(|| id.to_string())
    }
}

impl Render for SubthreadBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.clear_summary {
            self.clear_summary = false;
            self.summary
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        let Some(model) = self.model(cx) else {
            return div().into_any_element();
        };
        let p = theme::palette(cx);
        let connected = self.sidebar.read(cx).onboarding.connected();
        let thread_id = model.thread_id.clone();
        let pulling = self.pulling.contains(&thread_id);
        let verdict_pending = self.verdict_pending.contains(&thread_id);
        let is_review = model.kind == ThreadKind::Review;
        let badge = VerdictBadge::from_verdict(model.verdict.as_ref());

        let parent_id = model.parent_id.clone();
        let sidebar = self.sidebar.clone();
        let parent_link = h_flex()
            .id("subthread-parent")
            .gap(theme::sp(1.5))
            .items_center()
            .min_w_0()
            .text_size(theme::text_sm())
            .text_color(p.text_muted)
            .cursor_pointer()
            .hover(|style| style.text_color(p.text))
            .on_click(move |_, _, cx| {
                let parent = parent_id.clone();
                sidebar.update(cx, |sidebar, cx| sidebar.select_thread(parent, cx));
            })
            .children(
                kind_icon(model.kind)
                    .map(|icon| Icon::new(icon).with_size(px(13.)).text_color(p.accent_text)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.text)
                    .child(kind_label(model.kind)),
            )
            .child("of")
            .child(div().min_w_0().truncate().child(model.parent_title.clone()));

        let pull_thread = thread_id.clone();
        let mut actions = h_flex()
            .flex_shrink_0()
            .gap(theme::sp(1.))
            .items_center()
            .when_some(badge, |this, badge| {
                this.child(
                    div()
                        .px(theme::sp(2.))
                        .py(theme::sp(0.5))
                        .rounded(theme::radius())
                        .text_size(theme::text_xs())
                        .text_color(badge.color(&p))
                        .bg(badge.color(&p).opacity(0.12))
                        .child(badge.label()),
                )
            });
        if is_review {
            actions = actions
                .child(
                    Button::new("review-approve")
                        .small()
                        .primary()
                        .icon(IconName::Check)
                        .label("Approve")
                        .loading(verdict_pending && !self.requesting_changes)
                        .disabled(!connected || verdict_pending)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.submit_verdict(ReviewVerdict::Approve, cx);
                        })),
                )
                .child(
                    Button::new("review-request-changes")
                        .small()
                        .ghost()
                        .label("Request changes")
                        .toggled(self.requesting_changes)
                        .disabled(!connected || verdict_pending)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.requesting_changes = !this.requesting_changes;
                            if this.requesting_changes {
                                this.summary.update(cx, |input, cx| input.focus(window, cx));
                            }
                            cx.notify();
                        })),
                );
        }
        actions = actions.child(
            Button::new("subthread-pull")
                .small()
                .ghost()
                .icon(IconName::GitMerge)
                .label("Pull changes into parent")
                .tooltip(
                    "Apply what this subthread changed to the parent's checkouts. Never commits.",
                )
                .loading(pulling)
                .disabled(!connected || pulling)
                .on_click(cx.listener(move |this, _, _, cx| this.pull(pull_thread.clone(), cx))),
        );

        let mut bar = v_flex()
            .w_full()
            .flex_shrink_0()
            .px(theme::sp(4.))
            .py(theme::sp(2.))
            .gap(theme::sp(2.))
            .bg(p.panel)
            .border_b_1()
            .border_color(p.border)
            .child(
                h_flex()
                    .w_full()
                    .gap(theme::sp(3.))
                    .items_center()
                    .child(div().flex_1().min_w_0().child(parent_link))
                    .child(actions),
            );

        if is_review && self.requesting_changes {
            bar = bar.child(
                h_flex()
                    .w_full()
                    .gap(theme::sp(2.))
                    .items_center()
                    .child(div().flex_1().child(Input::new(&self.summary).small()))
                    .child(
                        Button::new("review-send-changes")
                            .small()
                            .primary()
                            .label("Send")
                            .loading(verdict_pending)
                            .disabled(!connected || verdict_pending)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.submit_request_changes(cx);
                            })),
                    ),
            );
        }

        if let Some((outcome, summary)) = self.lands.get(&thread_id) {
            let color = match outcome {
                LandOutcome::Landed => p.success,
                LandOutcome::Failed => p.danger,
                LandOutcome::Proposed => p.warning,
            };
            bar = bar.child(
                h_flex()
                    .gap(theme::sp(2.))
                    .items_start()
                    .text_size(theme::text_sm())
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(color)
                            .child(land_label(*outcome)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(p.text_muted)
                            .child(summary.clone()),
                    ),
            );
        }

        if let Some(repos) = self.pulls.get(&thread_id) {
            let summary = PullSummary::from_repos(repos);
            let mut rows = v_flex()
                .id("pull-results")
                .w_full()
                .max_h(px(180.))
                .overflow_y_scrollbar()
                .gap(px(2.))
                .font_family(theme::MONO_FONT)
                .text_size(theme::text_xs());
            for repo in repos {
                if repos.len() > 1 {
                    rows = rows.child(
                        div()
                            .pt(theme::sp(1.))
                            .font_family(theme::UI_FONT)
                            .text_color(p.text_faint)
                            .child(self.repo_title(&repo.repository_id, cx)),
                    );
                }
                for file in &repo.files {
                    let conflict = matches!(file.outcome, PullFileOutcome::Conflict { .. });
                    let reason = match &file.outcome {
                        PullFileOutcome::Conflict { reason, .. } => Some(reason.clone()),
                        _ => None,
                    };
                    rows = rows.child(
                        h_flex()
                            .w_full()
                            .px(theme::sp(2.))
                            .py(px(2.))
                            .gap(theme::sp(2.))
                            .rounded(theme::radius_sm())
                            .when(conflict, |this| this.bg(p.danger.opacity(0.10)))
                            .child(
                                div()
                                    .w(px(150.))
                                    .flex_shrink_0()
                                    .font_family(theme::UI_FONT)
                                    .text_color(if conflict { p.danger } else { p.text_faint })
                                    .child(outcome_label(&file.outcome)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(if conflict { p.text } else { p.text_muted })
                                    .child(file.path.clone()),
                            )
                            .when_some(reason, |this, reason| {
                                this.child(
                                    div()
                                        .max_w(px(260.))
                                        .truncate()
                                        .font_family(theme::UI_FONT)
                                        .text_color(p.text_muted)
                                        .child(reason),
                                )
                            }),
                    );
                }
            }
            bar = bar.child(
                v_flex()
                    .w_full()
                    .gap(theme::sp(1.))
                    .child(
                        h_flex()
                            .gap(theme::sp(2.))
                            .items_center()
                            .text_size(theme::text_sm())
                            .child(
                                Icon::new(if summary.conflicts > 0 {
                                    IconName::TriangleAlert
                                } else {
                                    IconName::Check
                                })
                                .with_size(px(13.))
                                .text_color(
                                    if summary.conflicts > 0 {
                                        p.danger
                                    } else {
                                        p.success
                                    },
                                ),
                            )
                            .child(
                                div()
                                    .text_color(p.text)
                                    .child(format!("Pulled into parent: {}", summary.text())),
                            ),
                    )
                    .child(rows),
            );
        }

        if let Some(error) = self.error.clone() {
            bar = bar.child(
                div()
                    .text_size(theme::text_sm())
                    .text_color(p.danger)
                    .child(error),
            );
        }

        bar.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IconName, PullSummary, ThreadId, VerdictBadge, bar_model, kind_icon, outcome_label,
        subthread_parent, thread_rows, title_from_prompt,
    };
    use trama_protocol::{
        FileChangeStatus, PullFileOutcome, PullFileResult, RepoPullResult, RepositoryId,
        ReviewVerdict, ThreadKind, ThreadSnapshot,
    };

    fn snapshot(id: &str, parent: Option<&str>, kind: ThreadKind) -> ThreadSnapshot {
        ThreadSnapshot {
            id: ThreadId::from(id),
            title: id.to_uppercase(),
            repo_count: 1,
            parent_thread_id: parent.map(ThreadId::from),
            kind,
            review_verdict: None,
            model: None,
            depth: 0,
            subagent: None,
        }
    }

    #[test]
    fn subthreads_nest_under_their_parent_with_kind_and_verdict() {
        let mut review = snapshot("r", Some("a"), ThreadKind::Review);
        review.review_verdict = Some(ReviewVerdict::RequestChanges {
            summary: "tests".into(),
        });
        let threads = vec![
            snapshot("a", None, ThreadKind::Top),
            snapshot("b", None, ThreadKind::Top),
            snapshot("s", Some("b"), ThreadKind::Sub),
            review,
            snapshot("l", Some("a"), ThreadKind::Land),
        ];
        let rows = thread_rows(&threads);
        let order: Vec<(String, usize)> = rows
            .iter()
            .map(|row| (threads[row.index].id.to_string(), row.depth))
            .collect();
        let expected: Vec<(String, usize)> = [("a", 0), ("r", 1), ("l", 1), ("b", 0), ("s", 1)]
            .into_iter()
            .map(|(id, depth)| (id.to_string(), depth))
            .collect();
        assert_eq!(order, expected);
        assert_eq!(rows[1].kind, ThreadKind::Review);
        assert_eq!(rows[1].verdict, Some(VerdictBadge::ChangesRequested));
        assert_eq!(rows[2].verdict, None);
        assert_eq!(kind_icon(ThreadKind::Top), None);
        assert_eq!(kind_icon(ThreadKind::Land), Some(IconName::PlaneLanding));
    }

    #[test]
    fn new_subthreads_hang_from_the_top_level_thread() {
        let threads = vec![
            snapshot("a", None, ThreadKind::Top),
            snapshot("s", Some("a"), ThreadKind::Sub),
        ];
        assert_eq!(
            subthread_parent(&threads, &ThreadId::from("a")),
            Some(ThreadId::from("a"))
        );
        assert_eq!(
            subthread_parent(&threads, &ThreadId::from("s")),
            Some(ThreadId::from("a"))
        );
        assert_eq!(subthread_parent(&threads, &ThreadId::from("zz")), None);
    }

    #[test]
    fn bar_appears_only_for_subthreads() {
        let threads = vec![
            snapshot("a", None, ThreadKind::Top),
            snapshot("r", Some("a"), ThreadKind::Review),
        ];
        assert_eq!(bar_model(&threads, Some(&ThreadId::from("a"))), None);
        let model = bar_model(&threads, Some(&ThreadId::from("r"))).unwrap();
        assert_eq!(model.kind, ThreadKind::Review);
        assert_eq!(model.parent_title, "A");
        assert_eq!(bar_model(&threads, None), None);
    }

    #[test]
    fn pull_summary_counts_outcomes_and_flags_conflicts() {
        let file = |path: &str, outcome| PullFileResult {
            path: path.into(),
            change: FileChangeStatus::Modified,
            outcome,
        };
        let repos = vec![RepoPullResult {
            repository_id: RepositoryId::from("r"),
            files: vec![
                file("a", PullFileOutcome::Applied),
                file("b", PullFileOutcome::Merged),
                file(
                    "c",
                    PullFileOutcome::Conflict {
                        markers_written: true,
                        reason: "both changed".into(),
                    },
                ),
            ],
        }];
        let summary = PullSummary::from_repos(&repos);
        assert_eq!(summary.conflicts, 1);
        assert_eq!(summary.text(), "1 applied, 1 merged, 1 conflict");
        assert_eq!(
            PullSummary::from_repos(&[]).text(),
            "Nothing to pull: the subthread has no changes."
        );
        assert_eq!(
            outcome_label(&repos[0].files[2].outcome),
            "Conflict (markers written)"
        );
    }

    #[test]
    fn titles_from_prompts_use_the_first_line() {
        assert_eq!(
            title_from_prompt("\n  Fix the parser\nmore"),
            "Fix the parser"
        );
        let long = "x".repeat(80);
        assert_eq!(title_from_prompt(&long).chars().count(), 61);
    }
}
