//! Shared step: people with access, with a confirmed "Remove".

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::share_flow::ShareFlow;
use super::share_view::ShareWizard;
use super::widgets::{note, section};
use crate::app::theme::{self, Palette};

/// Confirmation text: what removal does, so it is never a surprise.
pub fn remove_warning(username: &str) -> String {
    format!(
        "Remove {username}? Their grant is revoked and every device of theirs leaves the \
         encryption group; new content uses a new key. What they already downloaded stays with them."
    )
}

impl ShareWizard {
    pub(super) fn members_section(
        &self,
        p: &Palette,
        flow: &ShareFlow,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut rows = v_flex().gap(theme::sp(1.5));
        if flow.members.is_empty() {
            rows = rows.child(note(
                p,
                IconName::Users,
                p.text_muted,
                "Only you so far.".into(),
            ));
        }
        let busy = flow.removing.is_some();
        for name in &flow.members {
            let removing = flow.removing.as_deref() == Some(name.as_str());
            let n = name.clone();
            rows = rows.child(
                h_flex()
                    .gap(theme::sp(2.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(theme::text_sm())
                            .child(name.clone()),
                    )
                    .child(
                        Button::new(SharedString::from(format!("member-remove-{name}")))
                            .small()
                            .ghost()
                            .label(if removing { "Removing…" } else { "Remove" })
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.act(cx, |f| {
                                    f.ask_remove(&n);
                                    None
                                })
                            })),
                    ),
            );
        }
        let confirm = flow.confirm_remove.clone().map(|name| {
            v_flex()
                .gap(theme::sp(2.))
                .p(theme::sp(2.))
                .rounded(theme::radius())
                .border_1()
                .border_color(p.danger)
                .child(note(
                    p,
                    IconName::TriangleAlert,
                    p.danger,
                    remove_warning(&name),
                ))
                .child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .justify_end()
                        .child(
                            Button::new("member-remove-cancel")
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.act(cx, |f| {
                                        f.cancel_remove();
                                        None
                                    })
                                })),
                        )
                        .child(
                            Button::new("member-remove-confirm")
                                .small()
                                .danger()
                                .label("Remove")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.act(cx, ShareFlow::confirm_remove)
                                })),
                        ),
                )
        });
        section(p, "People with access", rows).when_some(confirm, |this, c| this.child(c))
    }
}
