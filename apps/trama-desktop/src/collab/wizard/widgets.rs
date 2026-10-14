//! Small shared building blocks for the wizard views.

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::theme::{self, Palette};

pub fn section(p: &Palette, title: &str, body: impl IntoElement) -> Div {
    v_flex()
        .gap(theme::sp(2.))
        .child(
            div()
                .text_size(theme::text_xs())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text_faint)
                .child(title.to_uppercase()),
        )
        .child(body)
}

/// A selectable option card (radio-like). The caller attaches `on_click`.
pub fn choice(
    p: &Palette,
    id: impl Into<ElementId>,
    selected: bool,
    label: &str,
    detail: &str,
) -> Stateful<Div> {
    h_flex()
        .id(id)
        .gap(theme::sp(2.))
        .items_start()
        .p(theme::sp(2.))
        .rounded(theme::radius())
        .border_1()
        .border_color(if selected { p.accent } else { p.border })
        .bg(if selected { p.accent_soft } else { p.panel })
        .cursor_pointer()
        .hover(|s| s.bg(p.hover))
        .child(
            Icon::new(if selected {
                IconName::Check
            } else {
                IconName::ChevronRight
            })
            .with_size(px(12.))
            .text_color(if selected { p.accent } else { p.text_faint }),
        )
        .child(
            v_flex()
                .min_w_0()
                .child(div().text_size(theme::text_sm()).child(label.to_owned()))
                .child(
                    div()
                        .text_size(theme::text_xs())
                        .text_color(p.text_muted)
                        .child(detail.to_owned()),
                ),
        )
}

/// One blocker line with a danger icon.
pub fn blocker(p: &Palette, title: String, detail: Option<String>) -> Div {
    h_flex()
        .gap(theme::sp(2.))
        .items_start()
        .child(
            Icon::new(IconName::CircleX)
                .with_size(px(12.))
                .text_color(p.danger),
        )
        .child(
            v_flex()
                .min_w_0()
                .child(div().text_size(theme::text_sm()).child(title))
                .children(detail.map(|d| {
                    div()
                        .text_size(theme::text_xs())
                        .text_color(p.text_muted)
                        .child(d)
                })),
        )
}

pub fn note(p: &Palette, icon: IconName, color: Hsla, text: String) -> Div {
    h_flex()
        .gap(theme::sp(1.5))
        .items_center()
        .text_size(theme::text_xs())
        .text_color(color)
        .child(Icon::new(icon).with_size(px(12.)))
        .child(div().min_w_0().text_color(p.text_muted).child(text))
}

/// Numbered, labeled step indicator ("1 Scope › 2 People › …"); steps
/// before `current` show a check.
pub fn steps(p: &Palette, labels: &[&str], current: usize) -> Div {
    let mut row = h_flex().gap(theme::sp(1.5)).items_center().flex_wrap();
    for (ix, label) in labels.iter().enumerate() {
        if ix > 0 {
            row = row.child(
                Icon::new(IconName::ChevronRight)
                    .with_size(px(11.))
                    .text_color(p.text_faint),
            );
        }
        let done = ix < current;
        let active = ix == current;
        let badge = div()
            .size(px(16.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .text_size(px(10.))
            .font_weight(FontWeight::SEMIBOLD);
        let badge = if done {
            badge.bg(p.success.opacity(0.15)).child(
                Icon::new(IconName::Check)
                    .with_size(px(10.))
                    .text_color(p.success),
            )
        } else if active {
            badge
                .bg(p.accent)
                .text_color(gpui_kit::white())
                .child((ix + 1).to_string())
        } else {
            badge
                .border_1()
                .border_color(p.border_strong)
                .text_color(p.text_faint)
                .child((ix + 1).to_string())
        };
        row = row.child(
            h_flex()
                .gap(theme::sp(1.5))
                .items_center()
                .text_size(theme::text_sm())
                .text_color(if active {
                    p.text
                } else if done {
                    p.text_muted
                } else {
                    p.text_faint
                })
                .when(active, |this| this.font_weight(FontWeight::MEDIUM))
                .child(badge)
                .child(label.to_string()),
        );
    }
    row
}
