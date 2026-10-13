//! Disk used by managed thread checkouts (⌘K "Disk usage", Manage projects).
//!
//! Checkouts are copy-on-write clones, so the size `du` reports (apparent)
//! overstates what they cost; the daemon also reports the private bytes that
//! removing a checkout would free. Rows are sorted by private size and offer
//! Archive and Delete copy. When the daemon refuses because a checkout holds
//! work that is not in the source repository, the workspace shows what would
//! be lost and resends with `force` on confirmation.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_protocol::{CheckoutRemoval, ClientMessage, DiskUsageItem, ThreadId};

use crate::app::theme::{self, Palette};

/// What a removal request asked for, kept to resend it with `force`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalKind {
    /// Archive the thread and remove its checkouts.
    Archive,
    /// Remove the checkouts only; the thread stays active.
    DeleteCopy,
}

impl RemovalKind {
    pub fn message(self, thread_id: ThreadId, force: bool) -> ClientMessage {
        match self {
            Self::Archive => ClientMessage::ArchiveThread {
                thread_id,
                delete_checkouts: true,
                force,
            },
            Self::DeleteCopy => ClientMessage::DeleteThreadCheckouts { thread_id, force },
        }
    }

    pub fn confirm_title(self) -> &'static str {
        match self {
            Self::Archive => "Archive this thread anyway?",
            Self::DeleteCopy => "Delete this thread's copy anyway?",
        }
    }

    pub fn confirm_label(self) -> &'static str {
        match self {
            Self::Archive => "Archive anyway",
            Self::DeleteCopy => "Delete anyway",
        }
    }
}

/// `1536` -> `1.5 KB` (binary units, one decimal from KB up).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Private size, prefixed with `~` when the volume could only estimate it.
pub fn private_label(item: &DiskUsageItem) -> String {
    let size = format_bytes(item.private_bytes);
    if item.private_estimated {
        format!("~{size}")
    } else {
        size
    }
}

/// Largest private size first; ties by apparent size, then thread title.
pub fn sort_by_private(items: &mut [DiskUsageItem]) {
    items.sort_by(|a, b| {
        b.private_bytes
            .cmp(&a.private_bytes)
            .then(b.apparent_bytes.cmp(&a.apparent_bytes))
            .then_with(|| a.thread_title.cmp(&b.thread_title))
    });
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub count: usize,
    pub private_bytes: u64,
    pub apparent_bytes: u64,
    /// Some private sizes are estimates.
    pub estimated: bool,
}

pub fn totals(items: &[DiskUsageItem]) -> Totals {
    items.iter().fold(Totals::default(), |acc, item| Totals {
        count: acc.count + 1,
        private_bytes: acc.private_bytes.saturating_add(item.private_bytes),
        apparent_bytes: acc.apparent_bytes.saturating_add(item.apparent_bytes),
        estimated: acc.estimated || item.private_estimated,
    })
}

/// `3 checkouts · 1.2 GB private of 40.0 GB apparent`.
pub fn totals_label(totals: &Totals) -> String {
    format!(
        "{} checkout{} \u{b7} {}{} private of {} apparent",
        totals.count,
        if totals.count == 1 { "" } else { "s" },
        if totals.estimated { "~" } else { "" },
        format_bytes(totals.private_bytes),
        format_bytes(totals.apparent_bytes),
    )
}

/// Coarse age of the thread's last activity.
pub fn last_active_label(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(at) = at else {
        return "no activity".into();
    };
    let seconds = (now - at).num_seconds().max(0);
    match seconds {
        0..60 => "active just now".into(),
        60..3_600 => format!("active {} min ago", seconds / 60),
        3_600..86_400 => format!("active {} h ago", seconds / 3_600),
        _ => format!("active {} d ago", seconds / 86_400),
    }
}

