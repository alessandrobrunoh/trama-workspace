//! GPUI view of the Share wizard; state lives in the collab store.

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
use trama_domain::share::Capability;
use trama_protocol::{ClientMessage, ShareInvite, ShareRole, ShareScope, ShareStatus};

use super::blockers::report_view;
use super::route::WizardOp;
use super::share_flow::{ShareFlow, ShareStep, invite_capabilities};
use super::widgets::{choice, note, section};
use crate::app::theme::{self, Palette};
use crate::collab::CollabStore;

/// Renders `store.vnext.share`; actions go through the collab store.
pub struct ShareWizard {
    store: Entity<CollabStore>,
    invite: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

pub fn scope_text(scope: ShareScope) -> (&'static str, &'static str) {
    match scope {
        ShareScope::ThreadOnly => ("This thread only", "No parent, siblings or children."),
        ShareScope::Subtree => (
            "Thread and descendants",
            "Every child thread, at any depth.",
        ),
        ShareScope::Workspace => (
            "Workspace roots",
            "The workspace's root threads and their descendants.",
        ),
    }
}

pub fn capability_label(cap: Capability) -> &'static str {
    match cap {
        Capability::ReadHistory => "history",
        Capability::ReadCode => "code",
        Capability::PublishTurn => "turns",
        Capability::Comment => "comments",
        Capability::Review => "reviews",
        Capability::Export => "export",
        Capability::ManageShare => "manage",
    }
}

fn status_text(status: ShareStatus) -> &'static str {
    match status {
        ShareStatus::Ready => "Ready",
        ShareStatus::Preparing | ShareStatus::Incomplete => {
            "Uploading the snapshot; joiners wait until it is complete."
        }
        ShareStatus::Paused => "Paused",
        ShareStatus::Closed => "Closed",
    }
}

impl ShareWizard {
    pub fn new(store: Entity<CollabStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let invite =
            cx.new(|cx| InputState::new(window, cx).placeholder("Invite by username, e.g. bob"));
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&invite, window, |this, input, event, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    let raw = input.read(cx).value().to_string();
                    let me = this.store.read(cx).username().map(str::to_owned);
                    let mut added = false;
                    this.act(cx, |f| {
                        match f.add_invites(&raw, me.as_deref()) {
                            Ok(()) => {
                                f.error = None;
                                added = true;
                            }
                            Err(e) => f.error = Some(e),
                        }
                        None
                    });
                    if added {
                        input.update(cx, |i, cx| i.set_value("", window, cx));
                    }
                }
            }),
        ];
        Self {
            store,
            invite,
            _subscriptions: subscriptions,
        }
    }

    /// Clears the invite field for a freshly opened wizard.
    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.invite.update(cx, |i, cx| i.set_value("", window, cx));
    }

    /// Runs an action on the flow and emits the message it returns.
    pub fn act(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut ShareFlow) -> Option<ClientMessage>,
    ) {
        self.store.update(cx, |store, cx| {
            let message = store.vnext.share.as_mut().and_then(f);
            store.dispatch_wizard(WizardOp::Share, message, cx);
        });
    }

    fn scope_step(&self, p: &Palette, flow: &ShareFlow, cx: &mut Context<Self>) -> Div {
        let mut options = v_flex().gap(theme::sp(1.5));
        for scope in [
            ShareScope::ThreadOnly,
            ShareScope::Subtree,
            ShareScope::Workspace,
        ] {
            let (label, detail) = scope_text(scope);
            options = options.child(
                choice(
                    p,
                    SharedString::from(format!("scope-{}", scope.as_str())),
                    flow.scope == scope,
                    label,
                    detail,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.act(cx, |f| {
                        f.set_scope(scope);
                        None
                    })
                })),
            );
        }
        section(p, "What to share", options)
    }

    fn invite_row(&self, p: &Palette, invite: &ShareInvite, cx: &mut Context<Self>) -> Div {
        let name = invite.username.clone();
        let caps = invite_capabilities(invite)
            .into_iter()
            .map(capability_label)
            .collect::<Vec<_>>()
            .join(", ");
        let writer = invite.role == ShareRole::Writer;
        let (n1, n2, n3) = (name.clone(), name.clone(), name.clone());
        h_flex()
            .gap(theme::sp(2.))
            .items_center()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_size(theme::text_sm()).child(name.clone()))
                    .child(
                        div()
                            .text_size(theme::text_xs())
                            .text_color(p.text_muted)
                            .child(format!("Can: {caps}")),
                    ),
            )
            .child(
                Button::new(SharedString::from(format!("role-{name}")))
                    .small()
                    .label(if writer { "Writer" } else { "Reader" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let role = if writer {
                            ShareRole::Reader
                        } else {
                            ShareRole::Writer
                        };
                        this.act(cx, |f| {
                            f.set_role(&n1, role);
                            None
                        })
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("code-{name}")))
                    .small()
                    .when(invite.read_code, |b| b.primary())
                    .label(if invite.read_code {
                        "Code: yes"
                    } else {
                        "Code: no"
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.act(cx, |f| {
                            f.toggle_read_code(&n2);
                            None
                        })
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("remove-{name}")))
                    .small()
                    .ghost()
                    .icon(IconName::Close)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.act(cx, |f| {
                            f.remove_invite(&n3);
                            None
                        })
                    })),
            )
    }

    fn people_step(&self, p: &Palette, flow: &ShareFlow, cx: &mut Context<Self>) -> Div {
        let rows: Vec<_> = flow
            .invites
            .iter()
            .map(|i| self.invite_row(p, i, cx))
            .collect();
        section(p, "People", v_flex().gap(theme::sp(2.)).child(Input::new(&self.invite)).children(rows)).child(note(
            p,
            IconName::Lock,
            p.text_muted,
            "Code is sent only to people you mark \u{201c}Code: yes\u{201d}; everyone clones with their own GitHub access.".into(),
        ))
    }

    fn shared_step(&self, p: &Palette, flow: &ShareFlow, cx: &mut Context<Self>) -> Div {
        let ShareStep::Shared(share) = &flow.step else {
            return div();
        };
        let id = share.share_id.clone();
        let mut requests = v_flex().gap(theme::sp(1.5));
        if flow.requests.is_empty() {
            requests = requests.child(note(
                p,
                IconName::Users,
                p.text_muted,
                "No devices waiting.".into(),
            ));
        }
        for r in &flow.requests {
            let device = r.device_id.clone();
            let busy = flow.approving.is_some();
            requests = requests.child(
                h_flex()
                    .gap(theme::sp(2.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(theme::text_sm())
                            .child(format!("{} · device {}", r.username, r.device_id)),
                    )
                    .child(
                        Button::new(SharedString::from(format!("approve-{device}")))
                            .small()
                            .primary()
                            .label("Admit")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.act(cx, |f| f.approve(&device))
                            })),
                    ),
            );
        }
        v_flex()
            .gap(theme::sp(3.))
            .child(note(
                p,
                IconName::Check,
                p.success,
                status_text(share.status).into(),
            ))
            .child(section(
                p,
                "Share ID",
                h_flex()
                    .gap(theme::sp(2.))
                    .child(div().flex_1().text_size(theme::text_sm()).child(id.clone()))
                    .child(
                        Button::new("share-copy-id")
                            .small()
                            .icon(IconName::Copy)
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(id.clone()))
                            }),
                    ),
            ))
            .child(section(p, "Waiting to join", requests))
            .when_some(flow.error.clone(), |this, e| {
                this.child(note(p, IconName::TriangleAlert, p.danger, e))
            })
    }
}

