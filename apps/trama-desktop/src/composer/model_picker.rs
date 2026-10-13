//! The composer footer's model picker: shows the thread's model (its
//! override, else the workspace default) and changes it with
//! `SetThreadModel`.

use std::collections::HashMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, tooltip::Tooltip, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::ThreadId;
use trama_protocol::{ClientMessage, DaemonMessage, ModelInfo};

use crate::app::theme;
use crate::conversation::turns::model_label;
use crate::system::net::{DaemonClient, SendStatus};

/// What new sessions use when a thread has no override.
pub const DEFAULT_MODEL_LABEL: &str = "Sonnet 5.5";
const DEFAULT_ID: &str = "default";

/// Short label for a model id (`None`: the workspace default).
pub fn label_for(model: Option<&str>, models: &[ModelInfo]) -> String {
    match model {
        None | Some(DEFAULT_ID) | Some("") => DEFAULT_MODEL_LABEL.to_string(),
        Some(id) => models
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.label.trim_start_matches("Claude ").to_string())
            .unwrap_or_else(|| model_label(id)),
    }
}

/// The message choosing `id` sends; `default` clears the override.
pub fn set_model_message(thread_id: ThreadId, id: &str) -> ClientMessage {
    ClientMessage::SetThreadModel {
        thread_id,
        model: (id != DEFAULT_ID).then(|| id.to_string()),
    }
}

pub struct ModelPicker {
    client: DaemonClient,
    thread: Option<ThreadId>,
    overrides: HashMap<ThreadId, Option<String>>,
    models: Vec<ModelInfo>,
    requested: bool,
    open: bool,
    /// The model the live session reported (for the tooltip).
    pub session_model: Option<String>,
    pub enabled: bool,
}

impl ModelPicker {
    pub fn new(client: DaemonClient) -> Self {
        Self {
            client,
            thread: None,
            overrides: HashMap::new(),
            models: Vec::new(),
            requested: false,
            open: false,
            session_model: None,
            enabled: false,
        }
    }

    pub fn set_thread(&mut self, thread: Option<ThreadId>, cx: &mut Context<Self>) {
        self.thread = thread;
        self.open = false;
        self.session_model = None;
        cx.notify();
    }

    /// Overrides from the thread snapshots the sidebar holds.
    pub fn set_overrides(
        &mut self,
        overrides: impl IntoIterator<Item = (ThreadId, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        let next: HashMap<_, _> = overrides.into_iter().collect();
        if next != self.overrides {
            self.overrides = next;
            cx.notify();
        }
    }

    /// The selected thread's override.
    pub fn current(&self) -> Option<&str> {
        self.thread
            .as_ref()
            .and_then(|thread| self.overrides.get(thread))
            .and_then(|model| model.as_deref())
    }

    pub fn reconnected(&mut self) {
        self.requested = false;
    }

    pub fn apply(&mut self, message: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        match message {
            DaemonMessage::Models { items } => {
                self.models = items.clone();
                cx.notify();
                true
            }
            DaemonMessage::ThreadModelChanged { thread_id, model } => {
                self.overrides.insert(thread_id.clone(), model.clone());
                cx.notify();
                true
            }
            _ => false,
        }
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        if !self.enabled || self.thread.is_none() {
            return;
        }
        self.open = !self.open;
        if self.open && (self.models.is_empty() || !self.requested) {
            self.requested = self.client.send(ClientMessage::ListModels) == SendStatus::Enqueued;
        }
        cx.notify();
    }

    fn choose(&mut self, id: String, cx: &mut Context<Self>) {
        self.open = false;
        let Some(thread) = self.thread.clone() else {
            return;
        };
        if self.client.send(set_model_message(thread.clone(), &id)) == SendStatus::Enqueued {
            // Shown right away; `ThreadModelChanged` confirms it.
            self.overrides
                .insert(thread, (id != DEFAULT_ID).then_some(id));
        }
        cx.notify();
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = theme::palette(cx);
        let current = self.current().unwrap_or(DEFAULT_ID).to_string();
        let mut list = v_flex().p(theme::sp(1.)).gap(px(1.));
        if self.models.is_empty() {
            list = list.child(
                div()
                    .px(theme::sp(2.))
                    .py(theme::sp(1.5))
                    .text_size(theme::text_sm())
                    .text_color(p.text_faint)
                    .child("Loading models…"),
            );
        }
        for (ix, model) in self.models.iter().enumerate() {
            let selected = model.id == current;
            let id = model.id.clone();
            let (title, detail) = if model.id == DEFAULT_ID {
                (
                    "Default".to_string(),
                    format!("Workspace default · {DEFAULT_MODEL_LABEL}"),
                )
            } else {
                (label_for(Some(&model.id), &self.models), model.id.clone())
            };
            list = list.child(
                h_flex()
                    .id(("model-row", ix))
                    .w_full()
                    .h(px(34.))
                    .px(theme::sp(2.))
                    .gap(theme::sp(2.))
                    .items_center()
                    .rounded(theme::radius())
                    .cursor_pointer()
                    .when(selected, |this| this.bg(p.accent_soft))
                    .hover(|style| style.bg(p.hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.choose(id.clone(), cx)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(theme::text_ui())
                                    .text_color(p.text)
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme::text_xs())
                                    .text_color(p.text_faint)
                                    .child(detail),
                            ),
                    )
                    .when(selected, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .with_size(px(13.))
                                .text_color(p.accent_text),
                        )
                    }),
            );
        }
        v_flex()
            .id("model-menu")
            .w(px(248.))
            .rounded(theme::radius_lg())
            .border_1()
            .border_color(p.border_strong)
            .bg(p.elevated)
            .shadow_md()
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.open = false;
                cx.notify();
            }))
            .child(
                div()
                    .px(theme::sp(3.))
                    .pt(theme::sp(2.))
                    .text_size(theme::text_xs())
                    .text_color(p.text_faint)
                    .child("Model for this thread's next turns"),
            )
            .child(list)
            .into_any_element()
    }
}

