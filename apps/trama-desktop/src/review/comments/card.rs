use super::*;

// -- thread card ----------------------------------------------------------

pub(super) type Callback<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;

/// What a thread card can do; each view supplies its own reply composer and
/// answer navigation.
#[derive(Clone)]
pub struct CardHandlers {
    pub store: Entity<CommentStore>,
    pub on_toggle: Callback<CommentId>,
    pub on_reply: Callback<CommentId>,
    /// Scroll the transcript to the message that answered the comment.
    pub on_answer: Callback<MessageId>,
}

/// The transcript's author avatar, at comment size.
pub(super) fn avatar(actor: &Actor, p: &Palette) -> impl IntoElement {
    let (icon, bg, fg) = match actor {
        Actor::Human { .. } => (IconName::User, p.selected, p.text_muted),
        _ => (IconName::Bot, p.accent_soft, p.accent_text),
    };
    div()
        .size(px(16.))
        .flex_shrink_0()
        .rounded_full()
        .bg(bg)
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon).with_size(px(10.)).text_color(fg))
}

pub(super) fn quote(text: &str, p: &Palette) -> impl IntoElement {
    div()
        .w_full()
        .pl(theme::sp(2.))
        .border_l_2()
        .border_color(p.accent.opacity(0.5))
        .text_size(theme::text_sm())
        .line_height(relative(1.45))
        .text_color(p.text_muted)
        .line_clamp(3)
        .child(text.trim().to_string())
}

pub(super) fn badge(badge: AnchorBadge, p: &Palette) -> impl IntoElement {
    let color = badge.color(p);
    div()
        .flex_shrink_0()
        .px(theme::sp(1.5))
        .py(px(1.))
        .rounded(theme::radius_sm())
        .bg(color.opacity(0.12))
        .text_size(theme::text_xs())
        .text_color(color)
        .child(badge.label())
}

pub(super) fn phase_color(phase: &Phase, p: &Palette) -> Hsla {
    match phase {
        Phase::Pending => p.accent,
        Phase::Detached => p.text_muted,
        Phase::Delivered(_) => p.success,
        Phase::Resolved(_) => p.text_faint,
    }
}

pub(super) fn phase_summary(group: &CommentGroup) -> String {
    match group.phase() {
        Phase::Pending => "Sent with your next message".into(),
        Phase::Detached => "Not in your next message".into(),
        Phase::Delivered(delivery) if delivery.answer_message_id.is_some() => {
            format!("Answered in turn {}", delivery.turn)
        }
        Phase::Delivered(delivery) => {
            format!("Sent in turn {} · waiting for the answer", delivery.turn)
        }
        Phase::Resolved(_) => "Resolved".into(),
    }
}

