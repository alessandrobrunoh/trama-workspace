//! Settings: a dedicated view with a sidebar of sections (General,
//! Appearance, Keybindings, Collaboration, Providers, Advanced), persisted
//! in `settings.json` in the app config dir (see [`store`]).
//!
//! Appearance and keybindings apply live. Providers are owned by the daemon
//! (ADR 0015); Collaboration hosts the account view.

pub mod keybindings;
mod providers;
pub mod store;

use std::collections::BTreeMap;

use gpui_kit::component::theme::Theme;
use gpui_kit::component::{
    Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_protocol::DaemonMessage;

use crate::app::theme::{self, Palette};
use crate::collab::account::AccountView;
use crate::system::net::DaemonClient;
pub use keybindings::OpenSettings;
use store::{AccentChoice, Density, Settings, ThemeChoice};

/// The loaded settings, readable from any view.
pub struct CurrentSettings(pub Settings);

impl Global for CurrentSettings {}

pub fn current(cx: &App) -> Settings {
    cx.try_global::<CurrentSettings>()
        .map(|s| s.0.clone())
        .unwrap_or_default()
}

/// UI font size from Appearance.
pub fn ui_font_size(cx: &App) -> Pixels {
    px(current(cx).appearance.font_size)
}

/// Load `settings.json` and apply it. Call once at startup, after the
/// default keymap and theme are installed.
pub fn init(cx: &mut App) {
    let settings = store::load();
    keybindings::apply(&BTreeMap::new(), &settings.keybindings, cx);
    apply_appearance(&settings, cx.window_appearance(), cx);
    cx.set_global(CurrentSettings(settings));
}

fn accent(choice: AccentChoice) -> theme::Accent {
    match choice {
        AccentChoice::Indigo => theme::Accent::Indigo,
        AccentChoice::Blue => theme::Accent::Blue,
        AccentChoice::Violet => theme::Accent::Violet,
        AccentChoice::Green => theme::Accent::Green,
        AccentChoice::Orange => theme::Accent::Orange,
        AccentChoice::Pink => theme::Accent::Pink,
    }
}

fn appearance(choice: ThemeChoice) -> theme::AppearancePref {
    match choice {
        ThemeChoice::System => theme::AppearancePref::System,
        ThemeChoice::Light => theme::AppearancePref::Light,
        ThemeChoice::Dark => theme::AppearancePref::Dark,
    }
}

/// Push Appearance into the theme tokens (`theme::set_prefs`) and the Kit
/// theme (font size).
fn apply_appearance(settings: &Settings, os: WindowAppearance, cx: &mut App) {
    let a = &settings.appearance;
    theme::set_prefs(
        theme::ThemePrefs {
            appearance: appearance(a.theme),
            accent: accent(a.accent),
        },
        os,
        cx,
    );
    let size = px(a.font_size);
    Theme::update(cx, |theme| theme.font_size = size);
    cx.refresh_windows();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    General,
    Appearance,
    Keybindings,
    Collaboration,
    Providers,
    Advanced,
}

impl Section {
    const ALL: [Section; 6] = [
        Section::General,
        Section::Appearance,
        Section::Keybindings,
        Section::Collaboration,
        Section::Providers,
        Section::Advanced,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Appearance => "Appearance",
            Section::Keybindings => "Keybindings",
            Section::Collaboration => "Collaboration",
            Section::Providers => "Providers",
            Section::Advanced => "Advanced",
        }
    }
}

/// A project offered as the default (id, name).
pub type ProjectChoice = (String, String);

pub struct SettingsView {
    focus_handle: FocusHandle,
    section: Section,
    settings: Settings,
    /// Overrides currently installed in the keymap.
    applied_bindings: BTreeMap<String, Vec<String>>,
    /// Action whose keys are being recorded.
    recording: Option<&'static str>,
    binding_error: Option<String>,
    save_error: Option<String>,
    account: Entity<AccountView>,
    providers: providers::ProvidersSection,
    projects: Vec<ProjectChoice>,
    daemon_connected: bool,
}

impl SettingsView {
    pub fn new(
        client: DaemonClient,
        account: Entity<AccountView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = current(cx);
        Self {
            focus_handle: cx.focus_handle(),
            section: Section::General,
            applied_bindings: settings.keybindings.clone(),
            settings,
            recording: None,
            binding_error: None,
            save_error: None,
            account,
            providers: providers::ProvidersSection::new(client, window, cx),
            projects: Vec::new(),
            daemon_connected: false,
        }
    }

