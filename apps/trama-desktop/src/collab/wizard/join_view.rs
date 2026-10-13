//! GPUI view of the Join wizard: link input, then step-by-step progress.

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

use super::join_flow::{JOIN_STEPS, JoinFlow, JoinRemedy, StepMark, step_label};
use super::model::WizardCommand;
use super::widgets::{note, section};
use crate::app::theme::{self, Palette};

pub struct JoinWizard {
    pub flow: JoinFlow,
    input: Entity<InputState>,
}

impl EventEmitter<WizardCommand> for JoinWizard {}

impl JoinWizard {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Paste the share link"));
        Self {
            flow: JoinFlow::default(),
            input,
        }
    }

    pub fn update_flow(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut JoinFlow, &mut Vec<WizardCommand>),
    ) {
        let mut out = Vec::new();
        f(&mut self.flow, &mut out);
        for command in out {
            cx.emit(command);
        }
        cx.notify();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let link = self.input.read(cx).value().to_string();
        self.update_flow(cx, |f, out| {
            f.start(&link, out);
        });
    }

    fn step_row(&self, p: &Palette, step: trama_domain::share::JoinState) -> Div {
        let mark = self.flow.mark(step);
        let (icon, color) = match mark {
            StepMark::Done => (IconName::Check, p.success),
            StepMark::Current => (IconName::RefreshCw, p.accent),
            StepMark::Failed => (IconName::CircleX, p.danger),
            StepMark::Pending => (IconName::ChevronRight, p.text_faint),
        };
        h_flex()
            .gap(theme::sp(2.))
            .items_center()
            .text_size(theme::text_sm())
            .text_color(if mark == StepMark::Pending {
                p.text_faint
            } else {
                p.text
            })
            .child(Icon::new(icon).with_size(px(12.)).text_color(color))
            .child(step_label(step))
    }
}

impl Render for JoinWizard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let Some(state) = self.flow.state else {
            return v_flex()
                .gap(theme::sp(3.))
                .child(section(&p, "Join a share", Input::new(&self.input)))
                .when_some(self.flow.error.clone(), |this, e| {
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
        };
        let steps = v_flex()
            .gap(theme::sp(1.5))
            .children(JOIN_STEPS.map(|s| self.step_row(&p, s)));
        let remedy = self.flow.remedy();
        v_flex()
            .gap(theme::sp(3.))
            .child(section(&p, "Joining", steps))
            .when(state.is_error(), |this| {
                this.child(note(
                    &p,
                    IconName::TriangleAlert,
                    p.danger,
                    step_label(state).into(),
                ))
            })
            .when_some(self.flow.detail.clone(), |this, d| {
                this.child(note(&p, IconName::Info, p.text_muted, d))
            })
            .when(self.flow.is_ready(), |this| {
                this.child(note(
                    &p,
                    IconName::Check,
                    p.success,
                    "The shared threads are ready in your sidebar.".into(),
                ))
            })
            .child(
                h_flex()
                    .gap(theme::sp(2.))
                    .justify_end()
                    .when(!state.is_error() && !self.flow.is_ready(), |this| {
                        this.child(Spinner::new().small().color(p.accent))
                    })
                    .when(!self.flow.is_ready(), |this| {
                        this.child(Button::new("join-cancel").small().label("Cancel").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.update_flow(cx, |f, out| f.cancel(out))
                            }),
                        ))
                    })
                    .when(remedy == Some(JoinRemedy::Retry), |this| {
                        this.child(
                            Button::new("join-retry")
                                .primary()
                                .small()
                                .label("Retry")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.update_flow(cx, |f, out| f.retry(out))
                                })),
                        )
                    })
                    .when(remedy == Some(JoinRemedy::ConnectGitHub), |this| {
                        this.child(
                            Button::new("join-github")
                                .primary()
                                .small()
                                .label("Connect GitHub")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.update_flow(cx, |f, out| f.connect_github(out))
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
}