/// Bytes freed by a completed removal, when the daemon measured them.
pub fn freed_total(removed: &[CheckoutRemoval]) -> Option<u64> {
    let sizes: Vec<u64> = removed.iter().filter_map(|row| row.freed_bytes).collect();
    (!sizes.is_empty()).then(|| sizes.iter().sum())
}

/// What a forced removal would lose, one paragraph per checkout with work
/// that is not in the source repository.
pub fn refusal_details(checkouts: &[CheckoutRemoval]) -> String {
    let mut parts = Vec::new();
    for checkout in checkouts.iter().filter(|row| row.has_unsaved_work()) {
        let mut lines = vec![format!("{} ({})", checkout.title, checkout.path.display())];
        if checkout.changed_file_count > 0 {
            let shown = checkout.changed_files.len() as u32;
            let mut files = checkout.changed_files.join(", ");
            if checkout.changed_file_count > shown {
                files.push_str(&format!(
                    " and {} more",
                    checkout.changed_file_count - shown
                ));
            }
            lines.push(format!(
                "{} uncommitted or untracked file{}: {files}",
                checkout.changed_file_count,
                if checkout.changed_file_count == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        if checkout.commits_not_in_source > 0 {
            lines.push(format!(
                "{} commit{} not in the source repository",
                checkout.commits_not_in_source,
                if checkout.commits_not_in_source == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        parts.push(lines.join("\n"));
    }
    if parts.is_empty() {
        "The daemon reported unsaved work in this thread's checkouts.".into()
    } else {
        parts.join("\n\n")
    }
}

pub enum DiskUsageEvent {
    Refresh,
    Remove(ThreadId, RemovalKind),
}

/// The "Disk usage" dialog body.
pub struct DiskUsageView {
    items: Vec<DiskUsageItem>,
    loaded: bool,
    error: Option<String>,
    /// Threads with a removal in flight.
    pending: HashSet<ThreadId>,
    /// The dialog is open: refresh after archive/delete replies.
    pub visible: bool,
}

impl EventEmitter<DiskUsageEvent> for DiskUsageView {}

impl DiskUsageView {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            loaded: false,
            error: None,
            pending: HashSet::new(),
            visible: false,
        }
    }

    /// Back to loading (a `GetDiskUsage` was just sent).
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.loaded = false;
        self.error = None;
        cx.notify();
    }

    pub fn apply(&mut self, mut items: Vec<DiskUsageItem>, cx: &mut Context<Self>) {
        sort_by_private(&mut items);
        self.items = items;
        self.loaded = true;
        self.error = None;
        cx.notify();
    }

    /// A daemon error while loading; `false` when nothing was loading.
    pub fn fail_loading(&mut self, message: &str, cx: &mut Context<Self>) -> bool {
        if !self.visible || self.loaded {
            return false;
        }
        self.loaded = true;
        self.error = Some(message.to_string());
        cx.notify();
        true
    }

    pub fn set_pending(&mut self, thread_id: &ThreadId, pending: bool, cx: &mut Context<Self>) {
        let changed = if pending {
            self.pending.insert(thread_id.clone())
        } else {
            self.pending.remove(thread_id)
        };
        if changed {
            cx.notify();
        }
    }

    pub fn clear_pending(&mut self, cx: &mut Context<Self>) {
        if !self.pending.is_empty() {
            self.pending.clear();
            cx.notify();
        }
    }
}

impl Default for DiskUsageView {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for DiskUsageView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let totals = totals(&self.items);
        let now = Utc::now();
        let header = h_flex()
            .gap(theme::sp(2.))
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_size(theme::text_sm())
                    .text_color(p.text_muted)
                    .child(if self.loaded {
                        totals_label(&totals)
                    } else {
                        "Measuring checkouts\u{2026}".into()
                    }),
            )
            .child(
                Button::new("disk-usage-refresh")
                    .ghost()
                    .xsmall()
                    .icon(IconName::RefreshCw)
                    .tooltip("Measure again")
                    .disabled(!self.loaded)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DiskUsageEvent::Refresh))),
            );
        let note = small(
            "Checkouts are copy-on-write clones: private is what removing a copy frees, apparent is what du reports. Archived threads keep their history; removed copies are recreated when needed.",
            p.text_faint,
        );
        let mut list = v_flex()
            .id("disk-usage-list")
            .max_h(px(420.))
            .overflow_y_scrollbar()
            .gap(theme::sp(1.5));
        if let Some(error) = &self.error {
            list = list.child(small(error.clone(), p.danger));
        } else if self.loaded && self.items.is_empty() {
            list = list.child(small("No thread checkouts on disk.", p.text_muted));
        }
        for (index, item) in self.items.iter().enumerate() {
            list = list.child(self.row(index, item, now, &p, cx));
        }
        v_flex()
            .w_full()
            .gap(theme::sp(2.))
            .pb(theme::sp(1.))
            .child(header)
            .child(note)
            .child(list)
    }
}