    /// Called before the view is shown.
    pub fn reset_for_open(
        &mut self,
        section: Option<Section>,
        projects: Vec<ProjectChoice>,
        connected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(section) = section {
            self.section = section;
        }
        self.projects = projects;
        self.daemon_connected = connected;
        self.recording = None;
        self.binding_error = None;
        if connected {
            self.providers.refresh();
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Daemon replies for the Providers section.
    pub fn handle_daemon(&mut self, message: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        let handled = self.providers.handle(message);
        if handled || matches!(message, DaemonMessage::Error { .. }) {
            cx.notify();
        }
        handled
    }

    pub fn set_connected(&mut self, connected: bool, cx: &mut Context<Self>) {
        self.daemon_connected = connected;
        cx.notify();
    }

    /// Persist, publish and apply. Keybindings are re-applied only when they
    /// changed.
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = self.settings.clone().sanitized();
        self.save_error = store::save(&self.settings).err();
        cx.set_global(CurrentSettings(self.settings.clone()));
        if self.applied_bindings != self.settings.keybindings {
            keybindings::apply(&self.applied_bindings, &self.settings.keybindings, cx);
            self.applied_bindings = self.settings.keybindings.clone();
        }
        apply_appearance(&self.settings, window.appearance(), cx);
        cx.notify();
    }

    fn edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Settings),
    ) {
        change(&mut self.settings);
        self.commit(window, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.recording else {
            return;
        };
        cx.stop_propagation();
        if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
            self.recording = None;
            cx.notify();
            return;
        }
        let mac = cfg!(target_os = "macos");
        let Some(keys) = keybindings::from_keystroke(&event.keystroke, mac) else {
            return;
        };
        if let Some(other) = keybindings::conflict(id, &keys, &self.settings.keybindings, mac) {
            self.binding_error = Some(format!(
                "{} is already used by \u{201c}{}\u{201d}. Rebind that action first.",
                crate::app::keymap::label(&keys),
                other.label
            ));
            self.recording = None;
            cx.notify();
            return;
        }
        self.recording = None;
        self.binding_error = None;
        let default = keybindings::find(id)
            .map(|b| b.defaults.len() == 1 && b.defaults[0] == keys)
            .unwrap_or(false);
        self.edit(window, cx, |s| {
            if default {
                s.keybindings.remove(id);
            } else {
                s.keybindings.insert(id.to_owned(), vec![keys]);
            }
        });
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn section_title(text: &str) -> Div {
    div()
        .text_size(theme::text_title())
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.to_owned())
}

fn row(label: &str, detail: Option<&str>, control: impl IntoElement, p: &Palette) -> Div {
    h_flex()
        .w_full()
        .py(theme::sp(2.5))
        .gap(theme::sp(4.))
        .items_center()
        .border_b_1()
        .border_color(p.border)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().child(label.to_owned()))
                .when_some(detail, |this, detail| {
                    this.child(
                        div()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child(detail.to_owned()),
                    )
                }),
        )
        .child(control)
}

