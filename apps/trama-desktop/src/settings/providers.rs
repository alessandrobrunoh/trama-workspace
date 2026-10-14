//! Settings > Providers: the model providers the daemon knows (ADR 0015).
//!
//! The daemon owns the list and the keys; this view sends
//! `ListProviders` / `SaveProvider` / `RemoveProvider` / `SetDefaultProvider`
//! / `TestProvider` and renders the replies. The API key field is masked,
//! cleared after saving, and never filled from the daemon (it cannot be: the
//! daemon only says whether a key exists).

use std::collections::HashMap;

use gpui_kit::component::{
    Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_protocol::{
    ApiKeyUpdate, ClientMessage, DaemonMessage, ProviderConfig, ProviderKind, ProviderSnapshot,
    ProviderTestReport,
};

use crate::app::theme::{self, Palette};
use crate::system::net::DaemonClient;

/// One-click starting points for the add form.
pub struct Preset {
    pub label: &'static str,
    pub kind: ProviderKind,
    pub base_url: &'static str,
    pub model: &'static str,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        label: "OpenAI",
        kind: ProviderKind::OpenAi,
        base_url: "",
        model: "gpt-4o",
    },
    Preset {
        label: "OpenRouter",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://openrouter.ai/api/v1",
        model: "openai/gpt-4o",
    },
    Preset {
        label: "Ollama",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "http://localhost:11434/v1",
        model: "llama3.1",
    },
    Preset {
        label: "LM Studio",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "http://localhost:1234/v1",
        model: "",
    },
    Preset {
        label: "Groq",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.groq.com/openai/v1",
        model: "llama-3.3-70b-versatile",
    },
    Preset {
        label: "Custom",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "",
        model: "",
    },
];

/// The add/edit form.
struct Editor {
    /// Empty for a new provider.
    id: String,
    kind: ProviderKind,
    has_stored_key: bool,
    clear_key: bool,
    models: Vec<String>,
}

pub struct ProvidersSection {
    client: DaemonClient,
    pub items: Vec<ProviderSnapshot>,
    loaded: bool,
    editor: Option<Editor>,
    tests: HashMap<String, Option<ProviderTestReport>>,
    name: Entity<InputState>,
    base_url: Entity<InputState>,
    api_key: Entity<InputState>,
    model: Entity<InputState>,
    error: Option<String>,
}

/// Id used for test results of the unsaved form.
const DRAFT: &str = "";

