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

use super::WizardRequest;
use super::join_flow::{JOIN_STAGES, JoinFlow, JoinStage, JoinStep, StageMark, stage_label};
use super::widgets::{note, section};
use crate::app::theme::{self, Palette};

pub struct JoinWizard {
    pub flow: Option<JoinFlow>,
    input: Entity<InputState>,
}

impl EventEmitter<WizardRequest> for JoinWizard {}

impl JoinWizard {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Share ID from the owner"));
        Self { flow: None, input }
    }

    pub fn open(&mut self, flow: JoinFlow, cx: &mut Context<Self>) {
        self.flow = Some(flow);
        cx.notify();
    }

    pub fn act(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut JoinFlow) -> Option<ClientMessage>,
    ) {
        let Some(flow) = self.flow.as_mut() else {
            return;
        };
        if let Some(message) = f(flow) {
            cx.emit(WizardRequest(message));
        }
        cx.notify();
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
        let Some(flow) = self.flow.clone() else {
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
