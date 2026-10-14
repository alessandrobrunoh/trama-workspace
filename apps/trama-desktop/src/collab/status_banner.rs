//! Transcript banners for a share's persisted status: warnings such as
//! "published without code", checkout restore failures with a
//! "Retry restore" button and "Update available — Apply".

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
};
use gpui_kit::*;

use crate::app::theme::{self, Palette};
use crate::collab::vnext::{StatusView, warning_text};

/// Restore failure text, `None` when the restore did not fail.
pub fn restore_text(status: &StatusView) -> Option<String> {
    let reason = status.restore_failed.as_ref()?;
    let mut text = if reason.is_empty() {
        "Could not restore the shared code.".to_owned()
    } else {
        format!("Could not restore the shared code: {reason}")
    };
    if let Some(error) = &status.retry_error {
        text.push_str(&format!(" (retry failed: {error})"));
    }
    Some(text)
}

/// "Update available" text, `None` when there is no pending update.
pub fn update_text(status: &StatusView) -> Option<String> {
    status.update_available?;
    let mut text = "Update available: the shared code changed.".to_owned();
    if let Some(error) = &status.update_error {
        text.push_str(&format!(" (not applied: {error})"));
    }
    Some(text)
}

/// Banners for one share, newest warning first, restore failure on top.
pub fn banners(
    p: &Palette,
    status: &StatusView,
    on_retry: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_apply: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Vec<AnyElement> {
    let mut out = Vec::new();
    if let Some(text) = update_text(status) {
        let action = if status.updating {
            div()
                .text_color(p.text_muted)
                .child("Applying…")
                .into_any_element()
        } else {
            Button::new("apply-share-update")
                .xsmall()
                .ghost()
                .label("Apply")
                .on_click(on_apply)
                .into_any_element()
        };
        out.push(banner(p, IconName::Info, text, Some(action)));
    }
    if let Some(text) = restore_text(status) {
        let action = if status.retrying {
            div()
                .text_color(p.text_muted)
                .child("Retrying…")
                .into_any_element()
        } else {
            Button::new("retry-restore")
                .xsmall()
                .ghost()
                .label("Retry restore")
                .on_click(on_retry)
                .into_any_element()
        };
        out.push(banner(p, IconName::TriangleAlert, text, Some(action)));
    }
    for warning in &status.warnings {
        out.push(banner(p, IconName::Info, warning_text(warning), None));
    }
    out
}

fn banner(p: &Palette, icon: IconName, text: String, action: Option<AnyElement>) -> AnyElement {
    let mut row = h_flex()
        .w_full()
        .px(theme::sp(3.))
        .py(theme::sp(2.))
        .gap(theme::sp(2.))
        .items_center()
        .rounded(theme::radius())
        .border_1()
        .border_color(p.warning.opacity(0.30))
        .bg(p.warning.opacity(0.08))
        .text_size(theme::text_sm())
        .text_color(p.text)
        .child(Icon::new(icon).with_size(px(14.)).text_color(p.warning))
        .child(div().flex_1().child(text));
    if let Some(action) = action {
        row = row.child(action);
    }
    row.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{StatusView, restore_text, update_text};

    #[test]
    fn update_text_only_when_offered() {
        let mut status = StatusView::default();
        assert_eq!(update_text(&status), None);
        status.update_available = Some(3);
        status.update_error = Some("local changes".into());
        assert_eq!(
            update_text(&status).unwrap(),
            "Update available: the shared code changed. (not applied: local changes)"
        );
    }

    #[test]
    fn restore_text_covers_reason_and_retry_error() {
        let mut status = StatusView::default();
        assert_eq!(restore_text(&status), None);
        status.restore_failed = Some(String::new());
        assert_eq!(
            restore_text(&status).unwrap(),
            "Could not restore the shared code."
        );
        status.restore_failed = Some("dirty index".into());
        status.retry_error = Some("offline".into());
        assert_eq!(
            restore_text(&status).unwrap(),
            "Could not restore the shared code: dirty index (retry failed: offline)"
        );
    }
}
