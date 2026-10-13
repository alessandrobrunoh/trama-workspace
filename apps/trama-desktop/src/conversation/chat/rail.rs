use super::*;

/// Rail width, inset from the top and bottom of the transcript, the
/// dashes' distance from the right edge, and the hover card's width.
pub(super) const RAIL_WIDTH: f32 = 26.;
pub(super) const RAIL_INSET: f32 = 10.;
pub(super) const RAIL_DASH_RIGHT: f32 = 8.;
pub(super) const RAIL_CARD_WIDTH: f32 = 320.;

impl ChatPanel {
    /// Turn extents from the transcript's last layout.
    pub(super) fn rail_geometry(&self) -> Option<RailGeometry> {
        let count = self.scroll.children_count();
        if count == 0 || self.rail.turn_rows.is_empty() {
            return None;
        }
        let bounds = self.scroll.bounds();
        let view_height = f32::from(bounds.size.height);
        if view_height <= 0. {
            return None;
        }
        let top = |row: usize| {
            self.scroll
                .bounds_for_item(row)
                .map(|item| f32::from(item.top() - bounds.top()))
        };
        let starts: Option<Vec<f32>> = self.rail.turn_rows.iter().map(|&row| top(row)).collect();
        let starts = starts?;
        let content_height = view_height + f32::from(self.scroll.max_offset().y).max(0.);
        let content_end = self
            .scroll
            .bounds_for_item(count - 1)
            .map(|item| f32::from(item.bottom() - bounds.top()))
            .unwrap_or(content_height);
        let view_top = -f32::from(self.scroll.offset().y);
        Some(RailGeometry {
            spans: turns::turn_spans(&starts, content_end),
            content_height,
            view: Span::new(view_top, view_top + view_height),
        })
    }

