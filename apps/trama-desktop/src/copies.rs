//! Selected-thread working copies. Never guesses a path from an original repo.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::path::PathBuf;
use trama_protocol::{CheckoutSnapshot, CheckoutStatus, ThreadId};

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
        CheckoutStatus::Ready => "Pronta",
        CheckoutStatus::NeedsPreparation => "Da preparare",
        CheckoutStatus::Missing => "Non trovata",
        CheckoutStatus::Invalid => "Errore",
    }
}

pub struct CopyPanel {
    pub state: CopyState,
    expanded: bool,
    launching: bool,
    notice: Option<String>,
    launch_task: Option<Task<()>>,
}

impl CopyPanel {
    pub fn new() -> Self {
        Self {
            state: CopyState::default(),
            expanded: true,
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
                Err(error) => Err(format!("Copia non apribile: {error:?}")),
            }
        });
        self.launch_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.launching = false;
                this.notice = Some(match result {
                    Ok(()) => "Richiesta di apertura inviata.".into(),
                    Err(error) => error,
                });
                cx.notify();
            });
        }));
    }
}

impl Render for CopyPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.state.thread.is_none() {
            return div().into_any_element();
        }
        v_flex().w_full().border_b_1().border_color(cx.theme().border).px_4().py_2()
            .child(Button::new("toggle-working-copies").ghost().small()
                .label(if self.expanded { "▾ Copie di lavoro" } else { "▸ Copie di lavoro" })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.expanded = !this.expanded;
                    cx.notify();
                })))
            .when(self.expanded, |panel| panel.child(
                v_flex().gap_2().max_h(px(230.)).overflow_y_scrollbar()
                    .when(!self.state.loaded, |panel| panel.child(
                        div().text_xs().child("Verifica delle copie di questo thread…")))
                    .when(self.state.loaded && self.state.rows.is_empty(), |panel| panel.child(
                        div().text_xs().child("Nessun repository collegato a questo thread.")))
                    .children(self.state.rows.iter().cloned().enumerate().map(|(index, row)| {
                        let path = ready_path(&row);
                        let zed_path = path.clone();
                        let finder_path = path.clone();
                        let clipboard_path = path.clone();
                        v_flex().gap_1()
                            .child(div().text_sm().child(format!("{} · {}", row.title, status_label(row.status))))
                            .child(div().text_xs().text_color(cx.theme().muted_foreground)
                                .child(format!("Repository originale: {}", row.source_path.display())))
                            .child(div().text_xs().child(match &row.checkout_path {
                                Some(path) => format!("Copia di lavoro di questo thread: {}", path.display()),
                                None => "Nessuna copia apribile. Usa «Prepara copia di lavoro» nella sidebar.".into(),
                            }))
                            .when_some(row.detail, |panel, detail| panel.child(div().text_xs().child(detail)))
                            .child(h_flex().gap_2()
                                .child(Button::new(SharedString::from(format!("open-zed-{index}"))).ghost().small()
                                    .label("Apri in Zed").disabled(path.is_none() || self.launching)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(path) = &zed_path { this.open(path.clone(), false, cx); }
                                    })))
                                .child(Button::new(SharedString::from(format!("open-finder-{index}"))).ghost().small()
                                    .label("Mostra nel Finder").disabled(path.is_none() || self.launching)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(path) = &finder_path { this.open(path.clone(), true, cx); }
                                    })))
                                .child(Button::new(SharedString::from(format!("copy-path-{index}"))).ghost().small()
                                    .label("Copia percorso").disabled(path.is_none())
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        if let Some(path) = &clipboard_path {
                                            cx.write_to_clipboard(ClipboardItem::new_string(path.to_string_lossy().into_owned()));
                                        }
                                    }))))
                    }))
                    .when_some(self.notice.clone(), |panel, notice| panel.child(div().text_xs().child(notice)))
            ))
            .into_any_element()
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
