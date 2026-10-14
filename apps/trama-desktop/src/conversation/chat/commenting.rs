use super::*;

impl ChatPanel {
    /// Comment on the selected transcript text or the picked diff lines
    /// (⌘⌥M and the floating button).
    pub fn comment_on_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(hit) = self.selection.clone() {
            self.open_selection_draft(hit, None, window, cx);
        } else if let Some(pick) = self.patch_pick.clone() {
            self.open_patch_draft(pick, None, window, cx);
        } else if self.uses_daemon() {
            self.connection_error = Some(format!(
                "Select text anywhere in the transcript, then type your comment (or press {}).",
                crate::app::keymap::label("secondary-alt-m")
            ));
            cx.notify();
        }
    }

    /// Whether a selection or a diff pick waits for a comment.
    pub fn has_comment_target(&self) -> bool {
        self.selection.is_some() || self.patch_pick.is_some()
    }

    /// Type to comment: a printable key while transcript text is selected
    /// (or diff lines are picked) opens the composer under the passage with
    /// that text already in it.
    pub(super) fn type_to_comment(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selection.is_none() && self.patch_pick.is_none() {
            return;
        }
        if self
            .composer
            .input
            .focus_handle(cx)
            .contains_focused(window, cx)
            || self
                .draft
                .as_ref()
                .is_some_and(|open| open.draft.is_focused(window, cx))
        {
            return;
        }
        if event.keystroke.key == "escape" && self.patch_pick.is_some() && self.selection.is_none()
        {
            self.patch_pick = None;
            cx.notify();
            return;
        }
        let Some(text) = comments::typed_text(&event.keystroke) else {
            return;
        };
        cx.stop_propagation();
        window.prevent_default();
        match self.selection.clone() {
            Some(hit) => self.open_selection_draft(hit, Some(text), window, cx),
            None => {
                if let Some(pick) = self.patch_pick.clone() {
                    self.open_patch_draft(pick, Some(text), window, cx);
                }
            }
        }
    }

    /// The hover action on a message: the selection inside it, else the
    /// whole message.
    pub(super) fn comment_on_message(
        &mut self,
        message_id: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hit = match &self.selection {
            Some(hit @ SelectionHit::Message { message_id: id, .. }) if id == &message_id => {
                Some(hit.clone())
            }
            _ => self.whole_message_hit(&message_id),
        };
        if let Some(hit) = hit {
            self.open_selection_draft(hit, None, window, cx);
        }
    }

    pub(super) fn whole_message_hit(&self, message_id: &MessageId) -> Option<SelectionHit> {
        let view = self.message_views.get(message_id)?;
        let base = self.entries.iter().find_map(|entry| match entry {
            ChatEntry::User {
                text,
                message_id: Some(id),
            } if id == message_id => Some(user_body_start(text)),
            ChatEntry::Assistant {
                message_id: Some(id),
                ..
            } if id == message_id => Some(0),
            _ => None,
        })?;
        let range = comments::selection_to_message_range(&view.source, base, 0..view.source.len())?;
        Some(SelectionHit::Message {
            message_id: message_id.clone(),
            range,
        })
    }

    /// Forget the selection the composer is about to take over.
    pub(super) fn drop_selection(&mut self, cx: &mut Context<Self>) {
        for view in self.message_views.values() {
            for segment in &view.segments {
                segment
                    .state
                    .update(cx, |state, cx| state.clear_selection(cx));
            }
        }
        if let Some(SelectionHit::Block(selection)) = &self.selection {
            self.block_texts.clear_selection(&selection.block.key, cx);
        }
        self.selection = None;
        self.selection_point = None;
        self.connection_error = None;
    }

    pub(super) fn open_selection_draft(
        &mut self,
        hit: SelectionHit,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.uses_daemon() {
            return;
        }
        let (target, anchor, quote) = match &hit {
            SelectionHit::Message { message_id, range } => (
                CommentTarget::MessageRange {
                    message_id: message_id.clone(),
                    start_byte: range.start,
                    end_byte: range.end,
                },
                DraftAnchor::Passage {
                    message_id: message_id.clone(),
                    range: range.clone(),
                },
                None,
            ),
            SelectionHit::Block(selection) => {
                let Some(target) = self.block_target(selection.block.turn) else {
                    self.connection_error =
                        Some("This turn is not saved yet; comment once it has an answer.".into());
                    cx.notify();
                    return;
                };
                (
                    target,
                    DraftAnchor::Block {
                        key: selection.block.key.clone(),
                        kind: selection.block.kind,
                        excerpt: selection.text.clone(),
                        turn: selection.block.turn,
                    },
                    Some((selection.block.kind, selection.text.clone())),
                )
            }
        };
        self.drop_selection(cx);
        self.patch_pick = None;
        let draft = CommentDraft::new(
            target,
            None,
            initial,
            window,
            cx,
            |this: &mut Self, window, cx| this.submit_draft(window, cx),
        );
        let draft = match quote {
            Some((kind, excerpt)) => draft.with_quote(kind, excerpt),
            None => draft,
        };
        self.draft = Some(ChatDraft { anchor, draft });
        cx.notify();
    }

    /// Where a comment on a block of `turn` is stored: the first line of
    /// the turn's stored answer, else of its stored prompt. The block's
    /// passage goes in the comment's body ([`comments::proxy_body`]).
    pub(super) fn block_target(&self, turn: usize) -> Option<CommentTarget> {
        let turn_list = turns::segment_turns(&self.entries);
        let range = turn_list.get(turn)?.entries.clone();
        let entries = &self.entries[range];
        let answer = entries.iter().rev().find_map(|entry| match entry {
            ChatEntry::Assistant {
                markdown,
                streaming: false,
                message_id: Some(id),
            } if self.stored_len.get(id) == Some(&markdown.len()) => {
                comments::proxy_anchor(markdown).map(|range| (id.clone(), range))
            }
            _ => None,
        });
        let prompt = || {
            entries.iter().find_map(|entry| match entry {
                ChatEntry::User {
                    text,
                    message_id: Some(id),
                } => {
                    let base = user_body_start(text);
                    comments::proxy_anchor(&text[base..])
                        .map(|range| (id.clone(), range.start + base..range.end + base))
                }
                _ => None,
            })
        };
        let (message_id, range) = answer.or_else(prompt)?;
        Some(CommentTarget::MessageRange {
            message_id,
            start_byte: range.start,
            end_byte: range.end,
        })
    }

    /// A comment on picked diff lines: on the code they wrote when the file
    /// is in a known checkout, else a block comment quoting the lines.
    pub(super) fn open_patch_draft(
        &mut self,
        pick: PatchPick,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.uses_daemon() {
            return;
        }
        let found = self
            .entries
            .iter()
            .enumerate()
            .find_map(|(ix, entry)| match entry {
                ChatEntry::ToolCall { tool_use_id, .. } if tool_use_id == &pick.tool_use_id => {
                    Some((ix, self.tool_views.file_edit(entry)?))
                }
                _ => None,
            });
        let Some((ix, edit)) = found else {
            return;
        };
        let Some(hunk) = edit.hunks.get(pick.hunk) else {
            return;
        };
        let quoted: Vec<String> = hunk
            .lines
            .get(pick.lines())
            .unwrap_or_default()
            .iter()
            .map(tools::quoted_line)
            .collect();
        let (target, quote) =
            match comments::patch_target(&self.checkouts, &edit.path, hunk, pick.lines()) {
                Some(target) => (target, None),
                None => {
                    let turn = turns::segment_turns(&self.entries)
                        .iter()
                        .position(|turn| turn.entries.contains(&ix))
                        .unwrap_or(0);
                    let Some(target) = self.block_target(turn) else {
                        self.connection_error = Some(
                            "This turn is not saved yet; comment once it has an answer.".into(),
                        );
                        cx.notify();
                        return;
                    };
                    (target, Some((comments::KIND_PATCH, quoted.join("\n"))))
                }
            };
        self.drop_selection(cx);
        let draft = CommentDraft::new(
            target,
            None,
            initial,
            window,
            cx,
            |this: &mut Self, window, cx| this.submit_draft(window, cx),
        );
        let draft = match quote {
            Some((kind, excerpt)) => draft.with_quote(kind, excerpt),
            None => draft,
        };
        self.draft = Some(ChatDraft {
            anchor: DraftAnchor::Patch {
                tool_use_id: pick.tool_use_id.clone(),
            },
            draft,
        });
        cx.notify();
    }

    /// Pick a diff line (shift extends within the hunk) and take the keys,
    /// so typing comments on the pick.
    pub(crate) fn press_patch_line(
        &mut self,
        at: PatchLine,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if !self.uses_daemon() {
            return;
        }
        self.doc_focus.focus(window, cx);
        self.patch_pick = match self.patch_pick.take() {
            Some(mut pick)
                if extend && pick.tool_use_id == at.tool_use_id && pick.hunk == at.hunk =>
            {
                pick.cursor = at.line;
                Some(pick)
            }
            _ => Some(PatchPick {
                tool_use_id: at.tool_use_id,
                hunk: at.hunk,
                anchor: at.line,
                cursor: at.line,
            }),
        };
        self.patch_dragging = true;
        cx.notify();
    }

    pub(crate) fn drag_patch_line(&mut self, at: PatchLine, cx: &mut Context<Self>) {
        if let Some(pick) = self.patch_pick.as_mut()
            && self.patch_dragging
            && pick.tool_use_id == at.tool_use_id
            && pick.hunk == at.hunk
            && pick.cursor != at.line
        {
            pick.cursor = at.line;
            cx.notify();
        }
    }

    pub(super) fn open_reply(
        &mut self,
        root: CommentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self
            .comments
            .read(cx)
            .groups()
            .into_iter()
            .find(|group| group.id() == &root)
            .map(|group| group.root.target);
        let Some(target) = target else {
            return;
        };
        self.thread_open.insert(root.clone(), true);
        let draft = CommentDraft::new(
            target,
            Some(root.clone()),
            None,
            window,
            cx,
            |this: &mut Self, window, cx| this.submit_draft(window, cx),
        );
        self.draft = Some(ChatDraft {
            anchor: DraftAnchor::Reply(root),
            draft,
        });
        cx.notify();
    }

    pub(super) fn submit_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.draft else {
            return;
        };
        if open.draft.submit(&self.comments, cx) {
            if matches!(open.anchor, DraftAnchor::Patch { .. }) {
                self.patch_pick = None;
            }
            self.draft = None;
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    pub(super) fn cancel_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.draft.as_ref().map(|open| &open.anchor),
            Some(DraftAnchor::Patch { .. })
        ) {
            self.patch_pick = None;
        }
        self.draft = None;
        self.focus_composer(window, cx);
        cx.notify();
    }

    pub(super) fn toggle_thread(&mut self, id: CommentId, cx: &mut Context<Self>) {
        let default = self
            .comments
            .read(cx)
            .groups()
            .iter()
            .find(|group| group.id() == &id)
            .is_some_and(CommentGroup::open_by_default);
        let open = self.thread_open.get(&id).copied().unwrap_or(default);
        self.thread_open.insert(id, !open);
        cx.notify();
    }

    /// Scroll the transcript to a message and tint it briefly.
    pub fn reveal_message(&mut self, message_id: MessageId, cx: &mut Context<Self>) {
        self.reveal = Some(message_id.clone());
        self.flash = Some(message_id);
        self._flash_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1400))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.flash = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Deliver the pending comments as a turn of their own.
    pub(super) fn send_comments_only(&mut self, cx: &mut Context<Self>) {
        if !can_submit(self.lifecycle) || !self.uses_daemon() {
            return;
        }
        if self.comments.update(cx, |store, cx| store.send_pending(cx)) != SendStatus::Enqueued {
            return;
        }
        self.pending_prompt = None;
        self._event_task = None;
        self.last_failed = false;
        self.connection_error = None;
        self.lifecycle = TurnLifecycle::Running;
        self.final_assistant_persisted = false;
        self.status = AgentStatus::Running;
        self.composer.set_disabled(true, cx);
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    pub(super) fn card_handlers(&self, cx: &mut Context<Self>) -> CardHandlers {
        let toggle = cx.entity().downgrade();
        let reply = toggle.clone();
        let answer = toggle.clone();
        CardHandlers {
            store: self.comments.clone(),
            on_toggle: Rc::new(move |id, _, cx| {
                let _ = toggle.update(cx, |this, cx| this.toggle_thread(id, cx));
            }),
            on_reply: Rc::new(move |id, window, cx| {
                let _ = reply.update(cx, |this, cx| this.open_reply(id, window, cx));
            }),
            on_answer: Rc::new(move |id, _, cx| {
                let _ = answer.update(cx, |this, cx| this.reveal_message(id, cx));
            }),
        }
    }

    /// The selectable body of a persisted message, split after each
    /// commented block with the block's threads (and the open composer)
    /// right under it. Also highlights anchored passages and reports a
    /// selection inside the message. Returns the body and the message's
    /// threads.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn message_body(
        &mut self,
        message_id: &MessageId,
        source: &str,
        base: usize,
        strong: bool,
        ctx: &CardCtx,
        hits: &mut Vec<SelectionHit>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let groups = &ctx.groups;
        let (threads, anchors) = comments::message_anchors(groups, message_id, base, source);

        // Threads go under the block their passage ends in; threads whose
        // range no longer fits go after the message.
        let mut at_cut: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut trailing = Vec::new();
        for &group in &threads {
            match anchors.iter().find(|anchor| anchor.group == group) {
                Some(anchor) => {
                    let cut = comments::block_end(source, anchor.range.end);
                    match at_cut.iter_mut().find(|(at, _)| *at == cut) {
                        Some((_, list)) => list.push(group),
                        None => at_cut.push((cut, vec![group])),
                    }
                }
                None => trailing.push(group),
            }
        }
        let draft_range = match self.draft.as_ref().map(|open| &open.anchor) {
            Some(DraftAnchor::Passage {
                message_id: id,
                range,
            }) if id == message_id && range.start >= base && range.end - base <= source.len() => {
                Some(range.start - base..range.end - base)
            }
            _ => None,
        };
        let draft_cut = draft_range
            .as_ref()
            .map(|range| comments::block_end(source, range.end));
        let mut cuts: Vec<usize> = at_cut
            .iter()
            .map(|(cut, _)| *cut)
            .chain(draft_cut)
            .filter(|cut| *cut > 0 && *cut < source.len())
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut bounds = vec![0];
        bounds.extend(cuts);
        bounds.push(source.len());
        let ranges: Vec<Range<usize>> = bounds.windows(2).map(|w| w[0]..w[1]).collect();

        let view = self
            .message_views
            .entry(message_id.clone())
            .or_insert_with(|| MessageView {
                source: source.to_string(),
                segments: Vec::new(),
            });
        if view.source != source {
            view.source = source.to_string();
            view.segments.clear();
        }
        let mut old = std::mem::take(&mut view.segments);
        for range in &ranges {
            let segment = match old.iter().position(|segment| &segment.range == range) {
                Some(index) => old.remove(index),
                None => Segment {
                    range: range.clone(),
                    state: cx.new(|cx| {
                        TextViewState::markdown(&source[range.clone()], cx).selectable(true)
                    }),
                    applied: None,
                },
            };
            view.segments.push(segment);
        }

        // One highlight per passage (later overlapping ones are skipped);
        // the passage being commented on stays highlighted too.
        let mut marks: Vec<(Range<usize>, Hsla)> = anchors
            .iter()
            .filter_map(|anchor| {
                comments::passage_color(&groups[anchor.group].phase(), p)
                    .map(|color| (anchor.range.clone(), color))
            })
            .chain(draft_range.map(|range| (range, p.accent.opacity(0.3))))
            .collect();
        marks.sort_by_key(|(range, _)| (range.start, range.end));
        let mut desired: Vec<(Range<usize>, Hsla)> = Vec::new();
        for (range, color) in marks {
            if desired
                .last()
                .is_some_and(|(last, _)| range.start < last.end)
            {
                continue;
            }
            desired.push((range, color));
        }

        let mut selected: Option<Range<usize>> = None;
        for segment in &mut view.segments {
            let local: Vec<(Range<usize>, Hsla)> = desired
                .iter()
                .filter_map(|(range, color)| {
                    let start = range.start.max(segment.range.start);
                    let end = range.end.min(segment.range.end);
                    (start < end).then(|| {
                        (
                            start - segment.range.start..end - segment.range.start,
                            *color,
                        )
                    })
                })
                .collect();
            let rendered = segment.state.read(cx).rendered_text();
            let key = (rendered.clone(), local);
            if segment.applied.as_ref() != Some(&key) {
                let highlights: Vec<RangeHighlight> = key
                    .1
                    .iter()
                    .filter_map(|(range, color)| {
                        rendered
                            .range_for_source(range.clone())
                            .map(|range| RangeHighlight::new(range, *color))
                    })
                    .collect();
                segment.state.update(cx, |state, cx| {
                    let _ = state.set_range_highlights(highlights, cx);
                });
                segment.applied = Some(key);
            }
            // A drag across blocks selects in several parts: join them.
            if let Some(range) = segment.state.read(cx).selected_source_range()
                && range.start < range.end
            {
                let range = range.start + segment.range.start..range.end + segment.range.start;
                selected = Some(match selected {
                    Some(seen) => seen.start.min(range.start)..seen.end.max(range.end),
                    None => range,
                });
            }
        }
        if let Some(selected) = selected
            && let Some(range) = comments::selection_to_message_range(source, base, selected)
        {
            hits.push(SelectionHit::Message {
                message_id: message_id.clone(),
                range,
            });
        }

        let states: Vec<(Range<usize>, Entity<TextViewState>)> = view
            .segments
            .iter()
            .map(|segment| (segment.range.clone(), segment.state.clone()))
            .collect();
        let last = source.len();
        let mut body = v_flex().w_full().gap(theme::sp(2.));
        for (range, state) in states {
            body = body.child(
                div()
                    .w_full()
                    .font_family(theme::PROSE_FONT)
                    .text_size(theme::text_prose())
                    .line_height(relative(1.6))
                    .text_color(p.text)
                    .when(strong, |this| this.font_weight(FontWeight::MEDIUM))
                    .child(TextView::new(&state).selectable(true)),
            );
            let ends_here = |cut: usize| cut == range.end || (range.end == last && cut >= last);
            let mut under: Vec<usize> = at_cut
                .iter()
                .filter(|(cut, _)| ends_here(*cut))
                .flat_map(|(_, list)| list.iter().copied())
                .collect();
            if range.end == last {
                under.extend(trailing.iter().copied());
            }
            for group in under {
                body = body.child(self.inline_card(ctx, group, "inline", p, cx));
            }
            if draft_cut.is_some_and(ends_here)
                && let Some(open) = &self.draft
            {
                body = body.child(self.render_open_draft("chat-draft", open, p, cx));
            }
        }
        body.into_any_element()
    }

    /// A thread as a card in the document, with the reply composer when
    /// it is open on this thread (drawn under the first card only).
    pub(super) fn inline_card(
        &self,
        ctx: &CardCtx,
        index: usize,
        place: &str,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let group = &ctx.groups[index];
        let answer = match group.phase() {
            Phase::Delivered(delivery) | Phase::Resolved(Some(delivery)) => delivery
                .answer_message_id
                .and_then(|id| ctx.answers.get(&id).map(|text| (id.clone(), text.clone()))),
            _ => None,
        };
        let reply = if ctx.reply_drawn.get() {
            None
        } else {
            self.draft
                .as_ref()
                .filter(|open| open.anchor == DraftAnchor::Reply(group.id().clone()))
                .map(|open| {
                    ctx.reply_drawn.set(true);
                    self.render_open_draft("chat-reply", open, p, cx)
                })
        };
        comments::render_inline(
            format!("chat-{place}-{}", group.id()),
            group,
            answer,
            reply,
            ctx.me.as_deref(),
            &ctx.handlers,
            p,
        )
    }

    pub(super) fn render_open_draft(
        &self,
        id: &str,
        open: &ChatDraft,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let submit = cx.listener(|this, _, window, cx| this.submit_draft(window, cx));
        let weak = cx.entity().downgrade();
        let cancel: comments::CancelDraft = Rc::new(move |window, cx| {
            let _ = weak.update(cx, |this, cx| this.cancel_draft(window, cx));
        });
        comments::render_draft(id.to_string(), &open.draft, submit, cancel, p, cx)
    }
    /// "Sent N comments" above a delivered prompt; expands to the block.
    pub(super) fn sent_block(
        &self,
        message_id: &MessageId,
        text: &str,
        block: &PromptBlock,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open_blocks.contains(message_id);
        let id = message_id.clone();
        let label = if block.entries == 0 {
            "Sent comments".to_string()
        } else {
            format!(
                "Sent {}",
                comments::plural(block.entries, "comment", "comments")
            )
        };
        v_flex()
            .w_full()
            .gap(theme::sp(1.5))
            .child(
                h_flex().child(
                    h_flex()
                        .id(SharedString::from(format!("sent-block-{message_id}")))
                        .h(px(24.))
                        .px(theme::sp(2.))
                        .gap(theme::sp(1.5))
                        .items_center()
                        .rounded(theme::radius())
                        .border_1()
                        .border_color(p.border)
                        .bg(p.elevated)
                        .cursor_pointer()
                        .text_size(theme::text_xs())
                        .text_color(p.text_muted)
                        .hover(|style| style.bg(p.hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.open_blocks.remove(&id) {
                                this.open_blocks.insert(id.clone());
                            }
                            cx.notify();
                        }))
                        .child(
                            Icon::new(IconName::MessageSquareText)
                                .with_size(px(12.))
                                .text_color(p.accent_text),
                        )
                        .child(div().text_color(p.text).child(label))
                        .child(
                            Icon::new(if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .with_size(px(11.))
                            .text_color(p.text_faint),
                        ),
                ),
            )
            .when(open, |this| {
                this.child(
                    div()
                        .w_full()
                        .px(theme::sp(2.5))
                        .py(theme::sp(2.))
                        .rounded(theme::radius())
                        .bg(p.bg)
                        .border_1()
                        .border_color(p.border)
                        .font_family(theme::MONO_FONT)
                        .text_size(theme::text_xs())
                        .line_height(relative(1.5))
                        .text_color(p.text_muted)
                        .child(text[block.block.clone()].to_string()),
                )
            })
            .into_any_element()
    }

    /// Pending comments above the composer, with "Send comments only".
    pub(super) fn pending_strip(
        &self,
        groups: &[CommentGroup],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pending: Vec<&CommentGroup> = groups
            .iter()
            .filter(|group| group.phase() == Phase::Pending)
            .collect();
        let notice = self.comments.read(cx).notice.clone();
        if pending.is_empty() && notice.is_none() {
            return None;
        }
        let total = comments::pending_total(groups);
        let can_send = can_submit(self.lifecycle) && self.uses_daemon();
        let store = self.comments.clone();
        let chip = |group: &CommentGroup| {
            let root = &group.root;
            let reveal = match &root.target {
                CommentTarget::MessageRange { message_id, .. } => Some(message_id.clone()),
                _ => None,
            };
            let icon = match &root.target {
                CommentTarget::MessageRange { .. } => IconName::MessageSquare,
                CommentTarget::Code { .. } => IconName::FileText,
                CommentTarget::DiffHunk { .. } => IconName::FileDiff,
            };
            let mut tip = comments::target_label(&root.target);
            if let Some(excerpt) = &root.excerpt {
                tip = format!("{tip}\n{}", comments::one_line(excerpt, 120));
            }
            let tooltip: SharedString = tip.into();
            let delete = comments::can_delete(root).then(|| {
                let store = store.clone();
                let comment_id = root.id.clone();
                div()
                    .id(SharedString::from(format!(
                        "pending-chip-delete-{}",
                        root.id
                    )))
                    .size(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::radius_sm())
                    .cursor_pointer()
                    .hover(|style| style.bg(p.hover))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        store.update(cx, |store, cx| store.delete(comment_id.clone(), cx));
                    })
                    .child(
                        Icon::new(IconName::Close)
                            .with_size(px(10.))
                            .text_color(p.text_faint),
                    )
            });
            h_flex()
                .id(SharedString::from(format!("pending-chip-{}", root.id)))
                .h(px(24.))
                .max_w(px(280.))
                .pl(theme::sp(2.))
                .pr(theme::sp(1.))
                .gap(theme::sp(1.5))
                .items_center()
                .rounded(theme::radius())
                .border_1()
                .border_color(p.accent.opacity(0.35))
                .bg(p.accent_soft)
                .text_size(theme::text_xs())
                .text_color(p.text)
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .when_some(reveal, |this, message_id| {
                    this.cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.reveal_message(message_id.clone(), cx);
                        }))
                })
                .child(Icon::new(icon).with_size(px(11.)).text_color(p.accent_text))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(comments::one_line(&root.body, 48)),
                )
                .when(
                    group
                        .replies
                        .iter()
                        .any(|reply| reply.state == trama_protocol::CommentState::Pending),
                    |this| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .text_color(p.text_faint)
                                .child("reply"),
                        )
                    },
                )
                .children(delete)
        };
        Some(
            v_flex()
                .w_full()
                .pb(theme::sp(2.))
                .gap(theme::sp(1.5))
                .when(!pending.is_empty(), |this| {
                    this.child(
                        h_flex()
                            .w_full()
                            .gap(theme::sp(1.5))
                            .items_center()
                            .text_size(theme::text_xs())
                            .text_color(p.text_muted)
                            .child(
                                Icon::new(IconName::MessageSquare)
                                    .with_size(px(12.))
                                    .text_color(p.accent_text),
                            )
                            .child(div().flex_1().child(format!(
                                "{} will be sent with your next message",
                                comments::plural(total, "comment", "comments")
                            )))
                            .child(
                                Button::new("send-comments-only")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::ArrowUp)
                                    .label("Send comments only")
                                    .tooltip("Deliver the comments now, without a message")
                                    .disabled(!can_send)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.send_comments_only(cx)),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .gap(theme::sp(1.5))
                            .children(pending.iter().map(|group| chip(group))),
                    )
                })
                .when_some(notice, |this, notice| {
                    this.child(
                        div()
                            .text_size(theme::text_xs())
                            .text_color(p.warning)
                            .child(notice),
                    )
                })
                .into_any_element(),
        )
    }
}

/// Where the person's own text starts in a stored user message.
pub(super) fn user_body_start(text: &str) -> usize {
    comments::split_prompt_block(text)
        .map(|block| block.body_start)
        .unwrap_or(0)
}
