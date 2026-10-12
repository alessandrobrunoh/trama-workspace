use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{InputEvent, Textarea, TextareaState},
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::agent::{BillingSource, UsageReport};

use crate::theme;

/// Context window used for the meter. Claude models in use today have a
/// 200k-token window; the daemon does not report it yet.
pub const ASSUMED_CONTEXT_WINDOW: u64 = 200_000;

/// Composer input: Kit `TextareaState` (0.7.1's textarea counterpart of
/// `InputState`) plus a send button. Enter or Cmd+Enter submits; Shift+Enter
/// inserts a newline.
pub struct Composer {
    pub input: Entity<TextareaState>,
}

/// What the footer row shows under the input.
pub struct ComposerFooter {
    pub model: Option<String>,
    pub status_label: &'static str,
    pub status_color: Hsla,
    pub usage: Option<UsageReport>,
    pub cost_usd_micros: Option<u64>,
    pub turns: Option<u32>,
    /// From the agent's `SessionInfo`; `None` until a session reports it.
    pub billing: Option<BillingBadge>,
}

/// Who pays for the session, plus runtime details for the tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingBadge {
    pub billing: BillingSource,
    pub api_key_source: Option<String>,
    pub agent_version: Option<String>,
    pub permission_mode: Option<String>,
}

impl BillingBadge {
    pub fn label(&self) -> &'static str {
        match self.billing {
            BillingSource::Subscription => "Claude subscription",
            BillingSource::ApiKey => "API key",
            BillingSource::Unknown => "Billing unknown",
        }
    }

    /// API-key billing is opt-in and charges the Anthropic API account.
    pub fn warns(&self) -> bool {
        self.billing != BillingSource::Subscription
    }

    pub fn tooltip(&self) -> String {
        let mut lines = vec![match self.billing {
            BillingSource::Subscription => "Turns use your Claude subscription login.".to_string(),
            BillingSource::ApiKey => format!(
                "Turns are billed to the Anthropic API ({}), not your Claude subscription.",
                self.api_key_source.as_deref().unwrap_or("API key")
            ),
            BillingSource::Unknown => format!(
                "Claude reported an unrecognized credential source ({}). It may bill the API.",
                self.api_key_source.as_deref().unwrap_or("unknown")
            ),
        }];
        if let Some(version) = &self.agent_version {
            lines.push(format!("Claude Code {version}"));
        }
        if let Some(mode) = &self.permission_mode {
            lines.push(format!("Permission mode: {mode}"));
        }
        lines.join("\n")
    }
}