    pub(super) fn scroll_to_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        if let Some(&row) = self.rail.turn_rows.get(turn) {
            self.scroll.scroll_to_top_of_item(row);
            cx.notify();
        }
    }

    /// Click a tick: scroll to its turn; ⌘+click bookmarks it instead.
    pub(super) fn click_turn(&mut self, turn: usize, bookmark: bool, cx: &mut Context<Self>) {
        if !bookmark {
            self.scroll_to_turn(turn, cx);
            return;
        }
        let message = turns::segment_turns(&self.entries)
            .get(turn)
            .and_then(|turn| turn.message_id.clone());
        if let (Some(thread), Some(message)) = (self.selected_thread.clone(), message) {
            self.bookmarks.toggle(&thread, &message);
            cx.notify();
        }
    }

    /// ⌥+scroll: move one turn per notch (or per stretch of a trackpad
    /// swipe). Returns whether the event was used.
    pub(super) fn scroll_by_marker(
        &mut self,
        delta: Pixels,
        precise: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(geometry) = self.rail_geometry() else {
            return false;
        };
        if geometry.spans.len() < 2 {
            return false;
        }
        let delta = f32::from(delta);
        if delta == 0. {
            return true;
        }
        if precise {
            if self.rail.pending.signum() != delta.signum() {
                self.rail.pending = 0.;
            }
            self.rail.pending += delta;
            if self.rail.pending.abs() < 40. {
                return true;
            }
        }
        let cooldown = if precise { 350 } else { 90 };
        if self
            .rail
            .last_jump
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(cooldown))
        {
            self.rail.pending = 0.;
            return true;
        }
        self.rail.pending = 0.;
        // Content moves up (negative delta) when scrolling down.
        let down = delta < 0.;
        if let Some(turn) = turns::marker_target(&geometry.spans, geometry.view.top, down) {
            self.rail.last_jump = Some(std::time::Instant::now());
            self.scroll_to_turn(turn, cx);
        }
        true
    }

    /// The rail on the transcript's right edge and the hovered turn's card.
    pub(super) fn render_rail(
        &self,
        turn_list: &[turns::Turn],
        geometry: &RailGeometry,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> (Option<AnyElement>, Option<AnyElement>) {
        let bounds = self.scroll.bounds();
        let rail_height = f32::from(bounds.size.height) - 2. * RAIL_INSET;
        if rail_height < 40. || geometry.spans.len() != turn_list.len() {
            return (None, None);
        }
        let current = turns::current_turn(&geometry.spans, geometry.view, geometry.content_height);
        let dashes = turns::rail_dashes(&geometry.spans, geometry.view, current, rail_height);
        let marked = |turn: &turns::Turn| match (&self.selected_thread, &turn.message_id) {
            (Some(thread), Some(message)) => self.bookmarks.contains(thread, message),
            _ => false,
        };
        let pitch = match dashes.as_slice() {
            [first, second, ..] => second.top - first.top,
            _ => turns::DASH_PITCH,
        };

        let mut rail = div().relative().size_full();
        for (index, dash) in dashes.iter().enumerate() {
            let hovered = self.rail.hovered == Some(index);
            let bookmarked = marked(&turn_list[index]);
            let color = if bookmarked {
                p.accent
            } else if dash.current || hovered {
                p.text
            } else if dash.in_view {
                p.text_muted
            } else {
                p.text_faint
            };
            let length = if dash.current || hovered {
                turns::CURRENT_DASH_LENGTH
            } else {
                turns::DASH_LENGTH
            };
            // The hit area fills the pitch so dense rails stay easy to hit.
            let slot = pitch.max(turns::DASH_THICKNESS + 2.);
            let slop = (slot - turns::DASH_THICKNESS) / 2.;
            rail = rail.child(
                div()
                    .id(("rail-dash", index))
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(dash.top - slop))
                    .h(px(slot))
                    .cursor_pointer()
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.rail.hovered = Some(index);
                        } else if this.rail.hovered == Some(index) {
                            this.rail.hovered = None;
                        }
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        this.click_turn(index, event.modifiers().platform, cx);
                    }))
                    .child(
                        div()
                            .absolute()
                            .right(px(RAIL_DASH_RIGHT))
                            .top(px(slop))
                            .h(px(turns::DASH_THICKNESS))
                            .w(px(length))
                            .rounded(px(1.))
                            .bg(color),
                    ),
            );
        }
        let rail = div()
            .absolute()
            .top(px(RAIL_INSET))
            .bottom(px(RAIL_INSET))
            .right_0()
            .w(px(RAIL_WIDTH))
            .child(rail)
            .into_any_element();

        let card = self
            .rail
            .hovered
            .filter(|index| *index < turn_list.len())
            .map(|index| {
                let at = point(
                    bounds.right() - px(RAIL_WIDTH + 6.),
                    bounds.top() + px(RAIL_INSET + dashes[index].top - 10.),
                );
                let card = self.turn_card(index, turn_list, marked(&turn_list[index]), p);
                deferred(
                    anchored()
                        .anchor(Anchor::TopRight)
                        .position(at)
                        .snap_to_window_with_margin(px(8.))
                        .child(card),
                )
                .with_priority(2)
                .into_any_element()
            });
        (Some(rail), card)
    }

    /// The hover card: question, answer summary, author, model, time and
    /// the rail's modifier hints.
    pub(super) fn turn_card(
        &self,
        index: usize,
        turn_list: &[turns::Turn],
        bookmarked: bool,
        p: &Palette,
    ) -> AnyElement {
        let turn = &turn_list[index];
        let question = turns::question_preview(&turn.question, 3, 220);
        let at = turn
            .message_id
            .as_ref()
            .and_then(|id| self.prompt_times.get(id))
            .copied();
        let model = self
            .model
            .as_deref()
            .map(|model| format!("{} via Claude Code", turns::model_label(model)))
            .unwrap_or_else(|| "Claude Code".into());
        let remote = turn
            .message_id
            .as_ref()
            .and_then(|id| self.remote_author(id))
            .map(str::to_string);
        let (author, avatar_icon, avatar_bg, avatar_fg) = if turn.question.is_empty() {
            ("Claude", IconName::Bot, p.accent_soft, p.accent_text)
        } else {
            ("You", IconName::User, p.selected, p.text_muted)
        };
        let author: SharedString = remote.clone().unwrap_or_else(|| author.into()).into();
        let key = |label: &'static str| {
            div()
                .px(px(4.))
                .min_w(px(16.))
                .h(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::radius_sm())
                .border_1()
                .border_color(p.border_strong)
                .bg(p.bg)
                .text_color(p.text_muted)
                .child(label)
        };
        let hint = |keys: &'static str, label: &'static str| {
            h_flex()
                .gap(theme::sp(1.))
                .items_center()
                .child(key(keys))
                .child(format!("+ {label}"))
        };
        v_flex()
            .w(px(RAIL_CARD_WIDTH))
            .rounded(theme::radius_lg())
            .border_1()
            .border_color(p.border_strong)
            .bg(p.elevated)
            .shadow_md()
            .overflow_hidden()
            .child(
                v_flex()
                    .px(theme::sp(3.))
                    .pt(theme::sp(2.5))
                    .pb(theme::sp(3.))
                    .gap(theme::sp(2.))
                    .child(
                        h_flex()
                            .gap(theme::sp(1.5))
                            .items_center()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child(format!("Turn {} of {}", index + 1, turn_list.len()))
                            .when(bookmarked, |this| {
                                this.child(div().flex_1()).child(
                                    h_flex()
                                        .gap(theme::sp(1.))
                                        .items_center()
                                        .text_color(p.accent_text)
                                        .child(Icon::new(IconName::Bookmark).with_size(px(11.)))
                                        .child("Bookmarked"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(theme::text_ui())
                            .line_height(relative(1.4))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if question.is_empty() {
                                p.text_muted
                            } else {
                                p.text
                            })
                            .child(if question.is_empty() {
                                "Turn started by the agent".to_string()
                            } else {
                                question
                            }),
                    )
                    .when_some(turn.summary.clone(), |this, summary| {
                        this.child(
                            div()
                                .w_full()
                                .truncate()
                                .text_size(theme::text_sm())
                                .text_color(p.text_muted)
                                .child(summary),
                        )
                    })
                    .child(
                        h_flex()
                            .pt(theme::sp(1.))
                            .gap(theme::sp(2.))
                            .items_center()
                            .child(match &remote {
                                Some(name) => crate::collab::presence::avatar(p, name, 22., false),
                                None => div()
                                    .size(px(22.))
                                    .flex_shrink_0()
                                    .rounded_full()
                                    .bg(avatar_bg)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(avatar_icon)
                                            .with_size(px(12.))
                                            .text_color(avatar_fg),
                                    ),
                            })
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_size(theme::text_sm())
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(p.text)
                                            .child(author),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(theme::text_xs())
                                            .text_color(p.text_faint)
                                            .child(model),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .flex_shrink_0()
                                    .items_end()
                                    .text_size(theme::text_xs())
                                    .child(div().text_color(p.text_muted).child(match at {
                                        Some(at) => comments::relative_time(
                                            at.timestamp(),
                                            comments::now_secs(),
                                        ),
                                        None => "just now".into(),
                                    }))
                                    .when_some(at, |this, at| {
                                        this.child(
                                            div()
                                                .text_color(p.text_faint)
                                                .child(turns::absolute_time(at)),
                                        )
                                    }),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px(theme::sp(3.))
                    .py(theme::sp(1.5))
                    .gap(theme::sp(4.))
                    .items_center()
                    .border_t_1()
                    .border_color(p.border)
                    .bg(p.panel)
                    .text_size(theme::text_xs())
                    .text_color(p.text_faint)
                    .child(hint("\u{2318}", "click to bookmark"))
                    .child(hint("\u{2325}", "scroll by marker")),
            )
            .into_any_element()
    }
}

pub(super) fn notice(p: &Palette, icon: IconName, color: Hsla, text: String) -> impl IntoElement {
    h_flex()
        .w_full()
        .px(theme::sp(3.))
        .py(theme::sp(2.))
        .gap(theme::sp(2.))
        .items_center()
        .rounded(theme::radius())
        .border_1()
        .border_color(p.border)
        .bg(p.elevated)
        .text_size(theme::text_sm())
        .text_color(p.text_muted)
        .child(Icon::new(icon).with_size(px(14.)).text_color(color))
        .child(div().flex_1().child(text))
}

pub fn empty_state(
    p: &Palette,
    icon: IconName,
    title: &'static str,
    body: &'static str,
    keys: &[(&'static str, &'static str)],
) -> AnyElement {
    v_flex()
        .w_full()
        .pt(theme::sp(24.))
        .items_center()
        .gap(theme::sp(3.))
        .child(
            div()
                .size(px(44.))
                .rounded(theme::radius_lg())
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .flex()
                .items_center()
                .justify_center()
                .child(Icon::new(icon).with_size(px(20.)).text_color(p.text_muted)),
        )
        .child(
            div()
                .text_size(theme::text_title())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child(title),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_size(theme::text_ui())
                .line_height(relative(1.5))
                .text_color(p.text_muted)
                .child(body),
        )
        .when(!keys.is_empty(), |this| {
            this.child(
                h_flex()
                    .pt(theme::sp(2.))
                    .gap(theme::sp(4.))
                    .children(keys.iter().map(|(keys, label)| {
                        h_flex()
                            .gap(theme::sp(1.5))
                            .items_center()
                            .text_size(theme::text_sm())
                            .text_color(p.text_faint)
                            .children(
                                Keystroke::parse(keys)
                                    .ok()
                                    .map(|stroke| Kbd::new(stroke).outline()),
                            )
                            .child(*label)
                    })),
            )
        })
        .into_any_element()
}

/// One line describing what a tool call does, from its input.
pub fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let field = |key: &str| input.get(key).and_then(|value| value.as_str());
    let summary = match name {
        "Bash" => field("command").map(str::to_string),
        "Grep" => field("pattern").map(|pattern| match field("path") {
            Some(path) => format!("{pattern}  in {path}"),
            None => pattern.to_string(),
        }),
        "Glob" => field("pattern").map(str::to_string),
        "WebFetch" => field("url").map(str::to_string),
        "WebSearch" => field("query").map(str::to_string),
        "Task" | "Agent" => field("description").map(str::to_string),
        "TodoWrite" => input
            .get("todos")
            .and_then(|todos| todos.as_array())
            .map(|todos| format!("{} items", todos.len())),
        _ => None,
    };
    let summary = summary
        .or_else(|| {
            [
                "file_path",
                "path",
                "notebook_path",
                "command",
                "pattern",
                "url",
                "query",
            ]
            .iter()
            .find_map(|key| field(key).map(str::to_string))
        })
        .or_else(|| {
            input.as_object().and_then(|map| {
                map.values()
                    .find_map(|value| value.as_str().map(str::to_string))
            })
        })
        .unwrap_or_default();
    let line = summary.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(120).collect();
    if line.chars().count() > 120 || summary.lines().nth(1).is_some() {
        out.push('…');
    }
    out
}
