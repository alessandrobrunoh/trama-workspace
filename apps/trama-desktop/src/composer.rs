use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{InputEvent, Textarea, TextareaState},
};
use gpui_kit::*;

/// Composer input: Kit `TextareaState` (0.7.1's textarea counterpart of
/// `InputState`) plus a send button. Enter submits.
pub struct Composer {
    pub input: Entity<TextareaState>,
}

impl Composer {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Message TramaWorkspace…")
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

    pub fn render(
        &self,
        running: bool,
        on_send: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut App,
    ) -> impl IntoElement {
        let can_send = !running && !self.input.read(cx).value().trim().is_empty();

        h_flex()
            .w_full()
            .items_end()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Textarea::new(&self.input).h(px(72.))),
            )
            .child(
                Button::new("send-message")
                    .primary()
                    .icon(IconName::Send)
                    .label("Send")
                    .disabled(!can_send)
                    .on_click(on_send),
            )
    }
}
