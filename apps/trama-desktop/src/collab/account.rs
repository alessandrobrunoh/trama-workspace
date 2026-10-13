//! Collaboration account dialog (connect, status, disconnect) and the
//! "Join shared thread…" dialog.
//!
//! The password goes straight into `ConnectCollab` and the field is cleared
//! right away; the UI never keeps it. The daemon stores only the session
//! and runner tokens, owner-only, in its app dir.

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_protocol::CollabRunnerState;

use crate::collab::{CollabOp, CollabStore, DEFAULT_SERVER_URL, JoinTarget, parse_join};
use crate::theme::{self, Palette};

pub fn runner_label(state: CollabRunnerState) -> &'static str {
    match state {
        CollabRunnerState::Off => "Runner off",
        CollabRunnerState::Connecting => "Runner connecting",
        CollabRunnerState::Online => "Runner online",
        CollabRunnerState::Failed => "Runner failed",
    }
}

pub fn runner_color(state: CollabRunnerState, p: &Palette) -> Hsla {
    match state {
        CollabRunnerState::Off => p.text_faint,
        CollabRunnerState::Connecting => p.warning,
        CollabRunnerState::Online => p.success,
        CollabRunnerState::Failed => p.danger,
    }
}

pub struct AccountView {
    store: Entity<CollabStore>,
    server: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    runner_token: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl AccountView {
    pub fn new(store: Entity<CollabStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let server = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(DEFAULT_SERVER_URL)
                .default_value(DEFAULT_SERVER_URL)
        });
        let username = cx.new(|cx| InputState::new(window, cx).placeholder("Username"));
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Password")
                .masked(true)
        });
        let runner_token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Runner token (optional, trr_…)")
                .masked(true)
        });
        let mut subscriptions = vec![cx.observe(&store, |_, _, cx| cx.notify())];
        for input in [&server, &username, &password, &runner_token] {
            subscriptions.push(
                cx.subscribe_in(input, window, |this, _, event, window, cx| {
                    if let InputEvent::PressEnter { shift: false, .. } = event {
                        this.connect(window, cx);
                    }
                }),
            );
        }
        Self {
            store,
            server,
            username,
            password,
            runner_token,
            _subscriptions: subscriptions,
        }
    }

    /// Prefill from the current status (or a join link's server).
    pub fn reset(
        &mut self,
        server_hint: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let status = self.store.read(cx).status.clone();
        let server = server_hint
            .or_else(|| status.as_ref().and_then(|s| s.server_url.clone()))
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
        let username = status
            .as_ref()
            .and_then(|s| s.username.clone())
            .unwrap_or_default();
        self.server
            .update(cx, |input, cx| input.set_value(server, window, cx));
        self.username.update(cx, |input, cx| {
            input.set_value(username.clone(), window, cx)
        });
        self.clear_secrets(window, cx);
        let focus = if username.is_empty() {
            &self.username
        } else {
            &self.password
        };
        focus.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn clear_secrets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.password
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.runner_token
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let server = self.server.read(cx).value().trim().to_string();
        let username = self.username.read(cx).value().trim().to_string();
        let password = self.password.read(cx).value().to_string();
        let token = self.runner_token.read(cx).value().trim().to_string();
        if server.is_empty() || username.is_empty() || password.is_empty() {
            self.store.update(cx, |store, cx| {
                store.account_error =
                    Some("Enter the server URL, your username and your password.".into());
                cx.notify();
            });
            return;
        }
        // Hand the secrets to the daemon and forget them here.
        self.clear_secrets(window, cx);
        self.store.update(cx, |store, cx| {
            store.connect(
                server,
                username,
                password,
                (!token.is_empty()).then_some(token),
                cx,
            );
        });
    }
}

