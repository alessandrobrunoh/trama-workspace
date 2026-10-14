//! `@` mentions: the chip a picked mention becomes in the composer, and the
//! mapping between chips and `SendPrompt.mentions`.
//!
//! A picked item is an atomic inline token over the text `@label`. Its token
//! id carries the structured mention (`<kind>:<opaque value>`), so deleting
//! the chip removes the mention and nothing else needs to be tracked.

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Sizable as _,
    input::{InlineToken, InlineTokenContext, InlineTokenSpan},
};
use gpui_kit::*;
use trama_protocol::{MentionKind, Mentionable, PromptMention};

use crate::app::theme;

pub fn kind_name(kind: MentionKind) -> &'static str {
    match kind {
        MentionKind::File => "file",
        MentionKind::Directory => "directory",
        MentionKind::Thread => "thread",
        MentionKind::Person => "person",
    }
}

fn kind_from_name(name: &str) -> Option<MentionKind> {
    Some(match name {
        "file" => MentionKind::File,
        "directory" => MentionKind::Directory,
        "thread" => MentionKind::Thread,
        "person" => MentionKind::Person,
        _ => return None,
    })
}

pub fn kind_icon(kind: MentionKind) -> IconName {
    match kind {
        MentionKind::File => IconName::File,
        MentionKind::Directory => IconName::Folder,
        MentionKind::Thread => IconName::MessageSquare,
        MentionKind::Person => IconName::User,
    }
}

/// Token id for a mention: `<kind>:<value>`. The value may contain `:`.
pub fn token_id(kind: MentionKind, value: &str) -> String {
    format!("{}:{value}", kind_name(kind))
}

/// The mention a token id stands for; `None` for tokens that are not ours.
pub fn mention_from_token_id(id: &str) -> Option<PromptMention> {
    let (kind, value) = id.split_once(':')?;
    let kind = kind_from_name(kind)?;
    (!value.is_empty()).then(|| PromptMention {
        kind,
        value: value.to_string(),
    })
}

/// Text a mention occupies in the prompt. Labels never contain control
/// characters (inline tokens refuse them), so newlines become spaces.
pub fn token_text(label: &str) -> String {
    let label: String = label
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("@{}", label.trim())
}

/// The chip for a picked completion.
pub fn token_for(item: &Mentionable) -> InlineToken {
    let text = token_text(&item.label);
    InlineToken::new(token_id(item.kind, &item.value), text.clone())
        .with_label(text.trim_start_matches('@').to_string())
}

/// Mentions carried by the composer's chips, in order, without repeats.
pub fn mentions_from_tokens(spans: &[InlineTokenSpan]) -> Vec<PromptMention> {
    mentions_from_ids(spans.iter().map(|span| span.token().id().as_ref()))
}

pub fn mentions_from_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Vec<PromptMention> {
    let mut out: Vec<PromptMention> = Vec::new();
    for mention in ids.into_iter().filter_map(mention_from_token_id) {
        if !out.contains(&mention) {
            out.push(mention);
        }
    }
    out
}

/// How a mention chip looks inside the composer text.
pub fn render_token(context: &InlineTokenContext, _: &mut Window, cx: &mut App) -> AnyElement {
    let p = theme::palette(cx);
    let kind = mention_from_token_id(context.token().id())
        .map(|m| m.kind)
        .unwrap_or(MentionKind::File);
    let selected = context.is_selected();
    div()
        .flex()
        .items_center()
        .gap(px(3.))
        .px(px(4.))
        .h(context.line_height())
        .max_w(context.available_width())
        .rounded(theme::radius_sm())
        .bg(if selected {
            p.accent.opacity(0.32)
        } else {
            p.accent_soft
        })
        .text_color(p.accent_text)
        .child(
            Icon::new(kind_icon(kind))
                .with_size(px(11.))
                .flex_shrink_0(),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .child(context.token().label().clone()),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` brings GPUI's own `test` attribute into scope.
    use core::prelude::v1::test;
    use gpui_kit::component::input::InputContent;

    fn item(kind: MentionKind, label: &str, value: &str) -> Mentionable {
        Mentionable {
            kind,
            label: label.into(),
            detail: None,
            value: value.into(),
        }
    }

    #[test]
    fn token_ids_round_trip_values_with_colons() {
        let id = token_id(MentionKind::File, "01REPO:src/lib.rs");
        assert_eq!(id, "file:01REPO:src/lib.rs");
        assert_eq!(
            mention_from_token_id(&id),
            Some(PromptMention {
                kind: MentionKind::File,
                value: "01REPO:src/lib.rs".into()
            })
        );
        assert_eq!(mention_from_token_id("bogus:x"), None);
        assert_eq!(mention_from_token_id("thread:"), None);
        assert_eq!(mention_from_token_id("plain"), None);
    }

    #[test]
    fn picked_item_becomes_an_at_label_chip() {
        let token = token_for(&item(MentionKind::Thread, "Auth\nrefactor", "01THREAD"));
        assert_eq!(token.text().as_ref(), "@Auth refactor");
        assert_eq!(token.label().as_ref(), "Auth refactor");
        assert_eq!(token.id().as_ref(), "thread:01THREAD");
    }

    #[test]
    fn chips_map_to_mentions_and_removing_a_chip_drops_its_mention() {
        let file = token_for(&item(MentionKind::File, "src/lib.rs", "r:src/lib.rs"));
        let person = token_for(&item(MentionKind::Person, "Alessandro", "local"));
        let text = format!(
            "fix {} with {} and {}",
            file.text(),
            person.text(),
            file.text()
        );
        let first = 4..4 + file.text().len();
        let second_start = first.end + 6;
        let second = second_start..second_start + person.text().len();
        let third_start = second.end + 5;
        let third = third_start..third_start + file.text().len();
        let content = InputContent::new(text.clone())
            .with_token(first.clone(), file.clone())
            .unwrap()
            .with_token(second.clone(), person.clone())
            .unwrap()
            .with_token(third.clone(), file.clone())
            .unwrap();
        let mentions = mentions_from_tokens(content.tokens());
        assert_eq!(
            mentions,
            vec![
                PromptMention {
                    kind: MentionKind::File,
                    value: "r:src/lib.rs".into()
                },
                PromptMention {
                    kind: MentionKind::Person,
                    value: "local".into()
                },
            ],
            "duplicates collapse, order is kept"
        );

        // The person chip deleted: its text and its mention are gone.
        let mut without = text.clone();
        without.replace_range(second.clone(), "");
        let shift = second.len();
        let content = InputContent::new(without)
            .with_token(first, file.clone())
            .unwrap()
            .with_token(third.start - shift..third.end - shift, file)
            .unwrap();
        assert_eq!(
            mentions_from_tokens(content.tokens()),
            vec![PromptMention {
                kind: MentionKind::File,
                value: "r:src/lib.rs".into()
            }]
        );
    }
}