impl ProvidersSection {
    pub fn new(client: DaemonClient, window: &mut Window, cx: &mut App) -> Self {
        Self {
            client,
            items: Vec::new(),
            loaded: false,
            editor: None,
            tests: HashMap::new(),
            name: cx.new(|cx| InputState::new(window, cx).placeholder("Name, e.g. Ollama")),
            base_url: cx.new(|cx| {
                InputState::new(window, cx).placeholder("Base URL, e.g. http://localhost:11434/v1")
            }),
            api_key: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("API key (stored in the OS keychain)")
                    .masked(true)
            }),
            model: cx.new(|cx| InputState::new(window, cx).placeholder("Default model")),
            error: None,
        }
    }

    pub fn refresh(&mut self) {
        let _ = self.client.send(ClientMessage::ListProviders);
    }

    /// `true` when the message was for this section.
    pub fn handle(&mut self, message: &DaemonMessage) -> bool {
        match message {
            DaemonMessage::Providers { items } => {
                let changed = self.loaded && *items != self.items;
                self.items = items.clone();
                self.loaded = true;
                if changed {
                    // The composer's model picker lists provider models.
                    let _ = self.client.send(ClientMessage::ListModels);
                }
                true
            }
            DaemonMessage::ProviderTestResult { id, report } => {
                if let Some(editor) = &mut self.editor
                    && (id.is_empty() || *id == editor.id)
                    && report.ok
                {
                    editor.models = report.models.clone();
                }
                self.tests.insert(id.clone(), Some(report.clone()));
                true
            }
            DaemonMessage::Error { message, .. } if self.editor.is_some() => {
                self.error = Some(message.clone());
                false
            }
            _ => false,
        }
    }

    fn open_editor(
        &mut self,
        existing: Option<ProviderSnapshot>,
        preset: Option<&Preset>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (id, kind, name, url, model, has_key, models) = match (&existing, preset) {
            (Some(p), _) => (
                p.config.id.clone(),
                p.config.kind,
                p.config.name.clone(),
                p.config.base_url.clone().unwrap_or_default(),
                p.config.default_model.clone().unwrap_or_default(),
                p.has_api_key,
                p.config.models.clone(),
            ),
            (None, Some(preset)) => (
                String::new(),
                preset.kind,
                if preset.label == "Custom" {
                    String::new()
                } else {
                    preset.label.to_owned()
                },
                preset.base_url.to_owned(),
                preset.model.to_owned(),
                false,
                Vec::new(),
            ),
            (None, None) => return,
        };
        let set = |input: &Entity<InputState>, value: String, window: &mut Window, cx: &mut App| {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        };
        set(&self.name, name, window, cx);
        set(&self.base_url, url, window, cx);
        set(&self.api_key, String::new(), window, cx);
        set(&self.model, model, window, cx);
        self.tests.remove(DRAFT);
        self.error = None;
        self.editor = Some(Editor {
            id,
            kind,
            has_stored_key: has_key,
            clear_key: false,
            models,
        });
    }

    fn form_config(&self, cx: &App) -> Option<(ProviderConfig, ApiKeyUpdate)> {
        let editor = self.editor.as_ref()?;
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let url = text(&self.base_url);
        let model = text(&self.model);
        let key = text(&self.api_key);
        let config = ProviderConfig {
            id: editor.id.clone(),
            kind: editor.kind,
            name: text(&self.name),
            base_url: (!url.is_empty()).then_some(url),
            default_model: (!model.is_empty()).then_some(model),
            models: editor.models.clone(),
        };
        let key = if !key.is_empty() {
            ApiKeyUpdate::Set(key)
        } else if editor.clear_key {
            ApiKeyUpdate::Clear
        } else {
            ApiKeyUpdate::Keep
        };
        Some((config, key))
    }

    fn save(&mut self, window: &mut Window, cx: &mut App) {
        let Some((config, key)) = self.form_config(cx) else {
            return;
        };
        self.error = None;
        let _ = self.client.send(ClientMessage::SaveProvider {
            provider: config,
            api_key: key,
        });
        self.api_key
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.editor = None;
    }

    fn test_draft(&mut self, cx: &App) {
        let Some((config, key)) = self.form_config(cx) else {
            return;
        };
        let id = if config.id.is_empty() {
            DRAFT.to_owned()
        } else {
            config.id.clone()
        };
        self.tests.insert(id.clone(), None);
        let _ = self.client.send(ClientMessage::TestProvider {
            id,
            provider: Some(config),
            api_key: key,
        });
    }

    pub fn render(&mut self, view: View, p: &Palette) -> AnyElement {
        if !self.loaded {
            self.refresh();
        }
        let note = div()
            .p(theme::sp(3.))
            .rounded(theme::radius())
            .bg(p.accent_soft)
            .text_size(theme::text_sm())
            .text_color(p.text)
            .child(
                "Claude Code runs the full agent: tools, file edits, terminals and subagents, \
                 with your Claude Code login. API providers run plain chat turns only: no tools, \
                 no file access. They bill the API key you add here; Claude.ai credentials are \
                 never sent to them.",
            );

        let mut list = v_flex().gap(theme::sp(2.));
        for (index, item) in self.items.iter().enumerate() {
            list = list.child(self.render_row(index, item, view.clone(), p));
        }

        let presets =
            h_flex()
                .flex_wrap()
                .gap(theme::sp(1.5))
                .children(PRESETS.iter().enumerate().map(|(index, preset)| {
                    let view = view.clone();
                    Button::new(("provider-preset", index))
                        .small()
                        .outline()
                        .label(format!("Add {}", preset.label))
                        .on_click(move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.providers
                                    .open_editor(None, Some(&PRESETS[index]), window, cx);
                                cx.notify();
                            });
                        })
                }));

        v_flex()
            .gap(theme::sp(3.))
            .child(note)
            .child(list)
            .when(self.editor.is_none(), |this| this.child(presets))
            .when_some(self.editor.as_ref(), |this, editor| {
                this.child(self.render_editor(editor, &view, p))
            })
            .into_any_element()
    }

    fn render_row(
        &self,
        index: usize,
        item: &ProviderSnapshot,
        view: View,
        p: &Palette,
    ) -> impl IntoElement {
        let id = item.config.id.clone();
        let test = self.tests.get(&id).cloned();
        let detail = match item.config.kind {
            ProviderKind::ClaudeCode => {
                "Local `claude` CLI · agentic (tools, files, subagents)".to_owned()
            }
            kind => format!(
                "{} · {} · {} · chat only",
                kind.label(),
                item.config
                    .base_url
                    .clone()
                    .or_else(|| kind.default_base_url().map(str::to_owned))
                    .unwrap_or_default(),
                if item.has_api_key {
                    "key stored"
                } else {
                    "no key"
                },
            ),
        };
        let snapshot = item.clone();
        v_flex()
            .p(theme::sp(3.))
            .gap(theme::sp(1.5))
            .rounded(theme::radius_lg())
            .border_1()
            .border_color(if item.is_default { p.accent } else { p.border })
            .bg(p.panel)
            .child(
                h_flex()
                    .gap(theme::sp(2.))
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap(theme::sp(1.5))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(item.config.name.clone()),
                                    )
                                    .when(item.is_default, |this| {
                                        this.child(
                                            div()
                                                .text_size(theme::text_xs())
                                                .text_color(p.accent_text)
                                                .child("Default"),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme::text_xs())
                                    .text_color(p.text_faint)
                                    .child(detail),
                            )
                            .when_some(item.config.default_model.clone(), |this, model| {
                                this.child(
                                    div()
                                        .text_size(theme::text_xs())
                                        .text_color(p.text_muted)
                                        .child(format!("Default model: {model}")),
                                )
                            }),
                    )
                    .when(!item.is_default, |this| {
                        let view = view.clone();
                        let id = id.clone();
                        this.child(
                            Button::new(("provider-default", index))
                                .small()
                                .ghost()
                                .label("Make default")
                                .on_click(move |_, _, cx| {
                                    let _ = view.update(cx, |this, _| {
                                        let _ = this.providers.client.send(
                                            ClientMessage::SetDefaultProvider { id: id.clone() },
                                        );
                                    });
                                }),
                        )
                    })
                    .when(!item.builtin, |this| {
                        let test_view = view.clone();
                        let edit_view = view.clone();
                        let remove_view = view.clone();
                        let test_id = id.clone();
                        let remove_id = id.clone();
                        this.child(
                            Button::new(("provider-test", index))
                                .small()
                                .ghost()
                                .label("Test")
                                .on_click(move |_, _, cx| {
                                    let _ = test_view.update(cx, |this, cx| {
                                        this.providers.tests.insert(test_id.clone(), None);
                                        let _ = this.providers.client.send(
                                            ClientMessage::TestProvider {
                                                id: test_id.clone(),
                                                provider: None,
                                                api_key: ApiKeyUpdate::Keep,
                                            },
                                        );
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            Button::new(("provider-edit", index))
                                .small()
                                .ghost()
                                .label("Edit")
                                .on_click(move |_, window, cx| {
                                    let _ = edit_view.update(cx, |this, cx| {
                                        this.providers.open_editor(
                                            Some(snapshot.clone()),
                                            None,
                                            window,
                                            cx,
                                        );
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            Button::new(("provider-remove", index))
                                .small()
                                .ghost()
                                .label("Remove")
                                .on_click(move |_, _, cx| {
                                    let _ = remove_view.update(cx, |this, _| {
                                        let _ = this.providers.client.send(
                                            ClientMessage::RemoveProvider {
                                                id: remove_id.clone(),
                                            },
                                        );
                                    });
                                }),
                        )
                    }),
            )
            .when_some(test, |this, test| this.child(test_line(test.as_ref(), p)))
    }

    fn render_editor(&self, editor: &Editor, view: &View, p: &Palette) -> impl IntoElement {
        let test = self
            .tests
            .get(if editor.id.is_empty() {
                DRAFT
            } else {
                &editor.id
            })
            .cloned();
        let label = |text: &'static str| {
            div()
                .text_size(theme::text_xs())
                .text_color(p.text_muted)
                .child(text)
        };
        let key_hint = match (editor.kind, editor.has_stored_key, editor.clear_key) {
            (_, true, false) => "A key is stored. Leave empty to keep it.",
            (_, true, true) => "The stored key will be removed on save.",
            (ProviderKind::OpenAi, false, _) => "Required for OpenAI.",
            _ => "Optional for local servers (Ollama, LM Studio).",
        };
        let models = editor.models.clone();
        v_flex()
            .p(theme::sp(3.))
            .gap(theme::sp(2.))
            .rounded(theme::radius_lg())
            .border_1()
            .border_color(p.border_strong)
            .bg(p.elevated)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if editor.id.is_empty() {
                        format!("Add {} provider", editor.kind.label())
                    } else {
                        format!("Edit {}", editor.id)
                    }),
            )
            .child(label("Name"))
            .child(Input::new(&self.name).id("provider-name"))
            .when(editor.kind == ProviderKind::OpenAiCompatible, |this| {
                this.child(label("Base URL (https://, or http:// for this machine)"))
                    .child(Input::new(&self.base_url).id("provider-url"))
            })
            .child(label("API key"))
            .child(Input::new(&self.api_key).id("provider-key"))
            .child(
                h_flex()
                    .gap(theme::sp(2.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child(key_hint),
                    )
                    .when(editor.has_stored_key && !editor.clear_key, |this| {
                        this.child(
                            Button::new("provider-clear-key")
                                .small()
                                .ghost()
                                .label("Remove stored key")
                                .on_click(on(view, |this, _, _| {
                                    if let Some(editor) = &mut this.editor {
                                        editor.clear_key = true;
                                    }
                                })),
                        )
                    }),
            )
            .child(label("Default model"))
            .child(Input::new(&self.model).id("provider-model"))
            .when(!models.is_empty(), |this| {
                this.child(
                    div()
                        .text_size(theme::text_xs())
                        .text_color(p.text_faint)
                        .child(format!(
                            "{} models from the last test are offered in the model picker.",
                            models.len()
                        )),
                )
            })
            .when_some(test, |this, test| this.child(test_line(test.as_ref(), p)))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_size(theme::text_sm())
                        .text_color(p.danger)
                        .child(error),
                )
            })
            .child(
                h_flex()
                    .gap(theme::sp(2.))
                    .justify_end()
                    .child(
                        Button::new("provider-cancel")
                            .small()
                            .ghost()
                            .label("Cancel")
                            .on_click(on(view, |this, _, _| this.editor = None)),
                    )
                    .child(
                        Button::new("provider-test-draft")
                            .small()
                            .outline()
                            .label("Test connection")
                            .on_click(on(view, |this, _, cx| this.test_draft(cx))),
                    )
                    .child(
                        Button::new("provider-save")
                            .small()
                            .primary()
                            .label("Save")
                            .disabled(false)
                            .on_click(on(view, |this, window, cx| this.save(window, cx))),
                    ),
            )
    }
}

type View = WeakEntity<super::SettingsView>;

/// Click handler running `f` on this section inside the settings view.
fn on(
    view: &View,
    f: fn(&mut ProvidersSection, &mut Window, &mut App),
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let view = view.clone();
    move |_, window, cx| {
        let _ = view.update(cx, |this, cx| {
            f(&mut this.providers, window, cx);
            cx.notify();
        });
    }
}

fn test_line(report: Option<&ProviderTestReport>, p: &Palette) -> Div {
    let (color, text) = match report {
        None => (p.text_muted, "Testing…".to_owned()),
        Some(report) if report.ok => (p.success, report.message.clone()),
        Some(report) => (p.danger, report.message.clone()),
    };
    div()
        .text_size(theme::text_xs())
        .text_color(color)
        .child(text)
}