impl Render for AccountView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let store = self.store.read(cx);
        let status = store.status.clone();
        let signed_in = store.signed_in();
        let runner = store.runner();
        let connecting = store.is_pending(CollabOp::Connect);
        let disconnecting = store.is_pending(CollabOp::Disconnect);
        let error = store.account_error.clone();
        let last_error = status.as_ref().and_then(|s| s.last_error.clone());
        let linked = status.as_ref().map_or(0, |s| s.linked_threads);

        let status_card = v_flex()
            .p(theme::sp(3.))
            .gap(theme::sp(2.))
            .rounded(theme::radius_lg())
            .border_1()
            .border_color(p.border)
            .bg(p.panel)
            .child(
                h_flex()
                    .gap(theme::sp(2.5))
                    .items_center()
                    .child(match status.as_ref().and_then(|s| s.username.clone()) {
                        Some(name) if signed_in => crate::presence::avatar(&p, &name, 28., false),
                        _ => div()
                            .size(px(28.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(p.selected)
                            .child(
                                Icon::new(IconName::User)
                                    .with_size(px(14.))
                                    .text_color(p.text_muted),
                            ),
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(theme::text_ui())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(match (&status, signed_in) {
                                        (Some(status), true) => format!(
                                            "Signed in as {}",
                                            status.username.clone().unwrap_or_default()
                                        ),
                                        _ => "Not signed in".to_string(),
                                    }),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme::text_xs())
                                    .font_family(theme::MONO_FONT)
                                    .text_color(p.text_faint)
                                    .child(
                                        status
                                            .as_ref()
                                            .and_then(|s| s.server_url.clone())
                                            .unwrap_or_else(|| "No collaboration server".into()),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap(theme::sp(1.5))
                            .items_center()
                            .text_size(theme::text_xs())
                            .text_color(p.text_muted)
                            .child(
                                div()
                                    .size(px(7.))
                                    .rounded_full()
                                    .bg(runner_color(runner, &p)),
                            )
                            .child(runner_label(runner)),
                    ),
            )
            .when(signed_in, |this| {
                this.child(
                    div()
                        .text_size(theme::text_xs())
                        .text_color(p.text_muted)
                        .child(format!(
                            "{linked} shared thread{} linked on this machine. Your turns in them run here, with your own Claude Code login.",
                            if linked == 1 { "" } else { "s" }
                        )),
                )
            })
            .when_some(last_error, |this, error| {
                this.child(
                    div()
                        .text_size(theme::text_xs())
                        .text_color(p.danger)
                        .child(error),
                )
            })
            .when(status.as_ref().is_some_and(|s| s.server_url.is_some()), |this| {
                this.child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .justify_end()
                        .when(disconnecting, |this| {
                            this.child(Spinner::new().small().color(p.accent))
                        })
                        .child(
                            Button::new("collab-disconnect")
                                .small()
                                .icon(IconName::Unplug)
                                .label("Disconnect")
                                .tooltip("Stop the runner and live updates; stay signed in")
                                .disabled(disconnecting)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.store
                                        .update(cx, |store, cx| store.disconnect(false, cx));
                                })),
                        )
                        .child(
                            Button::new("collab-forget")
                                .small()
                                .danger()
                                .icon(IconName::LogOut)
                                .label("Forget")
                                .tooltip("Sign out and delete the stored tokens. Shared threads keep their local history.")
                                .disabled(disconnecting)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.store
                                        .update(cx, |store, cx| store.disconnect(true, cx));
                                })),
                        ),
                )
            });

        let field = |label: &'static str, input: &Entity<InputState>, id: &'static str| {
            v_flex()
                .gap(theme::sp(1.))
                .child(
                    div()
                        .text_size(theme::text_xs())
                        .text_color(p.text_muted)
                        .child(label),
                )
                .child(Input::new(input).id(id))
        };

        v_flex()
            .gap(theme::sp(4.))
            .child(status_card)
            .child(
                v_flex()
                    .gap(theme::sp(2.5))
                    .child(
                        div()
                            .text_size(theme::text_xs())
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text_faint)
                            .child(if signed_in {
                                "SIGN IN AGAIN OR SWITCH ACCOUNT"
                            } else {
                                "SIGN IN"
                            }),
                    )
                    .child(field("Server URL", &self.server, "collab-server"))
                    .child(
                        h_flex()
                            .gap(theme::sp(2.))
                            .child(
                                div()
                                    .flex_1()
                                    .child(field("Username", &self.username, "collab-username")),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .child(field("Password", &self.password, "collab-password")),
                            ),
                    )
                    .child(field(
                        "Runner token (optional)",
                        &self.runner_token,
                        "collab-runner-token",
                    ))
                    .child(
                        div()
                            .text_size(theme::text_xs())
                            .line_height(relative(1.45))
                            .text_color(p.text_faint)
                            .child("Without a runner token the daemon creates one for you. The password is sent to the daemon once and never stored; only session and runner tokens are kept, owner-only, in the app data folder."),
                    ),
            )
            .when_some(error, |this, error| {
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
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child("Enter to connect \u{00b7} Esc to close"),
                    )
                    .when(connecting, |this| {
                        this.child(Spinner::new().small().color(p.accent))
                    })
                    .child(
                        Button::new("collab-connect")
                            .primary()
                            .label(if signed_in { "Reconnect" } else { "Connect" })
                            .disabled(connecting)
                            .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx))),
                    ),
            )
    }
}

// -- join -------------------------------------------------------------------------

