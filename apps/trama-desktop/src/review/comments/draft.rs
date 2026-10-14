use super::*;

// -- inline composer ------------------------------------------------------

/// An open comment or reply composer.
pub struct CommentDraft {
    pub target: CommentTarget,
    pub parent: Option<CommentId>,
    pub input: Entity<TextareaState>,
    /// A comment on a block that is not a message: its kind and passage,
    /// quoted in the body on submit ([`proxy_body`]).
    quote: Option<(&'static str, String)>,
    _enter: Subscription,
}

impl CommentDraft {
    /// Create the input, focus it, and call `on_submit` on Enter. `initial`
    /// is text already typed (type-to-comment starts with the first key).
    pub fn new<V: 'static>(
        target: CommentTarget,
        parent: Option<CommentId>,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<V>,
        mut on_submit: impl FnMut(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let placeholder = if parent.is_some() {
            "Reply…"
        } else {
            "Comment on this passage…"
        };
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(placeholder)
                .rows(2)
                .submit_on_enter(true)
        });
        let enter = cx.subscribe_in(&input, window, move |this, _, event, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                on_submit(this, window, cx);
            }
        });
        if let Some(initial) = initial.filter(|text| !text.is_empty()) {
            input.update(cx, |input, cx| input.insert(initial, window, cx));
        }
        input.focus_handle(cx).focus(window, cx);
        Self {
            target,
            parent,
            input,
            quote: None,
            _enter: enter,
        }
    }

    /// Quote `excerpt` of a `kind` block in the body on submit.
    pub fn with_quote(mut self, kind: &'static str, excerpt: String) -> Self {
        self.quote = Some((kind, excerpt));
        self
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.focus_handle(cx).is_focused(window)
    }

    pub fn body(&self, cx: &App) -> Option<String> {
        let value = self.input.read(cx).value().to_string();
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// Send it through `store`. Returns whether it left the client.
    pub fn submit(&self, store: &Entity<CommentStore>, cx: &mut App) -> bool {
        let Some(body) = self.body(cx) else {
            return false;
        };
        let body = match &self.quote {
            Some((kind, excerpt)) => proxy_body(kind, excerpt, &body),
            None => body,
        };
        let target = self.target.clone();
        let parent = self.parent.clone();
        store.update(cx, |store, cx| store.create(target, body, parent, cx)) == SendStatus::Enqueued
    }
}

/// Cancels an open composer (its button and Escape).
pub type CancelDraft = Rc<dyn Fn(&mut Window, &mut App)>;

pub fn render_draft(
    id: impl Into<SharedString>,
    draft: &CommentDraft,
    on_submit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_cancel: CancelDraft,
    p: &Palette,
    cx: &App,
) -> AnyElement {
    let id: SharedString = id.into();
    let empty = draft.body(cx).is_none();
    let escape = on_cancel.clone();
    v_flex()
        .id(id.clone())
        .capture_action(move |_: &Escape, window, cx| escape(window, cx))
        .w_full()
        .gap(theme::sp(1.5))
        .px(theme::sp(2.5))
        .py(theme::sp(2.))
        .rounded(theme::radius())
        .border_1()
        .border_color(p.accent.opacity(0.45))
        .bg(p.elevated)
        .child(
            div()
                .w_full()
                .text_size(theme::text_ui())
                .child(Textarea::new(&draft.input).appearance(false).h(px(44.))),
        )
        .child(
            h_flex()
                .w_full()
                .gap(theme::sp(1.5))
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_size(theme::text_xs())
                        .text_color(p.text_faint)
                        .child("Enter to comment · Shift+Enter new line · Esc to cancel"),
                )
                .child(
                    Button::new(SharedString::from(format!("{id}-cancel")))
                        .ghost()
                        .xsmall()
                        .label("Cancel")
                        .on_click(move |_, window, cx| on_cancel(window, cx)),
                )
                .child(
                    Button::new(SharedString::from(format!("{id}-submit")))
                        .primary()
                        .xsmall()
                        .label(if draft.parent.is_some() {
                            "Reply"
                        } else {
                            "Comment"
                        })
                        .disabled(empty)
                        .on_click(on_submit),
                ),
        )
        .into_any_element()
}
