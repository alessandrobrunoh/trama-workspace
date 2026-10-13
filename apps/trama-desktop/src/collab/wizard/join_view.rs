//! GPUI view of the Join wizard: share id, then staged progress.

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_protocol::ClientMessage;

use super::join_flow::{JOIN_STAGES, JoinFlow, JoinStage, JoinStep, StageMark, stage_label};
use super::route::WizardOp;
use super::widgets::{note, section};
use crate::app::theme::{self, Palette};
use crate::collab::CollabStore;

/// Renders `store.vnext.join`; actions go through the collab store.
pub struct JoinWizard {
    store: Entity<CollabStore>,
    input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl JoinWizard {
    pub fn new(store: Entity<CollabStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Share ID from the owner"));
        let subscriptions = vec![cx.observe(&store, |_, _, cx| cx.notify())];
        Self {
            store,
            input,
            _subscriptions: subscriptions,
        }
    }

    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| i.set_value("", window, cx));
        self.input.read(cx).focus_handle(cx).focus(window, cx);
    }

    pub fn act(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut JoinFlow) -> Option<ClientMessage>,
    ) {
        self.store.update(cx, |store, cx| {
            let message = store.vnext.join.as_mut().and_then(f);
            store.dispatch_wizard(WizardOp::Join, message, cx);
        });
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let raw = self.input.read(cx).value().to_string();
        self.act(cx, |f| f.start(&raw));
    }

    fn stage_row(p: &Palette, flow: &JoinFlow, stage: JoinStage) -> Div {
        let mark = flow.mark(stage);
        let (icon, color) = match mark {
            StageMark::Done => (IconName::Check, p.success),
            StageMark::Current => (IconName::RefreshCw, p.accent),
            StageMark::Failed => (IconName::CircleX, p.danger),
            StageMark::Pending => (IconName::ChevronRight, p.text_faint),
        };
        h_flex()
            .gap(theme::sp(2.))
            .items_center()
            .text_size(theme::text_sm())
            .text_color(if mark == StageMark::Pending {
                p.text_faint
            } else {
                p.text
            })
            .child(Icon::new(icon).with_size(px(12.)).text_color(color))
            .child(stage_label(stage))
    }
}

impl Render for JoinWizard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let Some(flow) = self.store.read(cx).vnext.join.clone() else {
            return div().into_any_element();
        };
        let entering = matches!(flow.step, JoinStep::Enter);
        if entering {
            return v_flex()
                .gap(theme::sp(3.))
                .child(section(&p, "Join a share", Input::new(&self.input)))
                .when_some(flow.error.clone(), |this, e| {
                    this.child(note(&p, IconName::TriangleAlert, p.danger, e))
                })
                .child(
                    h_flex().justify_end().child(
                        Button::new("join-start")
                            .primary()
                            .small()
                            .label("Join")
                            .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
                    ),
                )
                .into_any_element();
        }
        let stages = v_flex()
            .gap(theme::sp(1.5))
            .children(JOIN_STAGES.map(|s| Self::stage_row(&p, &flow, s)));
        let busy = matches!(
            flow.step,
            JoinStep::Requesting | JoinStep::Completing { .. }
        );
        let message = match &flow.step {
            JoinStep::AwaitingApproval { device_id } => Some(note(
                &p,
                IconName::Users,
                p.text_muted,
                format!("Ask the owner to admit device {device_id}, then check again."),
            )),
            JoinStep::RequestFailed(e) => {
                Some(note(&p, IconName::TriangleAlert, p.danger, e.clone()))
            }
            JoinStep::Joined(_) => Some(note(
                &p,
                IconName::Check,
                p.success,
                "The shared threads are in your sidebar.".into(),
            )),
            _ => None,
        };
        v_flex()
            .gap(theme::sp(3.))
            .child(section(&p, &format!("Joining {}", flow.share_id), stages))
            .children(message)
            .when_some(flow.error.clone(), |this, e| {
                this.child(note(&p, IconName::TriangleAlert, p.warning, e))
            })
            .child(
                h_flex()
                    .gap(theme::sp(2.))
                    .justify_end()
                    .when(busy, |this| {
                        this.child(Spinner::new().small().color(p.accent))
                    })
                    .when(!matches!(flow.step, JoinStep::Joined(_)) && !busy, |this| {
                        this.child(
                            Button::new("join-reset")
                                .small()
                                .label("Start over")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.act(cx, |f| {
                                        f.reset();
                                        None
                                    })
                                })),
                        )
                    })
                    .when(matches!(flow.step, JoinStep::RequestFailed(_)), |this| {
                        this.child(
                            Button::new("join-retry")
                                .primary()
                                .small()
                                .label("Retry")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.act(cx, JoinFlow::retry_request)
                                })),
                        )
                    })
                    .when(
                        matches!(flow.step, JoinStep::AwaitingApproval { .. }),
                        |this| {
                            this.child(
                                Button::new("join-check")
                                    .primary()
                                    .small()
                                    .label("Check again")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.act(cx, JoinFlow::complete)
                                    })),
                            )
                        },
                    ),
            )
            .into_any_element()
    }
}