impl Render for ShareWizard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let Some(flow) = self.store.read(cx).vnext.share.clone() else {
            return div().into_any_element();
        };
        let body: AnyElement = match &flow.step {
            ShareStep::Scope => self.scope_step(&p, &flow, cx).into_any_element(),
            ShareStep::People => self.people_step(&p, &flow, cx).into_any_element(),
            ShareStep::Checking => note(
                &p,
                IconName::RefreshCw,
                p.text_muted,
                "Checking conflicts, secrets and scouts…".into(),
            )
            .into_any_element(),
            ShareStep::Checked(report) => {
                section(&p, "Checks", report_view(&p, report)).into_any_element()
            }
            ShareStep::Creating => note(
                &p,
                IconName::Share,
                p.text_muted,
                "Capturing, encrypting and uploading the snapshot…".into(),
            )
            .into_any_element(),
            ShareStep::Shared(_) => self.shared_step(&p, &flow, cx).into_any_element(),
            ShareStep::Failed(message) => {
                note(&p, IconName::TriangleAlert, p.danger, message.clone()).into_any_element()
            }
        };
        let blocked = matches!(&flow.step, ShareStep::Checked(_)) && flow.primary_label().is_none();
        let busy = matches!(flow.step, ShareStep::Checking | ShareStep::Creating);
        let shared = matches!(flow.step, ShareStep::Shared(_));
        v_flex()
            .gap(theme::sp(4.))
            .child(
                h_flex()
                    .gap(theme::sp(1.5))
                    .items_center()
                    .text_size(theme::text_sm())
                    .text_color(p.text_muted)
                    .child(Icon::new(IconName::Share).with_size(px(13.)))
                    .child(div().min_w_0().truncate().child(flow.title.clone())),
            )
            .child(body)
            .when(flow.step == ShareStep::People, |this| {
                this.when_some(flow.error.clone(), |this, e| {
                    this.child(note(&p, IconName::TriangleAlert, p.danger, e))
                })
            })
            .child(
                h_flex()
                    .gap(theme::sp(2.))
                    .justify_end()
                    .when(busy, |this| {
                        this.child(Spinner::new().small().color(p.accent))
                    })
                    .when(flow.can_go_back(), |this| {
                        this.child(Button::new("share-back").small().label("Back").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.act(cx, |f| {
                                    f.back();
                                    None
                                })
                            }),
                        ))
                    })
                    .when(blocked, |this| {
                        this.child(
                            Button::new("share-recheck")
                                .small()
                                .label("Re-run checks")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.act(cx, ShareFlow::run_checks)
                                })),
                        )
                    })
                    .when(shared, |this| {
                        this.child(
                            Button::new("share-refresh")
                                .small()
                                .label("Refresh")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.act(cx, ShareFlow::refresh_requests)
                                })),
                        )
                    })
                    .when_some(flow.primary_label(), |this, label| {
                        this.child(
                            Button::new("share-next")
                                .primary()
                                .small()
                                .label(label)
                                .disabled(busy)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.act(cx, ShareFlow::next)),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }
}