/// A collapsed thread: one row that expands on click.
pub fn render_collapsed(
    id: impl Into<SharedString>,
    group: &CommentGroup,
    badge_for: Option<AnchorBadge>,
    handlers: &CardHandlers,
    p: &Palette,
) -> AnyElement {
    let phase = group.phase();
    let color = phase_color(&phase, p);
    let comment_id = group.id().clone();
    let on_toggle = handlers.on_toggle.clone();
    h_flex()
        .id(id.into())
        .w_full()
        .h(px(24.))
        .px(theme::sp(2.))
        .gap(theme::sp(1.5))
        .items_center()
        .rounded(theme::radius())
        .cursor_pointer()
        .text_size(theme::text_xs())
        .text_color(p.text_muted)
        .hover(|style| style.bg(p.hover))
        .on_click(move |_, window, cx| on_toggle(comment_id.clone(), window, cx))
        .child(
            Icon::new(IconName::MessageSquare)
                .with_size(px(12.))
                .text_color(color),
        )
        .child(div().flex_shrink_0().text_color(p.text).child(plural(
            group.len(),
            "comment",
            "comments",
        )))
        .child(
            div()
                .flex_shrink_0()
                .text_color(p.text_faint)
                .child(phase_summary(group)),
        )
        .children(badge_for.map(|b| badge(b, p)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(p.text_faint)
                .child(one_line(&group.root.body, 80)),
        )
        .into_any_element()
}

/// An expanded thread: quote, comments, status and actions.
#[allow(clippy::too_many_arguments)]
pub fn render_card(
    id: impl Into<SharedString>,
    group: &CommentGroup,
    show_excerpt: bool,
    badge_for: Option<AnchorBadge>,
    reply: Option<AnyElement>,
    handlers: &CardHandlers,
    now: i64,
    p: &Palette,
) -> AnyElement {
    let id: SharedString = id.into();
    let phase = group.phase();
    let color = phase_color(&phase, p);
    let root_id = group.id().clone();

    let comment = |comment: &CommentSnapshot| {
        let delete = can_delete(comment).then(|| {
            let store = handlers.store.clone();
            let comment_id = comment.id.clone();
            Button::new(SharedString::from(format!("{id}-delete-{}", comment.id)))
                .ghost()
                .xsmall()
                .icon(IconName::Delete)
                .tooltip("Delete this pending comment")
                .on_click(move |_, _, cx| {
                    store.update(cx, |store, cx| store.delete(comment_id.clone(), cx));
                })
        });
        let attach = matches!(comment.state, CommentState::Detached { .. }).then(|| {
            let store = handlers.store.clone();
            let comment_id = comment.id.clone();
            Button::new(SharedString::from(format!("{id}-attach-{}", comment.id)))
                .ghost()
                .xsmall()
                .icon(IconName::ArrowUp)
                .tooltip("Send with your next message")
                .on_click(move |_, _, cx| {
                    store.update(cx, |store, cx| store.attach(comment_id.clone(), cx));
                })
        });
        v_flex()
            .w_full()
            .gap(theme::sp(0.5))
            .child(
                h_flex()
                    .w_full()
                    .gap(theme::sp(1.5))
                    .items_center()
                    .text_size(theme::text_xs())
                    .child(avatar(&comment.author, p))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text)
                            .child(author_label(&comment.author)),
                    )
                    .child(
                        div()
                            .text_color(p.text_faint)
                            .child(relative_time(comment.created_at.timestamp(), now)),
                    )
                    .when(comment.state == CommentState::Pending, |this| {
                        this.child(
                            div()
                                .px(theme::sp(1.5))
                                .rounded(theme::radius_sm())
                                .bg(p.accent_soft)
                                .text_color(p.accent_text)
                                .child("Pending"),
                        )
                    })
                    .when(
                        matches!(comment.state, CommentState::Detached { .. }),
                        |this| this.child(div().text_color(p.text_faint).child("Not sent")),
                    )
                    .child(div().flex_1())
                    .children(attach)
                    .children(delete),
            )
            .child(
                div()
                    .w_full()
                    .text_size(theme::text_ui())
                    .line_height(relative(1.5))
                    .text_color(p.text)
                    .child(comment.body.clone()),
            )
    };

    let status: AnyElement = match &phase {
        Phase::Delivered(delivery) | Phase::Resolved(Some(delivery))
            if delivery.answer_message_id.is_some() =>
        {
            let answer = delivery
                .answer_message_id
                .clone()
                .unwrap_or_else(|| delivery.message_id.clone());
            let on_answer = handlers.on_answer.clone();
            let resolved = matches!(phase, Phase::Resolved(_));
            h_flex()
                .id(SharedString::from(format!("{id}-answer")))
                .gap(theme::sp(1.))
                .items_center()
                .cursor_pointer()
                .text_color(p.accent_text)
                .hover(|style| style.text_color(p.accent))
                .on_click(move |_, window, cx| on_answer(answer.clone(), window, cx))
                .child(if resolved {
                    format!("Resolved · answered in turn {}", delivery.turn)
                } else {
                    format!("Answered in turn {}", delivery.turn)
                })
                .child(Icon::new(IconName::ArrowRight).with_size(px(11.)))
                .into_any_element()
        }
        _ => div()
            .text_color(p.text_faint)
            .child(phase_summary(group))
            .into_any_element(),
    };

    let resolved = matches!(phase, Phase::Resolved(_));
    let store = handlers.store.clone();
    let toggle_resolved = {
        let root_id = root_id.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let root_id = root_id.clone();
            store.update(cx, |store, cx| {
                if resolved {
                    store.reopen(root_id, cx)
                } else {
                    store.resolve(root_id, cx)
                }
            });
        }
    };
    let on_reply = handlers.on_reply.clone();
    let on_toggle = handlers.on_toggle.clone();
    let reply_id = root_id.clone();
    let toggle_id = root_id.clone();

    v_flex()
        .w_full()
        .px(theme::sp(3.))
        .py(theme::sp(2.5))
        .gap(theme::sp(2.))
        .rounded(theme::radius())
        .border_1()
        .border_color(match phase {
            Phase::Pending => p.accent.opacity(0.4),
            _ => p.border,
        })
        .bg(p.elevated)
        .child(
            h_flex()
                .id(SharedString::from(format!("{id}-head")))
                .w_full()
                .gap(theme::sp(1.5))
                .items_center()
                .cursor_pointer()
                .text_size(theme::text_xs())
                .on_click(move |_, window, cx| on_toggle(toggle_id.clone(), window, cx))
                .child(div().size(px(6.)).rounded_full().bg(color))
                .child(div().text_color(p.text_muted).child(plural(
                    group.len(),
                    "comment",
                    "comments",
                )))
                .children(badge_for.map(|b| badge(b, p)))
                .child(div().flex_1())
                .child(
                    Icon::new(IconName::ChevronDown)
                        .with_size(px(12.))
                        .text_color(p.text_faint),
                ),
        )
        .when_some(
            group.root.excerpt.clone().filter(|_| show_excerpt),
            |this, excerpt| this.child(quote(&excerpt, p)),
        )
        .children(group.all().map(comment))
        .children(reply)
        .child(
            h_flex()
                .w_full()
                .gap(theme::sp(1.))
                .items_center()
                .text_size(theme::text_xs())
                .child(status)
                .child(div().flex_1())
                .when(!resolved, |this| {
                    this.child(
                        Button::new(SharedString::from(format!("{id}-reply")))
                            .ghost()
                            .xsmall()
                            .label("Reply")
                            .on_click(move |_, window, cx| on_reply(reply_id.clone(), window, cx)),
                    )
                })
                .child(
                    Button::new(SharedString::from(format!("{id}-resolve")))
                        .ghost()
                        .xsmall()
                        .icon(if resolved {
                            IconName::Undo2
                        } else {
                            IconName::Check
                        })
                        .label(if resolved { "Reopen" } else { "Resolve" })
                        .on_click(toggle_resolved),
                ),
        )
        .into_any_element()
}

