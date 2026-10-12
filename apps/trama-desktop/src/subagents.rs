//! Trama subagents in the UI: profile icons and task status badges (sidebar
//! rows and the thread header), live `SubagentStatusChanged` updates, the
//! tool call that spawned each child, and the transcript rows of message
//! annotations ("Subagent … started", "Sent agent message to @…").

use std::collections::HashMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, spinner::Spinner, tooltip::Tooltip};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::{MessageId, ThreadId};
use trama_domain::message::{Message, MessageAnnotation, Role};
use trama_protocol::{SubagentProfile, SubagentSnapshot, SubagentStatus, ThreadSnapshot};

use crate::chat::ChatPanel;
use crate::theme::{self, Palette};

// -- profiles and statuses ---------------------------------------------------

pub fn profile_icon(profile: SubagentProfile) -> IconName {
    match profile {
        SubagentProfile::Worker => IconName::Hammer,
        SubagentProfile::Scout => IconName::Telescope,
        SubagentProfile::Reviewer => IconName::MessageSquareDiff,
    }
}

pub fn profile_title(profile: SubagentProfile) -> &'static str {
    match profile {
        SubagentProfile::Worker => "Worker",
        SubagentProfile::Scout => "Scout",
        SubagentProfile::Reviewer => "Reviewer",
    }
}

pub fn status_label(status: SubagentStatus) -> &'static str {
    match status {
        SubagentStatus::Queued => "Queued",
        SubagentStatus::Running => "Running",
        SubagentStatus::WaitingToSync => "Waiting to sync",
        SubagentStatus::Failed => "Failed",
        SubagentStatus::Completed => "Done",
    }
}

pub fn status_color(status: SubagentStatus, p: &Palette) -> Hsla {
    match status {
        SubagentStatus::Queued => p.text_faint,
        SubagentStatus::Running => p.accent,
        SubagentStatus::WaitingToSync => p.warning,
        SubagentStatus::Failed => p.danger,
        SubagentStatus::Completed => p.success,
    }
}

fn status_icon_name(status: SubagentStatus) -> Option<IconName> {
    match status {
        SubagentStatus::Queued => Some(IconName::Clock),
        SubagentStatus::Running => None,
        SubagentStatus::WaitingToSync => Some(IconName::RefreshCw),
        SubagentStatus::Failed => Some(IconName::CircleX),
        SubagentStatus::Completed => Some(IconName::CircleCheck),
    }
}

/// The status as a small icon (a spinner while running).
pub fn status_glyph(status: SubagentStatus, size: f32, p: &Palette) -> AnyElement {
    match status_icon_name(status) {
        Some(icon) => Icon::new(icon)
            .with_size(px(size))
            .text_color(status_color(status, p))
            .into_any_element(),
        None => Spinner::new()
            .xsmall()
            .color(status_color(status, p))
            .into_any_element(),
    }
}

/// Tooltip text for a subagent: "Worker subagent · Running", plus detail.
pub fn subagent_tooltip(snapshot: &SubagentSnapshot) -> String {
    let mut text = format!(
        "{} subagent \u{00b7} {}",
        profile_title(snapshot.spawned_by.profile),
        status_label(snapshot.status)
    );
    if let Some(detail) = snapshot.detail.as_deref().filter(|d| !d.trim().is_empty()) {
        text.push_str("\n");
        text.push_str(detail.trim());
    }
    text
}

/// Sidebar decoration: the profile icon, and the status unless the task is
/// done (a finished subagent reads like any other thread).
pub fn sidebar_badge(id: &ThreadId, snapshot: &SubagentSnapshot, p: &Palette) -> AnyElement {
    let tooltip: SharedString = subagent_tooltip(snapshot).into();
    let status = snapshot.status;
    h_flex()
        .id(SharedString::from(format!("th-subagent-{id}")))
        .flex_shrink_0()
        .gap(theme::sp(1.))
        .items_center()
        .child(
            Icon::new(profile_icon(snapshot.spawned_by.profile))
                .with_size(px(12.))
                .text_color(p.text_faint),
        )
        .when(status != SubagentStatus::Completed, |this| {
            this.child(status_glyph(status, 11., p))
        })
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .into_any_element()
}

/// Header pill: profile icon and name, then the status in its color.
pub fn header_badge(snapshot: &SubagentSnapshot, p: &Palette) -> AnyElement {
    let status = snapshot.status;
    let color = status_color(status, p);
    let tooltip: Option<SharedString> = snapshot
        .detail
        .clone()
        .filter(|detail| !detail.trim().is_empty())
        .map(Into::into);
    h_flex()
        .id("subagent-status")
        .flex_shrink_0()
        .gap(theme::sp(1.5))
        .px(theme::sp(2.))
        .py(theme::sp(0.5))
        .items_center()
        .rounded(theme::radius())
        .bg(color.opacity(0.12))
        .text_size(theme::text_xs())
        .text_color(color)
        .child(status_glyph(status, 11., p))
        .child(status_label(status))
        .when_some(tooltip, |this, tooltip| {
            this.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        })
        .into_any_element()
}