impl SettingsView {
    /// A segmented choice; `pick` maps the clicked index into the settings.
    fn choice(
        &self,
        id: &'static str,
        labels: &[&'static str],
        selected: usize,
        pick: fn(&mut Settings, usize),
        cx: &mut Context<Self>,
    ) -> Div {
        h_flex()
            .gap(theme::sp(1.))
            .children(labels.iter().enumerate().map(|(index, label)| {
                Button::new((id, index))
                    .small()
                    .when(index == selected, |b| b.primary())
                    .when(index != selected, |b| b.ghost())
                    .label(*label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.edit(window, cx, |s| pick(s, index));
                    }))
            }))
    }

    fn render_general(&mut self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let selected = self.settings.general.default_project.clone();
        let mut projects = h_flex().flex_wrap().gap(theme::sp(1.)).child(
            Button::new("default-project-first")
                .small()
                .when(selected.is_none(), |b| b.primary())
                .when(selected.is_some(), |b| b.ghost())
                .label("First project")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.edit(window, cx, |s| s.general.default_project = None);
                })),
        );
        for (index, (id, name)) in self.projects.iter().enumerate() {
            let id = id.clone();
            let active = selected.as_deref() == Some(id.as_str());
            projects = projects.child(
                Button::new(("default-project", index))
                    .small()
                    .when(active, |b| b.primary())
                    .when(!active, |b| b.ghost())
                    .label(name.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let id = id.clone();
                        this.edit(window, cx, move |s| s.general.default_project = Some(id));
                    })),
            );
        }
        v_flex()
            .child(section_title("General"))
            .child(row(
                "Default project at startup",
                Some("The project selected when TramaWorkspace opens."),
                projects,
                p,
            ))
            .child(row(
                "Language",
                Some("More languages are planned."),
                div().text_color(p.text_muted).child("English"),
                p,
            ))
            .child(row(
                "Telemetry",
                Some("TramaWorkspace sends no usage data, crash reports or analytics."),
                div().text_color(p.text_muted).child("Off"),
                p,
            ))
    }

    fn render_appearance(&mut self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let a = self.settings.appearance.clone();
        let theme_index = match a.theme {
            ThemeChoice::System => 0,
            ThemeChoice::Light => 1,
            ThemeChoice::Dark => 2,
        };
        let accents = [
            AccentChoice::Indigo,
            AccentChoice::Blue,
            AccentChoice::Violet,
            AccentChoice::Green,
            AccentChoice::Orange,
            AccentChoice::Pink,
        ];
        let dark = gpui_kit::component::ActiveTheme::theme(&**cx).is_dark();
        let swatches = h_flex()
            .gap(theme::sp(1.5))
            .children(accents.iter().enumerate().map(|(index, choice)| {
                let (color, _) = accent(*choice).colors(dark);
                let active = *choice == a.accent;
                let choice = *choice;
                div()
                    .id(("accent", index))
                    .size(px(22.))
                    .rounded_full()
                    .bg(color)
                    .border_2()
                    .border_color(if active { p.text } else { color.opacity(0.0) })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.edit(window, cx, |s| s.appearance.accent = choice);
                    }))
            }));
        let size = a.font_size;
        let font = h_flex()
            .gap(theme::sp(2.))
            .items_center()
            .child(
                Button::new("font-smaller")
                    .small()
                    .ghost()
                    .label("A−")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.edit(window, cx, |s| s.appearance.font_size -= 1.);
                    })),
            )
            .child(
                div()
                    .w(px(40.))
                    .text_center()
                    .child(format!("{size:.0} px")),
            )
            .child(
                Button::new("font-larger")
                    .small()
                    .ghost()
                    .label("A+")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.edit(window, cx, |s| s.appearance.font_size += 1.);
                    })),
            );
        v_flex()
            .child(section_title("Appearance"))
            .child(row(
                "Theme",
                None,
                self.choice(
                    "theme",
                    &["System", "Light", "Dark"],
                    theme_index,
                    |s, i| {
                        s.appearance.theme =
                            [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark][i]
                    },
                    cx,
                ),
                p,
            ))
            .child(row("Accent color", None, swatches, p))
            .child(row("Font size", Some("Interface text."), font, p))
            .child(row(
                "Density",
                Some("Spacing of lists and rows."),
                self.choice(
                    "density",
                    &["Compact", "Comfortable"],
                    if a.density == Density::Compact { 0 } else { 1 },
                    |s, i| s.appearance.density = [Density::Compact, Density::Comfortable][i],
                    cx,
                ),
                p,
            ))
    }

    fn render_keybindings(&mut self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let mac = cfg!(target_os = "macos");
        let overrides = self.settings.keybindings.clone();
        let mut list = v_flex();
        for (index, binding) in keybindings::BINDINGS.iter().enumerate() {
            let keys = keybindings::effective(binding, &overrides);
            let customized = overrides.contains_key(binding.id);
            let recording = self.recording == Some(binding.id);
            let shown = if recording {
                "Press keys… (Esc cancels)".to_owned()
            } else if keys.is_empty() {
                "Unbound".to_owned()
            } else {
                keys.iter()
                    .map(|k| crate::app::keymap::label(k))
                    .collect::<Vec<_>>()
                    .join("  ")
            };
            let id = binding.id;
            let control = h_flex()
                .gap(theme::sp(1.))
                .items_center()
                .child(
                    div()
                        .min_w(px(120.))
                        .px(theme::sp(2.))
                        .py(theme::sp(0.5))
                        .rounded(theme::radius_sm())
                        .border_1()
                        .border_color(if recording { p.accent } else { p.border_strong })
                        .font_family(theme::MONO_FONT)
                        .text_size(theme::text_sm())
                        .text_color(if customized { p.accent_text } else { p.text })
                        .child(shown),
                )
                .child(
                    Button::new(("record-binding", index))
                        .small()
                        .ghost()
                        .label("Record")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.recording = Some(id);
                            this.binding_error = None;
                            window.focus(&this.focus_handle, cx);
                            cx.notify();
                        })),
                )
                .when(customized, |this| {
                    this.child(
                        Button::new(("reset-binding", index))
                            .small()
                            .ghost()
                            .label("Reset")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit(window, cx, |s| {
                                    s.keybindings.remove(id);
                                });
                            })),
                    )
                });
            list = list.child(row(binding.label, None, control, p));
        }
        let conflicts = keybindings::all_conflicts(&overrides, mac);
        v_flex()
            .child(
                h_flex()
                    .items_center()
                    .child(div().flex_1().child(section_title("Keybindings")))
                    .when(!overrides.is_empty(), |this| {
                        this.child(
                            Button::new("reset-all-bindings")
                                .small()
                                .ghost()
                                .label("Reset all to defaults")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.edit(window, cx, |s| s.keybindings.clear());
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(theme::text_xs())
                    .text_color(p.text_faint)
                    .child(if mac {
                        "Bindings use ⌘ on macOS; the same settings use Ctrl on Linux and Windows."
                    } else {
                        "Bindings use Ctrl on Linux and Windows; the same settings use ⌘ on macOS."
                    }),
            )
            .when_some(self.binding_error.clone(), |this, error| {
                this.child(div().pt(theme::sp(2.)).text_color(p.danger).child(error))
            })
            .children(conflicts.into_iter().map(|(a, b, key)| {
                div().text_color(p.warning).child(format!(
                    "{} is bound to both {a} and {b}.",
                    crate::app::keymap::label(&key)
                ))
            }))
            .child(list)
    }

    fn render_advanced(&mut self, p: &Palette) -> Div {
        let data_dir = trama_protocol::default_app_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "unknown".into());
        let settings_file = store::settings_path()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "unknown".into());
        let mono = |text: String| {
            div()
                .font_family(theme::MONO_FONT)
                .text_size(theme::text_xs())
                .text_color(p.text_muted)
                .child(text)
        };
        v_flex()
            .child(section_title("Advanced"))
            .child(row(
                "Data directory",
                Some("Database, daemon socket, checkouts, providers.json. TRAMA_DATA_DIR relocates it."),
                mono(data_dir),
                p,
            ))
            .child(row("Settings file", None, mono(settings_file), p))
            .child(row(
                "Daemon",
                Some("The local trama-daemon that runs agents and keeps sessions."),
                h_flex()
                    .gap(theme::sp(1.5))
                    .items_center()
                    .child(div().size(px(7.)).rounded_full().bg(if self.daemon_connected {
                        p.success
                    } else {
                        p.danger
                    }))
                    .child(if self.daemon_connected {
                        format!("Connected · protocol v{}", trama_protocol::PROTOCOL_VERSION)
                    } else {
                        "Not connected".to_owned()
                    }),
                p,
            ))
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let nav = v_flex()
            .w(px(180.))
            .flex_shrink_0()
            .h_full()
            .p(theme::sp(2.))
            .gap(theme::sp(0.5))
            .border_r_1()
            .border_color(p.border)
            .bg(p.panel)
            .children(Section::ALL.iter().map(|section| {
                let section = *section;
                let active = section == self.section;
                div()
                    .id(section.label())
                    .h(theme::row_height())
                    .px(theme::sp(2.5))
                    .flex()
                    .items_center()
                    .rounded(theme::radius())
                    .cursor_pointer()
                    .when(active, |d| d.bg(p.selected).font_weight(FontWeight::MEDIUM))
                    .when(!active, |d| {
                        d.text_color(p.text_muted).hover(|d| d.bg(p.hover))
                    })
                    .child(section.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section = section;
                        this.recording = None;
                        cx.notify();
                    }))
            }));
        let body: AnyElement = match self.section {
            Section::General => self.render_general(&p, cx).into_any_element(),
            Section::Appearance => self.render_appearance(&p, cx).into_any_element(),
            Section::Keybindings => self.render_keybindings(&p, cx).into_any_element(),
            Section::Collaboration => v_flex()
                .gap(theme::sp(3.))
                .child(section_title("Collaboration"))
                .child(
                    div()
                        .text_size(theme::text_sm())
                        .text_color(p.text_muted)
                        .child("Collaboration server, account and runner for shared threads."),
                )
                .child(self.account.clone())
                .into_any_element(),
            Section::Providers => {
                let view = cx.weak_entity();
                v_flex()
                    .gap(theme::sp(3.))
                    .child(section_title("Model providers"))
                    .child(if self.daemon_connected {
                        self.providers.render(view, &p)
                    } else {
                        div()
                            .text_color(p.text_muted)
                            .child("Connect to the daemon to manage providers.")
                            .into_any_element()
                    })
                    .into_any_element()
            }
            Section::Advanced => self.render_advanced(&p).into_any_element(),
        };
        h_flex()
            .id("settings")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .w_full()
            .h(px(560.))
            .text_size(theme::text_ui())
            .child(nav)
            .child(
                v_flex()
                    .id("settings-body")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .px(theme::sp(5.))
                    .py(theme::sp(4.))
                    .gap(theme::sp(2.))
                    .child(body)
                    .when_some(self.save_error.clone(), |this, error| {
                        this.child(
                            div()
                                .text_size(theme::text_sm())
                                .text_color(p.danger)
                                .child(format!("Could not save settings: {error}")),
                        )
                    }),
            )
    }
}