/// The color of comment marks: the left bar of a comment, the highlight
/// behind its passage.
pub fn mark_color(p: &Palette) -> Hsla {
    p.danger
}

/// Highlight behind a passage commented in the document: strong while the
/// comment waits, faint once delivered, none when resolved.
pub fn passage_color(phase: &Phase, p: &Palette) -> Option<Hsla> {
    match phase {
        Phase::Pending => Some(mark_color(p).opacity(0.22)),
        Phase::Delivered(_) => Some(mark_color(p).opacity(0.12)),
        Phase::Detached => Some(mark_color(p).opacity(0.08)),
        Phase::Resolved(_) => None,
    }
}

/// A comment author's round avatar: the person's initials (a collaborator
/// in a shared thread), a person glyph for you, the agent's icon.
pub fn author_avatar(actor: &Actor, me: Option<&str>, size: f32, p: &Palette) -> AnyElement {
    match crate::collab::Speaker::of(actor) {
        crate::collab::Speaker::Remote(name) => {
            crate::collab::presence::avatar(p, &name, size, false).into_any_element()
        }
        crate::collab::Speaker::Me if me.is_some() => {
            crate::collab::presence::avatar(p, me.unwrap_or_default(), size, false)
                .into_any_element()
        }
        speaker => {
            let (icon, bg, fg) = match speaker {
                crate::collab::Speaker::Me => (IconName::User, p.selected, p.text_muted),
                _ => (IconName::Bot, p.accent_soft, p.accent_text),
            };
            div()
                .size(px(size))
                .flex_shrink_0()
                .rounded_full()
                .bg(bg)
                .flex()
                .items_center()
                .justify_center()
                .child(Icon::new(icon).with_size(px(size * 0.6)).text_color(fg))
                .into_any_element()
        }
    }
}