impl DiskUsageView {
    fn row(
        &self,
        index: usize,
        item: &DiskUsageItem,
        now: DateTime<Utc>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pending = self.pending.contains(&item.thread_id);
        let archive_id = item.thread_id.clone();
        let delete_id = item.thread_id.clone();
        v_flex()
            .gap(theme::sp(0.5))
            .p(theme::sp(2.5))
            .rounded(theme::radius())
            .border_1()
            .border_color(p.border)
            .bg(p.bg)
            .child(
                h_flex()
                    .gap(theme::sp(2.))
                    .items_center()
                    .text_size(theme::text_sm())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.text)
                            .child(item.thread_title.clone()),
                    )
                    .when(item.archived, |this| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .px(theme::sp(1.5))
                                .rounded(theme::radius_sm())
                                .border_1()
                                .border_color(p.border)
                                .text_size(theme::text_xs())
                                .text_color(p.text_faint)
                                .child("Archived"),
                        )
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.text)
                            .child(private_label(item)),
                    ),
            )
            .child(
                h_flex()
                    .gap(theme::sp(2.))
                    .text_size(theme::text_xs())
                    .text_color(p.text_muted)
                    .child(div().flex_1().min_w_0().truncate().child(format!(
                        "{} \u{b7} {}",
                        item.repository_title,
                        last_active_label(item.last_active, now)
                    )))
                    .child(div().flex_shrink_0().child(format!(
                        "{} apparent{}",
                        format_bytes(item.apparent_bytes),
                        if item.private_estimated {
                            " \u{b7} private estimated"
                        } else {
                            ""
                        }
                    ))),
            )
            .child(
                div()
                    .truncate()
                    .font_family(theme::MONO_FONT)
                    .text_size(theme::text_xs())
                    .text_color(p.text_faint)
                    .child(item.path.display().to_string()),
            )
            .child(
                h_flex()
                    .gap(theme::sp(1.))
                    .pt(theme::sp(1.))
                    .child(
                        Button::new(SharedString::from(format!("disk-archive-{index}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Archive)
                            .label("Archive")
                            .disabled(pending || item.archived)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(DiskUsageEvent::Remove(
                                    archive_id.clone(),
                                    RemovalKind::Archive,
                                ));
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("disk-delete-{index}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Trash)
                            .label("Delete copy")
                            .disabled(pending)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(DiskUsageEvent::Remove(
                                    delete_id.clone(),
                                    RemovalKind::DeleteCopy,
                                ));
                            })),
                    )
                    .when(pending, |this| {
                        this.child(small("Working\u{2026}", p.text_faint))
                    }),
            )
    }
}

