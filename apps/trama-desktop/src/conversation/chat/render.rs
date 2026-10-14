use super::*;

impl Render for ChatPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.restore_thread_draft {
            if let Some(thread_id) = &self.selected_thread {
                let draft = self.drafts.remove(thread_id).unwrap_or_default();
                self.composer.set_prompt(&draft, window, cx);
            }
            self.restore_thread_draft = false;
        }
        let p = theme::palette(cx);
        let running = self.is_running();
        let stoppable = matches!(
            self.lifecycle,
            TurnLifecycle::Running | TurnLifecycle::Stopping
        );
        self.composer.set_disabled(running, cx);
        let uses_daemon = self.uses_daemon();
        let show_composer = uses_daemon || !self.connected;
        let groups = if self.comments_enabled() {
            self.comments.read(cx).groups()
        } else {
            Vec::new()
        };
        let handlers = self.card_handlers(cx);
        let shared = match (&self.collab, &self.selected_thread) {
            (Some(collab), Some(thread)) if uses_daemon => collab.read(cx).thread_view(thread),
            _ => Default::default(),
        };
        let me = self
            .collab
            .as_ref()
            .and_then(|collab| collab.read(cx).username().map(str::to_string));
        let pad = if self.pane_mode {
            theme::sp(4.)
        } else {
            theme::sp(6.)
        };

        // Each block is one child of the scroll container, so a message can
        // be scrolled to by its row index.
        let row = |content: AnyElement| doc_row(None, content, pad);
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut row_of: HashMap<MessageId, usize> = HashMap::new();
        let mut hits: Vec<SelectionHit> = Vec::new();

        if let Some(error) = self.connection_error.clone() {
            rows.push(row(
                notice(&p, IconName::TriangleAlert, p.warning, error).into_any_element()
            ));
        }
        if let Some(why) = &shared.ended {
            rows.push(row(crate::collab::presence::ended_banner(&p, why)));
        }
        if shared.revoked {
            rows.push(row(crate::collab::presence::revoked_banner(&p)));
        }
        if let Some(share_id) = shared.share_id.clone() {
            let retry = cx.listener(move |this, _: &ClickEvent, _, cx| {
                if let Some(collab) = this.collab.clone() {
                    collab.update(cx, |collab, cx| collab.retry_restore(&share_id, cx));
                }
            });
            for banner in crate::collab::status_banner::banners(&p, &shared.status, retry) {
                rows.push(row(banner));
            }
        }
        if !self.connected && !self.pane_mode && !self.demo {
            rows.push(row(notice(
                &p,
                IconName::Info,
                p.text_muted,
                "Offline demo: replies are scripted and nothing is sent to an agent.".into(),
            )
            .into_any_element()));
        }

        // Moved out while rendering so blocks can borrow `self` mutably.
        let entries = std::mem::take(&mut self.entries);
        if entries.is_empty() {
            rows.push(row(if uses_daemon || !self.connected {
                empty_state(
                    &p,
                    IconName::MessageSquare,
                    "Start the conversation",
                    "Describe the task. The agent works in this thread's checkout and streams its progress here.",
                    &[("enter", "Send"), ("shift-enter", "New line")],
                )
            } else {
                empty_state(
                    &p,
                    IconName::MessageSquare,
                    "No thread selected",
                    "Pick a thread in the sidebar or start a new one.",
                    &[
                        ("secondary-n", "New thread"),
                        ("secondary-k", "Jump to thread"),
                        ("secondary-/", "Shortcuts"),
                    ],
                )
            }));
        }

        let turn_list = turns::segment_turns(&entries);
        let hidden = self.hidden_entries(&entries);
        let items: Vec<Item> = tools::layout_items(&entries)
            .into_iter()
            .filter(|item| !matches!(item, Item::Single(ix) if hidden.contains(ix)))
            .collect();
        let answers: HashMap<MessageId, String> = entries
            .iter()
            .filter_map(|entry| match entry {
                ChatEntry::Assistant {
                    markdown,
                    message_id: Some(id),
                    ..
                } => turns::first_sentence(markdown, 140).map(|line| (id.clone(), line)),
                _ => None,
            })
            .collect();
        let placement = self.place_comments(&entries, &turn_list, &groups, &p);
        let card_ctx = CardCtx {
            groups,
            handlers,
            answers,
            me,
            reply_drawn: Cell::new(false),
        };
        let blocks = document::document(
            &items,
            &turn_list,
            |turn| placement.by_turn.contains_key(&turn),
            |item| self.entry_author(&entries[item.start()]),
        );

        self.block_texts.begin_frame();
        let mut turn_rows: Vec<usize> = Vec::with_capacity(turn_list.len());
        let mut last_author: Option<AuthorKey> = None;
        for block in &blocks {
            while turn_rows.len() <= block.turn && turn_rows.len() < turn_list.len() {
                turn_rows.push(rows.len());
            }
            let index = rows.len();
            let content = match &block.item {
                BlockItem::Comments(turn) => {
                    let list = placement.by_turn.get(turn).cloned().unwrap_or_default();
                    self.turn_comments(*turn, &list, &card_ctx, &p, cx)
                }
                BlockItem::Entries(item) => self.render_item(
                    item,
                    &entries,
                    block.turn,
                    &placement,
                    &card_ctx,
                    &mut hits,
                    &mut row_of,
                    index,
                    &p,
                    cx,
                ),
            };
            let avatar = block
                .avatar
                .then(|| self.avatar(&block.author, card_ctx.me.as_deref(), index, &p));
            rows.push(doc_row(avatar, content, pad));
            last_author = Some(block.author.clone());
        }
        self.entries = entries;
        let rows_changed = self.rail.turn_rows != turn_rows;
        self.rail.turn_rows = turn_rows;

        // A selection inside exactly one block can be commented on.
        hits.extend(
            self.block_texts
                .selections(cx)
                .into_iter()
                .map(SelectionHit::Block),
        );
        self.selection = if hits.len() == 1 { hits.pop() } else { None };
        if let Some(id) = self.reveal.take() {
            match row_of.get(&id) {
                Some(&index) => self.scroll.scroll_to_top_of_item(index),
                None => self.flash = None,
            }
        }
        if let Some(flash) = &self.flash
            && let Some(&index) = row_of.get(flash)
            && let Some(slot) = rows.get_mut(index)
        {
            let element = std::mem::replace(slot, div().into_any_element());
            *slot = div()
                .w_full()
                .flex_shrink_0()
                .bg(p.accent_soft)
                .child(element)
                .into_any_element();
        }

        let lease_button = |cx: &mut Context<Self>, key: usize| -> Option<AnyElement> {
            let offer = shared.lease.can_take && !shared.lease.taking;
            offer.then(|| {
                Button::new(("take-lease", key))
                    .xsmall()
                    .ghost()
                    .label("Take lease")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let (Some(collab), Some(thread)) =
                            (this.collab.clone(), this.selected_thread.clone())
                        {
                            collab.update(cx, |collab, cx| collab.take_lease(&thread, cx));
                        }
                    }))
                    .into_any_element()
            })
        };
        for (key, id) in shared.unshared.iter().enumerate() {
            if let Some(&index) = row_of.get(id)
                && let Some(slot) = rows.get_mut(index)
            {
                let element = std::mem::replace(slot, div().into_any_element());
                *slot = v_flex()
                    .w_full()
                    .flex_shrink_0()
                    .child(element)
                    .child(crate::collab::presence::unshared_badge(
                        &p,
                        &shared.lease,
                        lease_button(cx, key),
                    ))
                    .into_any_element();
            }
        }

        for turn in &shared.remote_turns {
            rows.push(row(crate::collab::presence::remote_turn(&p, turn)));
            last_author = None;
        }

        if running {
            let label = match self.lifecycle {
                TurnLifecycle::Stopping => "Stopping…",
                TurnLifecycle::AwaitingAssistant => "Finishing…",
                _ if self.has_open_approval() => "Waiting for approval…",
                _ if shared.queued && shared.runner_online => "Queued · starting on this machine…",
                _ if shared.queued => "Queued · runs when your runner is online…",
                _ => "Working…",
            };
            let avatar = (last_author != Some(AuthorKey::Agent))
                .then(|| self.avatar(&AuthorKey::Agent, None, rows.len(), &p));
            rows.push(doc_row(
                avatar,
                h_flex()
                    .min_h(px(22.))
                    .gap(theme::sp(2.))
                    .items_center()
                    .text_size(theme::text_ui())
                    .text_color(p.text_muted)
                    .child(Spinner::new().small().color(p.accent))
                    .child(label)
                    .into_any_element(),
                pad,
            ));
        }

        let session_model = self.model.clone();
        self.model_picker
            .update(cx, |picker, _| picker.session_model = session_model);
        let (composer_tools, composer_header) = self.composer_extras(&p, cx);
        let footer = ComposerFooter {
            model: self.model.clone(),
            status_label: self.status_pill(&p).0,
            status_color: self.status_pill(&p).1,
            usage: self.usage.clone(),
            cost_usd_micros: self.cost_usd_micros,
            turns: self.turns,
            billing: self.session_info.as_ref().map(billing_badge),
            model_picker: (uses_daemon && !self.pane_mode)
                .then(|| self.model_picker.clone().into_any_element()),
            tools: composer_tools,
            header: composer_header,
        };
        let terminals_open = uses_daemon && !self.pane_mode && self.terminals.read(cx).is_open();
        let strip = if show_composer && uses_daemon {
            self.pending_strip(&card_ctx.groups, &p, cx)
        } else {
            None
        };
        let live_drafts = crate::collab::presence::drafts_block(&p, &shared.drafts);
        let shared_footer = (shared.linked && !shared.revoked)
            .then(|| crate::collab::presence::shared_footer(&p, shared.runner_online));

        // The composer is the next block of the document: right under the
        // last output, with the user's avatar in the gutter, scrolling with
        // the transcript instead of sitting pinned to the bottom.
        if show_composer {
            let block = v_flex()
                .w_full()
                .children(live_drafts)
                .children(strip)
                .when(terminals_open, |this| this.child(self.terminals.clone()))
                .child(
                    self.composer_key_handlers(div().w_full(), cx)
                        .child(self.composer.render(
                            running,
                            stoppable,
                            footer,
                            cx.listener(|this, _, window, cx| this.submit(window, cx)),
                            cx.listener(|this, _, _, cx| this.stop(cx)),
                            window,
                            cx,
                        )),
                )
                .child(self.popup.clone())
                .children(shared_footer);
            let avatar = self.avatar(&AuthorKey::Me, card_ctx.me.as_deref(), rows.len(), &p);
            rows.push(doc_row(Some(avatar), block.into_any_element(), pad));
        }

        let row_count = rows.len();
        let transcript = v_flex()
            .id("chat-messages")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .track_focus(&self.doc_focus)
            .pt(theme::sp(8.))
            .pb(theme::sp(24.))
            .gap(theme::sp(2.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    // A diff line press stops before this: anything else
                    // drops the picked lines.
                    if this.patch_pick.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    this.selection_point = Some(event.position);
                    this.patch_dragging = false;
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                if this.selection_point.take().is_some() {
                    cx.notify();
                }
            }))
            .children(rows);

        // The rail reads the last layout; measure again once this one lands.
        if rows_changed || self.scroll.children_count() != row_count {
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }
        let (rail, rail_card) = match self.rail_geometry() {
            Some(geometry) if turn_list.len() > 1 => {
                self.render_rail(&turn_list, &geometry, &p, cx)
            }
            _ => (None, None),
        };
        let weak = cx.entity().downgrade();
        let option_scroll = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture
                        || !event.modifiers.alt
                        || !bounds.contains(&event.position)
                    {
                        return;
                    }
                    let delta = event.delta.pixel_delta(window.line_height()).y;
                    let precise = event.delta.precise();
                    let handled = weak
                        .update(cx, |this, cx| this.scroll_by_marker(delta, precise, cx))
                        .unwrap_or(false);
                    if handled {
                        cx.stop_propagation();
                    }
                });
            },
        )
        .absolute()
        .size_full();

        let floating = self
            .has_comment_target()
            .then_some(self.selection_point)
            .flatten()
            .filter(|_| uses_daemon && self.draft.is_none())
            .map(|at| {
                deferred(
                    anchored()
                        .position(at)
                        .offset(point(px(-48.), px(-40.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            h_flex()
                                .id("selection-comment")
                                .h(px(28.))
                                .px(theme::sp(2.5))
                                .gap(theme::sp(1.5))
                                .items_center()
                                .rounded(theme::radius())
                                .border_1()
                                .border_color(p.border_strong)
                                .bg(p.elevated)
                                .shadow_md()
                                .cursor_pointer()
                                .text_size(theme::text_sm())
                                .text_color(p.text)
                                .hover(|style| style.bg(p.hover))
                                // Act on press: the release would clear the
                                // text selection first.
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.comment_on_selection(window, cx);
                                    }),
                                )
                                .child(
                                    Icon::new(IconName::MessageSquarePlus)
                                        .with_size(px(13.))
                                        .text_color(p.accent_text),
                                )
                                .child("Comment")
                                .child(
                                    div()
                                        .text_size(theme::text_xs())
                                        .text_color(p.text_faint)
                                        .child("or just type"),
                                ),
                        ),
                )
                .with_priority(1)
            });

        v_flex()
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.type_to_comment(event, window, cx);
            }))
            .child(
                v_flex()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(transcript)
                    .child(option_scroll)
                    .children(rail),
            )
            .children(rail_card)
            .children(floating)
    }
}

// -- document blocks ----------------------------------------------------------

/// Width of the avatar gutter left of the text column.
pub(super) const GUTTER: f32 = 34.;
/// Avatar size in the gutter.
pub(super) const AVATAR: f32 = 22.;

/// One block of the document: the author's avatar in the gutter (only
/// where their run starts) and the block in the text column.
pub(super) fn doc_row(avatar: Option<AnyElement>, content: AnyElement, pad: Pixels) -> AnyElement {
    h_flex()
        .w_full()
        .flex_shrink_0()
        .justify_center()
        .child(
            h_flex()
                .w_full()
                .max_w(theme::transcript_width() + px(GUTTER))
                .px(pad)
                .items_start()
                .child(div().w(px(GUTTER)).flex_shrink_0().children(avatar))
                .child(div().flex_1().min_w_0().child(content)),
        )
        .into_any_element()
}

/// A round glyph avatar (the agent, a person without a known name).
pub(super) fn glyph_avatar(icon: IconName, bg: Hsla, fg: Hsla) -> Div {
    div()
        .size(px(AVATAR))
        .flex_shrink_0()
        .rounded_full()
        .bg(bg)
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon).with_size(px(13.)).text_color(fg))
}