/// Apply a `SubagentStatusChanged`. Returns whether a thread changed.
pub fn apply_status(
    threads: &mut [ThreadSnapshot],
    thread_id: &ThreadId,
    status: SubagentStatus,
    detail: Option<String>,
) -> bool {
    let Some(snapshot) = threads
        .iter_mut()
        .find(|thread| &thread.id == thread_id)
        .and_then(|thread| thread.subagent.as_mut())
    else {
        return false;
    };
    if snapshot.status == status && snapshot.detail == detail {
        return false;
    }
    snapshot.status = status;
    snapshot.detail = detail;
    true
}

// -- what the transcript needs to know about other threads -------------------

/// Titles, profiles and spawning tool calls of the threads the sidebar
/// lists, for annotation rows and subagent tool links.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadIndex {
    titles: HashMap<ThreadId, String>,
    profiles: HashMap<ThreadId, SubagentProfile>,
    by_tool_use: HashMap<String, ThreadId>,
    models: HashMap<ThreadId, String>,
    statuses: HashMap<ThreadId, SubagentStatus>,
}

impl ThreadIndex {
    pub fn from_threads(threads: &[ThreadSnapshot]) -> Self {
        let mut index = Self::default();
        for thread in threads {
            index.titles.insert(thread.id.clone(), thread.title.clone());
            if let Some(model) = &thread.model {
                index.models.insert(thread.id.clone(), model.clone());
            }
            if let Some(subagent) = &thread.subagent {
                index.statuses.insert(thread.id.clone(), subagent.status);
                index
                    .profiles
                    .insert(thread.id.clone(), subagent.spawned_by.profile);
                if let Some(tool_use_id) = &subagent.spawned_by.tool_use_id {
                    index
                        .by_tool_use
                        .insert(tool_use_id.clone(), thread.id.clone());
                }
            }
        }
        index
    }

    pub fn title(&self, id: &ThreadId) -> Option<&str> {
        self.titles.get(id).map(String::as_str)
    }

    pub fn profile(&self, id: &ThreadId) -> Option<SubagentProfile> {
        self.profiles.get(id).copied()
    }

    /// The thread's model override, if it has one.
    pub fn model(&self, id: &ThreadId) -> Option<&str> {
        self.models.get(id).map(String::as_str)
    }

    /// A subagent's task status.
    pub fn status(&self, id: &ThreadId) -> Option<SubagentStatus> {
        self.statuses.get(id).copied()
    }

    /// The subagent thread a `spawn_subagent` call (by Claude's tool use
    /// id) created.
    pub fn spawned_by_tool(&self, tool_use_id: &str) -> Option<&ThreadId> {
        self.by_tool_use.get(tool_use_id)
    }
}

/// Prefer the child recorded for this tool call over the guess from its
/// input and output (titles can repeat; the tool use id cannot).
pub fn link_spawned_child(
    link: &mut crate::tools::SubthreadLink,
    tool_use_id: &str,
    index: &ThreadIndex,
) {
    if link.message {
        return;
    }
    if let Some(child) = index.spawned_by_tool(tool_use_id) {
        link.thread = Some(child.clone());
        if let Some(title) = index.title(child)
            && (link.title.is_empty() || link.title == "Untitled")
        {
            link.title = title.to_string();
        }
    }
}

// -- annotation rows ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotationKind {
    SubagentStarted(SubagentProfile),
    SubagentFinished(SubagentStatus),
    Sent,
    Received,
}

/// A transcript row for an annotated message: a link to another thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationRow {
    pub message_id: MessageId,
    pub kind: AnnotationKind,
    pub thread_id: ThreadId,
    /// The other thread's title when the message was written.
    pub title: String,
    /// The agent message itself (sent / received).
    pub body: Option<String>,
}

