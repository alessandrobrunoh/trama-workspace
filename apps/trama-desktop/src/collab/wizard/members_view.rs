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

/// "writer · code · 2 devices" from the server roster, when loaded.
pub fn member_detail(flow: &ShareFlow, username: &str) -> Option<String> {
    let m = flow.member_info(username)?;
    let role = match m.role {
        trama_protocol::ShareRole::Writer => "writer",
        trama_protocol::ShareRole::Reader => "reader",
        trama_protocol::ShareRole::Owner => "owner",
    };
    let mut parts = vec![role.to_owned()];
    if m.read_code {
        parts.push("code".into());
    }
    parts.push(match m.devices.len() {
        0 => "not joined yet".into(),
        1 => "1 device".into(),
        n => format!("{n} devices"),
    });
    Some(parts.join(" · "))
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
        if let Some(why) = &flow.ended {
            return section(
                p,
                "People with access",
                note(p, IconName::Lock, p.warning, why.clone()),
            );
        }
        for name in &flow.members {
            let detail = member_detail(flow, name);
            let removing = flow.removing.as_deref() == Some(name.as_str());
            let n = name.clone();
            rows = rows.child(
                h_flex()
                    .gap(theme::sp(2.))
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_size(theme::text_sm()).child(name.clone()))
                            .children(detail.map(|d| {
                                div()
                                    .text_size(theme::text_xs())
                                    .text_color(p.text_muted)
                                    .child(d)
                            })),
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