pub enum JoinEvent {
    /// Sign in first (to the link's server, when it names one).
    OpenAccount(Option<String>),
}

pub struct JoinView {
    store: Entity<CollabStore>,
    input: Entity<InputState>,
    /// The project to join into, set when the dialog opens.
    pub workspace: Option<trama_domain::ids::WorkspaceId>,
    pub workspace_title: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<JoinEvent> for JoinView {}

impl JoinView {
    pub fn new(store: Entity<CollabStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("trama-join:http://127.0.0.1:8787/…#…")
        });
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::PressEnter { shift: false, .. } => this.join(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }),
        ];
        Self {
            store,
            input,
            workspace: None,
            workspace_title: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.store.update(cx, |store, _| store.join_error = None);
        self.input.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn parsed(&self, cx: &App) -> Option<Result<JoinTarget, String>> {
        let text = self.input.read(cx).value().to_string();
        (!text.trim().is_empty()).then(|| parse_join(&text))
    }

    fn join(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let target = match self.parsed(cx) {
            Some(Ok(target)) => target,
            Some(Err(message)) => {
                self.store.update(cx, |store, cx| {
                    store.join_error = Some(message);
                    cx.notify();
                });
                return;
            }
            None => return,
        };
        // The dialog stays open (with a spinner) until the daemon answers;
        // the workspace closes it when the joined thread arrives.
        let workspace = self.workspace.clone();
        self.store
            .update(cx, |store, cx| store.join(workspace, &target, cx));
    }
}

impl Render for JoinView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let store = self.store.read(cx);
        let signed_in = store.signed_in();
        let joining = store.is_pending(CollabOp::Join);
        let parsed = self.parsed(cx);
        let error = store.join_error.clone();
        let server = store.server_url().map(str::to_string);
        let link_server = match &parsed {
            Some(Ok(target)) => target.server_url.clone(),
            _ => None,
        };
        let wrong_server = match (&link_server, &server) {
            (Some(link), Some(mine)) => !crate::collab::same_server(link, mine),
            (Some(_), None) => true,
            _ => false,
        };
        let preview = match &parsed {
            Some(Ok(target)) => Some((
                p.text_muted,
                format!(
                    "Thread {} on {}{}",
                    target.thread_id,
                    target
                        .server_url
                        .clone()
                        .or(server.clone())
                        .unwrap_or_else(|| "your server".into()),
                    if target.link_token.is_some() {
                        " · with link token"
                    } else {
                        " · invited access"
                    }
                ),
            )),
            Some(Err(message)) => Some((p.danger, message.clone())),
            None => None,
        };
        let can_join = matches!(parsed, Some(Ok(_))) && signed_in && !joining;

        v_flex()
            .gap(theme::sp(3.))
            .child(
                div()
                    .text_size(theme::text_sm())
                    .line_height(relative(1.5))
                    .text_color(p.text_muted)
                    .child("Paste the link someone copied from their Share dialog. The thread joins this project with its own isolated checkouts; your turns in it run on this machine."),
            )
            .child(Input::new(&self.input).id("join-link"))
            .children(preview.map(|(color, text)| {
                div()
                    .text_size(theme::text_xs())
                    .text_color(color)
                    .child(text)
            }))
            .child(
                h_flex()
                    .gap(theme::sp(1.5))
                    .items_center()
                    .text_size(theme::text_sm())
                    .text_color(p.text_muted)
                    .child(Icon::new(IconName::FolderGit2).with_size(px(13.)))
                    .child(
                        self.workspace_title
                            .clone()
                            .unwrap_or_else(|| "No project selected".into()),
                    ),
            )
            .when(!signed_in || wrong_server, |this| {
                let hint = link_server.clone();
                this.child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .text_size(theme::text_sm())
                                .text_color(p.warning)
                                .child(if signed_in {
                                    "This link is for another collaboration server."
                                } else {
                                    "Sign in to the collaboration server first."
                                }),
                        )
                        .child(
                            Button::new("join-sign-in")
                                .small()
                                .label("Sign in…")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(JoinEvent::OpenAccount(hint.clone()));
                                })),
                        ),
                )
            })
            .when_some(error, |this, error| {
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
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child("Enter to join \u{00b7} Esc to cancel"),
                    )
                    .when(joining, |this| {
                        this.child(Spinner::new().small().color(p.accent))
                    })
                    .child(
                        Button::new("join-submit")
                            .primary()
                            .label("Join thread")
                            .disabled(!can_join)
                            .on_click(cx.listener(|this, _, window, cx| this.join(window, cx))),
                    ),
            )
    }
}