/// The row for `message`, if it carries an annotation.
pub fn annotation_row(message: &Message) -> Option<AnnotationRow> {
    let annotation = message.annotation.as_ref()?;
    let text = message.text();
    let body = (!text.trim().is_empty()).then(|| text.trim().to_string());
    let (kind, thread_id, title, body) = match annotation {
        MessageAnnotation::SubagentSpawned {
            child_thread_id,
            title,
            profile,
        } => (
            AnnotationKind::SubagentStarted(*profile),
            child_thread_id,
            title,
            None,
        ),
        MessageAnnotation::SubagentFinished {
            child_thread_id,
            title,
            status,
        } => (
            AnnotationKind::SubagentFinished(*status),
            child_thread_id,
            title,
            // "Subagent "x" finished: <detail>." keeps the detail.
            body.and_then(|body| {
                body.split_once(": ")
                    .map(|(_, detail)| detail.trim_end_matches('.').to_string())
            }),
        ),
        MessageAnnotation::AgentMessageSent {
            to_thread_id,
            to_title,
        } => (AnnotationKind::Sent, to_thread_id, to_title, body),
        MessageAnnotation::AgentMessageReceived {
            from_thread_id,
            from_title,
        } => (AnnotationKind::Received, from_thread_id, from_title, body),
    };
    Some(AnnotationRow {
        message_id: message.id.clone(),
        kind,
        thread_id: thread_id.clone(),
        title: title.clone(),
        body,
    })
}

/// Whether the row replaces the message in the transcript. A received
/// agent message stays a prompt (it starts a turn) with its own header.
pub fn is_standalone(message: &Message) -> bool {
    message.annotation.is_some() && message.role != Role::User
}

/// The row's one-line text, e.g. "Subagent Fix parser started (worker)".
pub fn row_text(row: &AnnotationRow, index: &ThreadIndex) -> String {
    let title = display_title(row, index);
    match &row.kind {
        AnnotationKind::SubagentStarted(profile) => {
            format!("Subagent {title} started ({})", profile.label())
        }
        AnnotationKind::SubagentFinished(status) => {
            let verb = if *status == SubagentStatus::Failed {
                "failed"
            } else {
                "finished"
            };
            match index.profile(&row.thread_id) {
                Some(profile) => format!("Subagent {title} {verb} ({})", profile.label()),
                None => format!("Subagent {title} {verb}"),
            }
        }
        AnnotationKind::Sent => format!("Sent agent message to @{title}"),
        AnnotationKind::Received => format!("Received agent message from @{title}"),
    }
}

/// The thread's current title, else the one recorded with the message.
fn display_title(row: &AnnotationRow, index: &ThreadIndex) -> String {
    index
        .title(&row.thread_id)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(&row.title)
        .to_string()
}

/// A thread link: "@Title", clickable when the sidebar knows the thread.
fn thread_link(
    id: SharedString,
    row: &AnnotationRow,
    index: &ThreadIndex,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let known = index.title(&row.thread_id).is_some();
    let thread = row.thread_id.clone();
    let mention = matches!(row.kind, AnnotationKind::Sent | AnnotationKind::Received);
    let title = display_title(row, index);
    h_flex()
        .id(id)
        .min_w_0()
        .gap(theme::sp(1.))
        .items_center()
        .font_weight(FontWeight::MEDIUM)
        .text_color(if known { p.accent_text } else { p.text })
        .when(known, |this| {
            this.cursor_pointer()
                .hover(|style| style.text_color(p.accent))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_thread(thread.clone(), cx);
                }))
        })
        .child(
            div()
                .min_w_0()
                .truncate()
                .child(if mention { format!("@{title}") } else { title }),
        )
        .into_any_element()
}

