//! Selected-thread working copies. Never guesses a path from an original repo.

use gpui_kit::component::{
    Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::path::PathBuf;
use trama_protocol::{CheckoutSnapshot, CheckoutStatus, ThreadId};

use crate::theme::{self, Palette};

#[derive(Default)]
pub struct CopyState {
    thread: Option<ThreadId>,
    rows: Vec<CheckoutSnapshot>,
    loaded: bool,
}

impl CopyState {
    pub fn select(&mut self, thread: Option<ThreadId>) {
        self.thread = thread;
        self.rows.clear();
        self.loaded = false;
    }

    pub fn apply(&mut self, thread: &ThreadId, rows: Vec<CheckoutSnapshot>) -> bool {
        if self.thread.as_ref() != Some(thread) {
            return false;
        }
        self.rows = rows;
        self.loaded = true;
        true
    }
}

fn ready_path(row: &CheckoutSnapshot) -> Option<PathBuf> {
    if row.status != CheckoutStatus::Ready {
        return None;
    }
    row.checkout_path
        .as_ref()
        .filter(|path| *path != &row.source_path)
        .cloned()
}

fn status_label(status: CheckoutStatus) -> &'static str {
    match status {
        CheckoutStatus::Ready => "Ready",
        CheckoutStatus::NeedsPreparation => "Needs preparation",
        CheckoutStatus::Missing => "Missing",
        CheckoutStatus::Invalid => "Error",
    }
}

pub struct CopyPanel {
    pub state: CopyState,
    launching: bool,
    notice: Option<String>,
    launch_task: Option<Task<()>>,
}

impl CopyPanel {
    pub fn new() -> Self {
        Self {
            state: CopyState::default(),
            launching: false,
            notice: None,
            launch_task: None,
        }
    }

    pub fn select(&mut self, thread: Option<ThreadId>, cx: &mut Context<Self>) {
        self.state.select(thread);
        self.notice = None;
        cx.notify();
    }

    pub fn apply(
        &mut self,
        thread: &ThreadId,
        rows: Vec<CheckoutSnapshot>,
        cx: &mut Context<Self>,
    ) {
        if self.state.apply(thread, rows) {
            cx.notify();
        }
    }

    fn open(&mut self, path: PathBuf, finder: bool, cx: &mut Context<Self>) {
        if self.launching {
            return;
        }
        self.launching = true;
        let job = cx.background_spawn(async move {
            let plan = if finder {
                crate::editor::finder_plan(&path)
            } else {
                crate::editor::zed_plan(&path, std::env::var_os("PATH").as_deref())
            };
            match plan {
                Ok(plan) => crate::editor::launch(plan),
                Err(error) => Err(format!("Cannot open this copy: {error:?}")),
            }
        });
        self.launch_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.launching = false;
                this.notice = Some(match result {
                    Ok(()) => "Open request sent.".into(),
                    Err(error) => error,
                });
                cx.notify();
            });
        }));
    }
}

impl Render for CopyPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let small = |text: String, color: Hsla| {
            div()
                .text_size(theme::text_xs())
                .text_color(color)
                .child(text)
        };
        if self.state.thread.is_none() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(small(
                    "Select a thread to see its working copies.".into(),
                    p.text_muted,
                ))
                .into_any_element();
        }
        v_flex()
            .id("copies")
            .size_full()
            .p(theme::sp(3.))
            .gap(theme::sp(2.))
            .overflow_y_scrollbar()
            .when(!self.state.loaded, |panel| {
                panel.child(small("Checking this thread's working copies…".into(), p.text_muted))
            })
            .when(self.state.loaded && self.state.rows.is_empty(), |panel| {
                panel.child(small("No repository is linked to this thread.".into(), p.text_muted))
            })
            .children(self.state.rows.iter().cloned().enumerate().map(|(index, row)| {
                let path = ready_path(&row);
                let zed_path = path.clone();
                let finder_path = path.clone();
                let clipboard_path = path.clone();
                v_flex()
                    .gap(theme::sp(1.))
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
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(p.text)
                                    .child(row.title.clone()),
                            )
                            .child(
                                div()
                                    .text_color(status_color(row.status, &p))
                                    .child(status_label(row.status)),
                            ),
                    )
                    .child(small(
                        format!("Original: {}", row.source_path.display()),
                        p.text_faint,
                    ))
                    .child(small(
                        match &row.checkout_path {
                            Some(path) => format!("Thread copy: {}", path.display()),
                            None => "No copy to open yet. Prepare an isolated checkout (\u{2318}K).".into(),
                        },
                        p.text_muted,
                    ))
                    .when_some(row.detail, |panel, detail| panel.child(small(detail, p.text_muted)))
                    .child(
                        h_flex()
                            .gap(theme::sp(1.))
                            .pt(theme::sp(1.))
                            .child(
                                Button::new(SharedString::from(format!("open-zed-{index}")))
                                    .ghost()
                                    .xsmall()
                                    .label("Open in Zed")
                                    .disabled(path.is_none() || self.launching)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(path) = &zed_path {
                                            this.open(path.clone(), false, cx);
                                        }
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("open-finder-{index}")))
                                    .ghost()
                                    .xsmall()
                                    .label("Reveal in Finder")
                                    .disabled(path.is_none() || self.launching)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(path) = &finder_path {
                                            this.open(path.clone(), true, cx);
                                        }
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("copy-path-{index}")))
                                    .ghost()
                                    .xsmall()
                                    .label("Copy path")
                                    .disabled(path.is_none())
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        if let Some(path) = &clipboard_path {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                path.to_string_lossy().into_owned(),
                                            ));
                                        }
                                    })),
                            ),
                    )
            }))
            .when_some(self.notice.clone(), |panel, notice| {
                panel.child(small(notice, p.text_muted))
            })
            .into_any_element()
    }
}

fn status_color(status: CheckoutStatus, p: &Palette) -> Hsla {
    match status {
        CheckoutStatus::Ready => p.success,
        CheckoutStatus::NeedsPreparation => p.warning,
        CheckoutStatus::Missing | CheckoutStatus::Invalid => p.danger,
    }
}

#[cfg(test)]
mod tests {
    use super::{CopyState, ready_path};
    use trama_protocol::{CheckoutSnapshot, CheckoutStatus, RepositoryId, ThreadId};

    fn row(status: CheckoutStatus) -> CheckoutSnapshot {
        CheckoutSnapshot {
            repository_id: RepositoryId::from("repo"),
            title: "backend".into(),
            source_path: "/source".into(),
            checkout_path: Some("/source/.trama/worktrees/thread/copy".into()),
            status,
            detail: None,
        }
    }

    #[test]
    fn only_ready_isolated_paths_enable_opening() {
        assert!(ready_path(&row(CheckoutStatus::Ready)).is_some());
        assert!(ready_path(&row(CheckoutStatus::Invalid)).is_none());
        assert!(ready_path(&row(CheckoutStatus::NeedsPreparation)).is_none());
        let mut original = row(CheckoutStatus::Ready);
        original.checkout_path = Some(original.source_path.clone());
        assert!(ready_path(&original).is_none());
    }

    #[test]
    fn stale_thread_responses_are_discarded() {
        let mut state = CopyState::default();
        state.select(Some(ThreadId::from("new")));
        assert!(!state.apply(&ThreadId::from("old"), vec![row(CheckoutStatus::Ready)]));
        assert!(state.rows.is_empty());
        assert!(state.apply(&ThreadId::from("new"), vec![row(CheckoutStatus::Ready)]));
        state.select(None);
        assert!(state.rows.is_empty());
        assert!(!state.loaded);
    }
}