/// A comment thread right under its passage, as in a document: a card
/// with a bar on its left, each comment as an avatar and its text, the
/// agent's answer as a "↩ answer" line, and the reply composer. Reply,
/// resolve and delete show on hover.
#[allow(clippy::too_many_arguments)]
pub fn render_inline(
    id: impl Into<SharedString>,
    group: &CommentGroup,
    answer: Option<(MessageId, String)>,
    reply: Option<AnyElement>,
    me: Option<&str>,
    handlers: &CardHandlers,
    p: &Palette,
) -> AnyElement {
    let id: SharedString = id.into();
    let phase = group.phase();
    let resolved = matches!(phase, Phase::Resolved(_));
    let bar = match phase {
        Phase::Pending => mark_color(p),
        Phase::Delivered(_) => mark_color(p).opacity(0.55),
        Phase::Detached => p.border_strong,
        Phase::Resolved(_) => p.border_strong,
    };
    let hover_group: SharedString = format!("{id}-card").into();
    let action = |name: &str, icon: IconName, tip: &'static str| {
        Button::new(SharedString::from(format!("{id}-{name}")))
            .ghost()
            .xsmall()
            .icon(icon)
            .tooltip(tip)
    };
    let actions = {
        let store = handlers.store.clone();
        let root_id = group.id().clone();
        let on_reply = handlers.on_reply.clone();
        let reply_id = root_id.clone();
        h_flex()
            .flex_shrink_0()
            .gap(theme::sp(0.5))
            .opacity(0.)
            .group_hover(hover_group.clone(), |style| style.opacity(1.))
            .when(!resolved, |this| {
                this.child(
                    action("reply", IconName::Undo2, "Reply")
                        .on_click(move |_, window, cx| on_reply(reply_id.clone(), window, cx)),
                )
            })
            .child(
                action(
                    "resolve",
                    if resolved {
                        IconName::Undo2
                    } else {
                        IconName::Check
                    },
                    if resolved { "Reopen" } else { "Resolve" },
                )
                .on_click(move |_, _, cx| {
                    let root_id = root_id.clone();
                    store.update(cx, |store, cx| {
                        if resolved {
                            store.reopen(root_id, cx)
                        } else {
                            store.resolve(root_id, cx)
                        }
                    });
                }),
            )
    };
    let comment = |index: usize, comment: &CommentSnapshot| {
        let delete = can_delete(comment).then(|| {
            let store = handlers.store.clone();
            let comment_id = comment.id.clone();
            action(
                &format!("delete-{index}"),
                IconName::Delete,
                "Delete this pending comment",
            )
            .on_click(move |_, _, cx| {
                store.update(cx, |store, cx| store.delete(comment_id.clone(), cx));
            })
        });
        let attach = matches!(comment.state, CommentState::Detached { .. }).then(|| {
            let store = handlers.store.clone();
            let comment_id = comment.id.clone();
            action(
                &format!("attach-{index}"),
                IconName::ArrowUp,
                "Send with your next message",
            )
            .on_click(move |_, _, cx| {
                store.update(cx, |store, cx| store.attach(comment_id.clone(), cx));
            })
        });
        let state = match &comment.state {
            CommentState::Pending => Some(("Pending", p.text_muted)),
            CommentState::Detached { .. } => Some(("Not sent", p.text_faint)),
            _ => None,
        };
        h_flex()
            .w_full()
            .gap(theme::sp(2.))
            .items_start()
            .child(
                div()
                    .pt(px(1.))
                    .child(author_avatar(&comment.author, me, 18., p)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme::text_ui())
                    .line_height(relative(1.5))
                    .text_color(if resolved { p.text_muted } else { p.text })
                    .child(display_body(&comment.body)),
            )
            .children(state.map(|(label, color)| {
                div()
                    .flex_shrink_0()
                    .pt(px(2.))
                    .text_size(theme::text_xs())
                    .text_color(color)
                    .child(label)
            }))
            .children(attach)
            .children(delete)
    };
    let answer = answer.map(|(message_id, excerpt)| {
        let on_answer = handlers.on_answer.clone();
        h_flex()
            .id(SharedString::from(format!("{id}-answer")))
            .w_full()
            .gap(theme::sp(1.5))
            .items_center()
            .cursor_pointer()
            .text_size(theme::text_sm())
            .text_color(p.text_muted)
            .hover(|style| style.text_color(p.text))
            .on_click(move |_, window, cx| on_answer(message_id.clone(), window, cx))
            .child(
                Icon::new(IconName::Undo2)
                    .with_size(px(12.))
                    .text_color(p.text_faint),
            )
            .child(div().flex_1().min_w_0().truncate().child(excerpt))
    });
    let status = match &phase {
        Phase::Delivered(delivery) if delivery.answer_message_id.is_none() => Some(format!(
            "Sent in turn {} · waiting for the answer",
            delivery.turn
        )),
        Phase::Resolved(_) => Some("Resolved".to_string()),
        Phase::Detached => Some("Not in your next message".to_string()),
        _ => None,
    };
    h_flex()
        .id(id.clone())
        .group(hover_group)
        .w_full()
        .items_stretch()
        .rounded(theme::radius())
        .border_1()
        .border_color(p.border)
        .bg(p.elevated)
        .overflow_hidden()
        .child(div().w(px(2.)).flex_shrink_0().bg(bar))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .px(theme::sp(2.5))
                .py(theme::sp(2.))
                .gap(theme::sp(1.5))
                .child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap(theme::sp(1.))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(theme::sp(1.5))
                                .children(group.all().enumerate().map(|(n, c)| comment(n, c))),
                        )
                        .child(actions),
                )
                .children(status.map(|status| {
                    div()
                        .text_size(theme::text_xs())
                        .text_color(p.text_faint)
                        .child(status)
                }))
                .children(answer)
                .children(reply),
        )
        .into_any_element()
}