impl Composer {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Ask the agent to do something…")
                .rows(3)
                .submit_on_enter(true)
        });
        Self { input }
    }

    pub fn take_prompt(&self, window: &mut Window, cx: &mut App) -> Option<String> {
        let mut prompt = None;
        self.input.update(cx, |input, cx| {
            let value = input.value().to_string();
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                prompt = Some(trimmed.to_string());
                input.set_value("", window, cx);
            }
        });
        prompt
    }

    pub fn value(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    pub fn set_prompt(&self, prompt: &str, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |input, cx| {
            input.set_value(prompt.to_string(), window, cx);
        });
    }

    pub fn set_disabled(&self, disabled: bool, cx: &mut App) {
        self.input.update(cx, |input, cx| {
            input.set_disabled(disabled, cx);
        });
    }

    pub fn subscribe_enter<V: 'static>(
        &self,
        window: &mut Window,
        cx: &mut Context<V>,
        mut on_enter: impl FnMut(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Subscription {
        cx.subscribe_in(&self.input, window, move |this, _, event, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                on_enter(this, window, cx);
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        running: bool,
        stoppable: bool,
        footer: ComposerFooter,
        on_send: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_stop: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        window: &Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let p = theme::palette(cx);
        let can_send = !running && !self.input.read(cx).value().trim().is_empty();
        let focused = self.input.focus_handle(cx).contains_focused(window, cx);

        let context_tokens = footer
            .usage
            .as_ref()
            .map(|u| u.input_tokens + u.cache_read_tokens + u.cache_write_tokens);
        let fraction = context_tokens
            .map(|tokens| (tokens as f32 / ASSUMED_CONTEXT_WINDOW as f32).clamp(0., 1.))
            .unwrap_or(0.);
        let meter_color = if fraction > 0.85 {
            p.danger
        } else if fraction > 0.6 {
            p.warning
        } else {
            p.accent
        };
        let usage_tooltip: SharedString = match &footer.usage {
            Some(u) => format!(
                "Context {} of an assumed {} window. Input {}, output {}, cache read {}, cache write {}.",
                format_tokens(context_tokens.unwrap_or(0)),
                format_tokens(ASSUMED_CONTEXT_WINDOW),
                format_tokens(u.input_tokens),
                format_tokens(u.output_tokens),
                format_tokens(u.cache_read_tokens),
                format_tokens(u.cache_write_tokens),
            )
            .into(),
            None => "No usage reported yet for this thread".into(),
        };

        let pill = |content: AnyElement| {
            h_flex()
                .h(px(22.))
                .px(theme::sp(2.))
                .gap(theme::sp(1.5))
                .items_center()
                .rounded(theme::radius())
                .text_size(theme::text_xs())
                .text_color(p.text_muted)
                .child(content)
        };

        let meter_width = if context_tokens.is_some() {
            fraction.max(0.04)
        } else {
            0.
        };

        let footer_row = h_flex()
            .w_full()
            .px(theme::sp(1.5))
            .pb(theme::sp(1.5))
            .gap(theme::sp(1.))
            .items_center()
            .child(pill(
                h_flex()
                    .gap(theme::sp(1.5))
                    .items_center()
                    .child(Icon::new(IconName::Cpu).with_size(px(12.)))
                    .child(footer.model.clone().unwrap_or_else(|| "Model —".into()))
                    .into_any_element(),
            ))
            .child(pill(
                h_flex()
                    .gap(theme::sp(1.5))
                    .items_center()
                    .child(div().size(px(6.)).rounded_full().bg(footer.status_color))
                    .child(match footer.turns {
                        Some(turns) => format!(
                            "{} · {turns} turn{}",
                            footer.status_label,
                            if turns == 1 { "" } else { "s" }
                        ),
                        None => footer.status_label.to_string(),
                    })
                    .into_any_element(),
            ))
            .when_some(footer.billing.clone(), |this, badge| {
                let color = if badge.warns() {
                    p.warning
                } else {
                    p.text_muted
                };
                let tooltip: SharedString = badge.tooltip().into();
                this.child(
                    h_flex()
                        .id("composer-billing")
                        .h(px(22.))
                        .px(theme::sp(2.))
                        .gap(theme::sp(1.5))
                        .items_center()
                        .rounded(theme::radius())
                        .text_size(theme::text_xs())
                        .text_color(color)
                        .when(badge.warns(), |this| this.bg(p.warning.opacity(0.12)))
                        .hover(|style| style.bg(p.hover))
                        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                        .child(
                            Icon::new(if badge.warns() {
                                IconName::KeyRound
                            } else {
                                IconName::UserCheck
                            })
                            .with_size(px(12.)),
                        )
                        .child(badge.label())
                        .when_some(badge.permission_mode.clone(), |this, mode| {
                            this.when(mode != "default", |this| {
                                this.child(div().text_color(p.text_faint).child(mode))
                            })
                        }),
                )
            })
            .child(div().flex_1())
            .child(
                h_flex()
                    .id("composer-usage")
                    .h(px(22.))
                    .px(theme::sp(2.))
                    .gap(theme::sp(2.))
                    .items_center()
                    .rounded(theme::radius())
                    .text_size(theme::text_xs())
                    .font_family(theme::MONO_FONT)
                    .text_color(p.text_muted)
                    .hover(|style| style.bg(p.hover))
                    .tooltip(move |window, cx| {
                        Tooltip::new(usage_tooltip.clone()).build(window, cx)
                    })
                    .child(
                        div()
                            .w(px(36.))
                            .h(px(4.))
                            .rounded_full()
                            .bg(p.border_strong)
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(meter_color)
                                    .w(relative(meter_width)),
                            ),
                    )
                    .child(match context_tokens {
                        Some(tokens) => format!("{} ctx", format_tokens(tokens)),
                        None => "— ctx".into(),
                    })
                    .child(
                        div().text_color(p.text_faint).child(
                            footer
                                .cost_usd_micros
                                .map(format_cost)
                                .unwrap_or_else(|| "$—".into()),
                        ),
                    ),
            )
            .when(stoppable, |this| {
                this.child(
                    Button::new("stop-message")
                        .small()
                        .icon(IconName::Square)
                        .label("Stop")
                        .tooltip("Interrupt the running turn")
                        .on_click(on_stop),
                )
            })
            .when(!stoppable, |this| {
                this.child(
                    Button::new("send-message")
                        .primary()
                        .small()
                        .icon(IconName::ArrowUp)
                        .tooltip("Send (Enter) · \u{2318}\u{21e7}Enter sends to a new subthread")
                        .disabled(!can_send)
                        .on_click(on_send),
                )
            });

        v_flex()
            .w_full()
            .rounded(theme::radius_lg())
            .border_1()
            .border_color(if focused { p.border_strong } else { p.border })
            .bg(p.elevated)
            .child(
                div()
                    .w_full()
                    .px(theme::sp(1.))
                    .pt(theme::sp(1.))
                    .text_size(theme::text_prose())
                    .child(Textarea::new(&self.input).appearance(false).h(px(64.))),
            )
            .child(footer_row)
    }
}

pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

pub fn format_cost(micros: u64) -> String {
    let dollars = micros as f64 / 1_000_000.0;
    if dollars >= 1.0 {
        format!("${dollars:.2}")
    } else {
        format!("${dollars:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::{BillingBadge, BillingSource, format_cost, format_tokens};

    #[test]
    fn api_key_billing_warns_and_explains_itself() {
        let mut badge = BillingBadge {
            billing: BillingSource::Subscription,
            api_key_source: Some("none".into()),
            agent_version: Some("2.1.0".into()),
            permission_mode: Some("default".into()),
        };
        assert!(!badge.warns());
        assert_eq!(badge.label(), "Claude subscription");
        assert!(badge.tooltip().contains("Claude Code 2.1.0"));
        badge.billing = BillingSource::ApiKey;
        badge.api_key_source = Some("ANTHROPIC_API_KEY".into());
        assert!(badge.warns());
        assert_eq!(badge.label(), "API key");
        assert!(
            badge
                .tooltip()
                .contains("billed to the Anthropic API (ANTHROPIC_API_KEY)")
        );
        badge.billing = BillingSource::Unknown;
        assert!(badge.warns());
    }

    #[test]
    fn formats_tokens_and_cost_compactly() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(9_700), "9.7k");
        assert_eq!(format_tokens(1_340_000), "1.3M");
        assert_eq!(format_cost(12_400), "$0.0124");
        assert_eq!(format_cost(2_500_000), "$2.50");
    }
}