impl Render for ModelPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let label = label_for(self.current(), &self.models);
        let usable = self.enabled && self.thread.is_some();
        let tooltip: SharedString = match &self.session_model {
            Some(model) => format!(
                "Choose the model for this thread. The current session runs {}.",
                model_label(model)
            )
            .into(),
            None => "Choose the model for this thread".into(),
        };
        let menu = self.open.then(|| {
            deferred(
                anchored()
                    .anchor(Anchor::BottomLeft)
                    .offset(point(px(0.), px(-4.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(self.render_menu(cx)),
            )
            .with_priority(3)
        });
        div().relative().children(menu).child(
            h_flex()
                .id("composer-model")
                .h(px(22.))
                .px(theme::sp(2.))
                .gap(theme::sp(1.5))
                .items_center()
                .rounded(theme::radius())
                .text_size(theme::text_xs())
                .text_color(p.text_muted)
                .when(self.open, |this| this.bg(p.hover))
                .when(usable, |this| {
                    this.cursor_pointer()
                        .hover(|style| style.bg(p.hover))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
                        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                })
                .child(Icon::new(IconName::Cpu).with_size(px(12.)))
                .child(label)
                .when(usable, |this| {
                    this.child(
                        Icon::new(IconName::ChevronDown)
                            .with_size(px(11.))
                            .text_color(p.text_faint),
                    )
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` brings GPUI's own `test` attribute into scope.
    use core::prelude::v1::test;

    fn models() -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "default".into(),
                label: "Default".into(),
            },
            ModelInfo {
                id: "claude-opus-5-5".into(),
                label: "Claude Opus 5.5".into(),
            },
        ]
    }

    #[test]
    fn labels_fall_back_to_the_workspace_default_and_id_parsing() {
        assert_eq!(label_for(None, &models()), "Sonnet 5.5");
        assert_eq!(label_for(Some("default"), &[]), "Sonnet 5.5");
        assert_eq!(label_for(Some("claude-opus-5-5"), &models()), "Opus 5.5");
        assert_eq!(label_for(Some("claude-haiku-5-5"), &[]), "Haiku 5.5");
    }

    #[test]
    fn choosing_default_clears_the_override() {
        let thread = ThreadId::from("t");
        assert_eq!(
            set_model_message(thread.clone(), "default"),
            ClientMessage::SetThreadModel {
                thread_id: thread.clone(),
                model: None
            }
        );
        assert_eq!(
            set_model_message(thread.clone(), "claude-fable-5-1"),
            ClientMessage::SetThreadModel {
                thread_id: thread,
                model: Some("claude-fable-5-1".into())
            }
        );
    }
}