/// "Sent agent message to @thread" / "Received agent message from @thread".
pub fn agent_message_header(
    ix: usize,
    row: &AnnotationRow,
    index: &ThreadIndex,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let (icon, label) = match row.kind {
        AnnotationKind::Received => (IconName::Inbox, "Received agent message from"),
        _ => (IconName::Send, "Sent agent message to"),
    };
    h_flex()
        .w_full()
        .gap(theme::sp(1.5))
        .items_center()
        .text_size(theme::text_sm())
        .child(Icon::new(icon).with_size(px(13.)).text_color(p.accent_text))
        .child(div().flex_shrink_0().text_color(p.text_muted).child(label))
        .child(thread_link(
            SharedString::from(format!("annotation-link-{ix}")),
            row,
            index,
            p,
            cx,
        ))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` brings GPUI's own `test` attribute into scope.
    use core::prelude::v1::test;
    use trama_protocol::SpawnedBy;

    fn snapshot(
        id: &str,
        title: &str,
        subagent: Option<(SubagentProfile, &str)>,
    ) -> ThreadSnapshot {
        ThreadSnapshot {
            id: ThreadId::from(id),
            title: title.into(),
            repo_count: 1,
            parent_thread_id: subagent.map(|_| ThreadId::from("parent")),
            kind: trama_protocol::ThreadKind::Sub,
            review_verdict: None,
            model: None,
            depth: 1,
            subagent: subagent.map(|(profile, tool)| SubagentSnapshot {
                spawned_by: SpawnedBy {
                    parent_thread_id: ThreadId::from("parent"),
                    tool_use_id: Some(tool.into()),
                    profile,
                },
                status: SubagentStatus::Running,
                detail: None,
            }),
            archived: false,
        }
    }

    fn annotated(role: Role, text: &str, annotation: MessageAnnotation) -> Message {
        let mut message = Message::new_system(ThreadId::from("parent"), text);
        message.role = role;
        message.annotation = Some(annotation);
        message
    }

    #[test]
    fn annotations_map_to_rows_and_text() {
        let index = ThreadIndex::from_threads(&[snapshot(
            "c1",
            "Fix parser",
            Some((SubagentProfile::Worker, "toolu_1")),
        )]);
        let spawned = annotated(
            Role::System,
            "Spawned worker subagent \"Fix parser\" (thread c1).",
            MessageAnnotation::SubagentSpawned {
                child_thread_id: ThreadId::from("c1"),
                title: "Fix parser".into(),
                profile: SubagentProfile::Worker,
            },
        );
        let row = annotation_row(&spawned).unwrap();
        assert_eq!(
            row.kind,
            AnnotationKind::SubagentStarted(SubagentProfile::Worker)
        );
        assert_eq!(row.body, None);
        assert!(is_standalone(&spawned));
        assert_eq!(
            row_text(&row, &index),
            "Subagent Fix parser started (worker)"
        );

        let finished = annotated(
            Role::System,
            "Subagent \"Fix parser\" failed: tests broke.",
            MessageAnnotation::SubagentFinished {
                child_thread_id: ThreadId::from("c1"),
                title: "Fix parser".into(),
                status: SubagentStatus::Failed,
            },
        );
        let row = annotation_row(&finished).unwrap();
        assert_eq!(row.body.as_deref(), Some("tests broke"));
        assert_eq!(
            row_text(&row, &index),
            "Subagent Fix parser failed (worker)"
        );

        let sent = annotated(
            Role::System,
            "Please also update the docs.",
            MessageAnnotation::AgentMessageSent {
                to_thread_id: ThreadId::from("gone"),
                to_title: "Docs".into(),
            },
        );
        let row = annotation_row(&sent).unwrap();
        assert_eq!(row_text(&row, &index), "Sent agent message to @Docs");
        assert_eq!(row.body.as_deref(), Some("Please also update the docs."));

        let received = annotated(
            Role::User,
            "do it",
            MessageAnnotation::AgentMessageReceived {
                from_thread_id: ThreadId::from("c1"),
                from_title: "Old title".into(),
            },
        );
        assert!(
            !is_standalone(&received),
            "a received message stays a prompt"
        );
        let row = annotation_row(&received).unwrap();
        assert_eq!(
            row_text(&row, &index),
            "Received agent message from @Fix parser",
            "the current title wins"
        );
        assert!(annotation_row(&Message::new_system(ThreadId::from("p"), "x")).is_none());
    }

    #[test]
    fn spawned_children_are_found_by_tool_use_id() {
        let threads = vec![
            snapshot(
                "c1",
                "Same title",
                Some((SubagentProfile::Scout, "toolu_a")),
            ),
            snapshot(
                "c2",
                "Same title",
                Some((SubagentProfile::Worker, "toolu_b")),
            ),
        ];
        let index = ThreadIndex::from_threads(&threads);
        let mut link = crate::tools::SubthreadLink {
            title: "Same title".into(),
            thread: Some(ThreadId::from("c1")),
            message: false,
        };
        link_spawned_child(&mut link, "toolu_b", &index);
        assert_eq!(link.thread, Some(ThreadId::from("c2")));
        let mut message = crate::tools::SubthreadLink {
            title: "x".into(),
            thread: None,
            message: true,
        };
        link_spawned_child(&mut message, "toolu_b", &index);
        assert_eq!(
            message.thread, None,
            "send_to_thread names its target itself"
        );
    }

    #[test]
    fn status_changes_apply_to_the_snapshot() {
        let mut threads = vec![snapshot("c1", "x", Some((SubagentProfile::Worker, "t")))];
        assert!(apply_status(
            &mut threads,
            &ThreadId::from("c1"),
            SubagentStatus::WaitingToSync,
            None
        ));
        assert!(!apply_status(
            &mut threads,
            &ThreadId::from("c1"),
            SubagentStatus::WaitingToSync,
            None
        ));
        assert!(!apply_status(
            &mut threads,
            &ThreadId::from("nope"),
            SubagentStatus::Failed,
            None
        ));
        let snapshot = threads[0].subagent.as_ref().unwrap();
        assert_eq!(snapshot.status, SubagentStatus::WaitingToSync);
        assert_eq!(status_label(snapshot.status), "Waiting to sync");
        assert_eq!(profile_icon(SubagentProfile::Scout), IconName::Telescope);
        assert!(subagent_tooltip(snapshot).starts_with("Worker subagent"));
    }
}