fn small(text: impl Into<SharedString>, color: Hsla) -> impl IntoElement {
    div()
        .text_size(theme::text_xs())
        .text_color(color)
        .child(text.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use std::path::PathBuf;
    use trama_protocol::RepositoryId;

    fn item(title: &str, private: u64, apparent: u64, estimated: bool) -> DiskUsageItem {
        DiskUsageItem {
            thread_id: ThreadId::from(title),
            thread_title: title.into(),
            repository_id: RepositoryId::from("repo"),
            repository_title: "app".into(),
            path: PathBuf::from(format!("/src/app/.trama/worktrees/{title}/c")),
            apparent_bytes: apparent,
            private_bytes: private,
            private_estimated: estimated,
            last_active: None,
            archived: false,
        }
    }

    fn removal(changed: &[&str], count: u32, commits: u32) -> CheckoutRemoval {
        CheckoutRemoval {
            repository_id: RepositoryId::from("repo"),
            title: "app".into(),
            path: PathBuf::from("/src/app/.trama/worktrees/t/c"),
            changed_files: changed.iter().map(|file| file.to_string()).collect(),
            changed_file_count: count,
            commits_not_in_source: commits,
            freed_bytes: Some(2048),
        }
    }

    #[test]
    fn bytes_use_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(20 << 30), "20.0 GB");
        assert_eq!(format_bytes(5 << 40), "5.0 TB");
    }

    #[test]
    fn rows_sort_by_private_size_and_totals_add_up() {
        let mut items = vec![
            item("small", 10, 4_000, false),
            item("big", 3 << 20, 4_000, false),
            item("tie-b", 10, 9_000, true),
        ];
        sort_by_private(&mut items);
        let order: Vec<_> = items.iter().map(|i| i.thread_title.as_str()).collect();
        assert_eq!(order, ["big", "tie-b", "small"]);
        let sum = totals(&items);
        assert_eq!(sum.count, 3);
        assert_eq!(sum.private_bytes, (3 << 20) + 20);
        assert_eq!(sum.apparent_bytes, 17_000);
        assert!(sum.estimated);
        assert!(totals_label(&sum).starts_with("3 checkouts \u{b7} ~3.0 MB private"));
        assert_eq!(private_label(&items[1]), "~10 B");
        assert_eq!(private_label(&items[0]), "3.0 MB");
        assert_eq!(totals(&[]), Totals::default());
    }

    #[test]
    fn last_active_is_coarse() {
        let now = Utc::now();
        assert_eq!(last_active_label(None, now), "no activity");
        assert_eq!(last_active_label(Some(now), now), "active just now");
        let hours = now - chrono::Duration::hours(5);
        assert_eq!(last_active_label(Some(hours), now), "active 5 h ago");
        let days = now - chrono::Duration::days(3);
        assert_eq!(last_active_label(Some(days), now), "active 3 d ago");
    }

    #[test]
    fn refusal_lists_only_unsaved_work() {
        let text = refusal_details(&[
            removal(&["src/lib.rs", "notes.md"], 3, 0),
            removal(&[], 0, 0),
            removal(&[], 0, 1),
        ]);
        assert!(text.contains("3 uncommitted or untracked files: src/lib.rs, notes.md and 1 more"));
        assert!(text.contains("1 commit not in the source repository"));
        assert_eq!(text.matches("/src/app/.trama/worktrees/t/c").count(), 2);
        assert!(refusal_details(&[]).contains("unsaved work"));
        assert_eq!(
            freed_total(&[removal(&[], 0, 0), removal(&[], 0, 0)]),
            Some(4096)
        );
        assert_eq!(freed_total(&[]), None);
    }

    #[test]
    fn removal_kinds_build_the_wire_messages() {
        let id = ThreadId::from("t1");
        assert_eq!(
            RemovalKind::Archive.message(id.clone(), false),
            ClientMessage::ArchiveThread {
                thread_id: id.clone(),
                delete_checkouts: true,
                force: false,
            }
        );
        assert_eq!(
            RemovalKind::DeleteCopy.message(id.clone(), true),
            ClientMessage::DeleteThreadCheckouts {
                thread_id: id,
                force: true,
            }
        );
    }
}
