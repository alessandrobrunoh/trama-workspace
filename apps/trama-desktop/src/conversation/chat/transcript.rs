use super::*;

impl ChatPanel {
    /// Comments can be read and written here.
    pub(super) fn comments_enabled(&self) -> bool {
        self.uses_daemon() || self.demo
    }

    /// Show a scripted transcript (`TRAMA_DEV_DEMO`, debug builds).
    pub fn load_demo(
        &mut self,
        thread: ThreadId,
        entries: Vec<ChatEntry>,
        received: Vec<(MessageId, crate::subagents::AnnotationRow)>,
        cx: &mut Context<Self>,
    ) {
        self.demo = true;
        self.selected_thread = Some(thread);
        for entry in &entries {
            if let ChatEntry::Assistant {
                markdown,
                message_id: Some(id),
                ..
            } = entry
            {
                self.stored_len.insert(id.clone(), markdown.len());
            }
        }
        self.entries = entries;
        self.received = received.into_iter().collect();
        if crate::devhooks::from_env().is_some_and(|dev| dev.bottom) {
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Show a subagent's transcript in the side pane (rather than as the
    /// selected thread).
    pub fn set_pane_mode(&mut self, pane: bool) {
        self.pane_mode = pane;
    }

    /// The selected thread's checkouts, for code comments on diff lines.
    pub fn set_checkouts(
        &mut self,
        thread_id: &ThreadId,
        checkouts: &[trama_protocol::CheckoutSnapshot],
        cx: &mut Context<Self>,
    ) {
        if !self.thread_matches(thread_id) {
            return;
        }
        self.checkouts = checkouts
            .iter()
            .map(|checkout| {
                (
                    checkout.repository_id.to_string(),
                    checkout
                        .checkout_path
                        .clone()
                        .unwrap_or_else(|| checkout.source_path.clone()),
                )
            })
            .collect();
        cx.notify();
    }

    /// Whether the text of block `key` is selected (a click on it then
    /// ends a selection instead of opening the block).
    pub(crate) fn text_selected(&self, key: &str, cx: &App) -> bool {
        self.block_texts.has_selection(key, cx)
    }

    pub(crate) fn toggle_thought(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.tool_views.open_thoughts.remove(&index) {
            self.tool_views.open_thoughts.insert(index);
        }
        cx.notify();
    }

    /// A subagent line was clicked: show it next to this transcript, or
    /// select it (`main`, ⌘-click).
    pub(crate) fn open_agent(&mut self, thread: ThreadId, main: bool, cx: &mut Context<Self>) {
        cx.emit(if main {
            ChatEvent::OpenThread(thread)
        } else {
            ChatEvent::OpenInPane(thread)
        });
    }

    pub(super) fn entry_author(&self, entry: &ChatEntry) -> AuthorKey {
        document::entry_author(
            entry,
            |id| self.remote_authors.get(id).cloned(),
            |id| self.received.get(id).map(|row| row.thread_id.clone()),
        )
    }

    /// "Subagent started" notes of subagents whose spawning call is shown:
    /// the call's "Agent spawned" line says it already.
    pub(super) fn hidden_entries(&self, entries: &[ChatEntry]) -> HashSet<usize> {
        let spawned: HashSet<&ThreadId> = entries
            .iter()
            .filter_map(|entry| match entry {
                ChatEntry::ToolCall {
                    tool_use_id, name, ..
                } if tools::tool_kind(name) == ToolKind::Subthread => {
                    self.thread_index.spawned_by_tool(tool_use_id)
                }
                _ => None,
            })
            .collect();
        entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                matches!(
                    entry,
                    ChatEntry::Annotation(row)
                        if matches!(row.kind, crate::subagents::AnnotationKind::SubagentStarted(_))
                            && spawned.contains(&row.thread_id)
                )
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Which turn and block each comment thread goes under.
    pub(super) fn place_comments(
        &self,
        entries: &[ChatEntry],
        turn_list: &[turns::Turn],
        groups: &[CommentGroup],
        p: &Palette,
    ) -> Placement {
        let mut placement = Placement::default();
        let turn_of = |index: usize| {
            turn_list
                .iter()
                .rposition(|turn| turn.entries.start <= index)
                .unwrap_or(0)
        };
        let mut message_turn: HashMap<&MessageId, usize> = HashMap::new();
        let mut edits: Vec<(usize, &str, Rc<tools::FileEdit>)> = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            match entry {
                ChatEntry::User {
                    message_id: Some(id),
                    ..
                }
                | ChatEntry::Assistant {
                    message_id: Some(id),
                    ..
                } => {
                    message_turn.insert(id, turn_of(index));
                }
                ChatEntry::ToolCall {
                    tool_use_id, name, ..
                } if tools::tool_kind(name) == ToolKind::Edit => {
                    if let Some(edit) = self.tool_views.file_edit(entry) {
                        edits.push((turn_of(index), tool_use_id.as_str(), edit));
                    }
                }
                _ => {}
            }
        }
        for (index, group) in groups.iter().enumerate() {
            let color =
                comments::passage_color(&group.phase(), p).unwrap_or(gpui_kit::transparent_black());
            match &group.root.target {
                CommentTarget::MessageRange { message_id, .. } => {
                    let Some(&turn) = message_turn.get(message_id) else {
                        continue;
                    };
                    placement.by_turn.entry(turn).or_default().push(index);
                    let Some(proxy) = comments::parse_proxy(&group.root.body) else {
                        continue;
                    };
                    if proxy.kind == comments::KIND_PATCH {
                        let wanted: Vec<&str> = proxy
                            .excerpt
                            .lines()
                            .filter(|line| !line.trim().is_empty())
                            .collect();
                        for (edit_turn, id, edit) in &edits {
                            if *edit_turn != turn {
                                continue;
                            }
                            let lines = tools::patch_lines_matching(edit, &wanted);
                            if !lines.is_empty() {
                                let slot = placement.patches.entry(id.to_string()).or_default();
                                slot.lines.extend(lines);
                                slot.groups.push(index);
                                break;
                            }
                        }
                    } else {
                        placement.marks.entry(turn).or_default().push(Mark {
                            group: Some(index),
                            kind: proxy.kind,
                            excerpt: proxy.excerpt,
                            color,
                        });
                    }
                }
                CommentTarget::Code { anchor } => {
                    for (turn, id, edit) in &edits {
                        if !comments::code_on_edit(anchor, &edit.path, &tools::new_lines(edit)) {
                            continue;
                        }
                        let slot = placement.patches.entry(id.to_string()).or_default();
                        slot.lines.extend(tools::patch_lines_in(
                            edit,
                            anchor.start_line..=anchor.end_line,
                        ));
                        slot.groups.push(index);
                        placement.by_turn.entry(*turn).or_default().push(index);
                        break;
                    }
                }
                CommentTarget::DiffHunk { .. } => {}
            }
        }
        if let Some(DraftAnchor::Block {
            kind,
            excerpt,
            turn,
            ..
        }) = self.draft.as_ref().map(|open| &open.anchor)
        {
            placement.marks.entry(*turn).or_default().push(Mark {
                group: None,
                kind,
                excerpt: excerpt.clone(),
                color: p.accent.opacity(0.3),
            });
        }
        placement
    }

    /// An author's round avatar for the gutter.
    pub(super) fn avatar(
        &self,
        author: &AuthorKey,
        me: Option<&str>,
        row: usize,
        p: &Palette,
    ) -> AnyElement {
        let (element, tip): (AnyElement, String) = match author {
            AuthorKey::Me => (
                match me {
                    Some(name) => {
                        crate::presence::avatar(p, name, AVATAR, false).into_any_element()
                    }
                    None => {
                        glyph_avatar(IconName::User, p.selected, p.text_muted).into_any_element()
                    }
                },
                "You".into(),
            ),
            AuthorKey::Person(name) => (
                crate::presence::avatar(p, name, AVATAR, false).into_any_element(),
                name.clone(),
            ),
            AuthorKey::Agent => (
                glyph_avatar(IconName::Bot, p.accent_soft, p.accent_text).into_any_element(),
                "Claude".into(),
            ),
            AuthorKey::PersonAgent(name) => (
                div()
                    .relative()
                    .child(crate::presence::avatar(p, name, AVATAR, false))
                    .child(
                        div()
                            .absolute()
                            .bottom(px(-3.))
                            .right(px(-3.))
                            .size(px(12.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(p.accent_soft)
                            .border_1()
                            .border_color(p.bg)
                            .child(
                                Icon::new(IconName::Bot)
                                    .with_size(px(8.))
                                    .text_color(p.accent_text),
                            ),
                    )
                    .into_any_element(),
                format!("{name}'s Claude"),
            ),
            AuthorKey::ThreadAgent(thread) => (
                glyph_avatar(IconName::Bot, p.selected, p.text_muted).into_any_element(),
                format!(
                    "Agent of {}",
                    self.thread_index.title(thread).unwrap_or("another thread")
                ),
            ),
        };
        let tip: SharedString = tip.into();
        div()
            .id(("doc-avatar", row))
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .child(element)
            .into_any_element()
    }

    /// One layout item as a block, with the threads on its passages and
    /// the composer of a new one under it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_item(
        &mut self,
        item: &Item,
        entries: &[ChatEntry],
        turn: usize,
        placement: &Placement,
        ctx: &CardCtx,
        hits: &mut Vec<SelectionHit>,
        row_of: &mut HashMap<MessageId, usize>,
        row: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let drawn_from = self.block_texts.drawn_len();
        let no_marks = Vec::new();
        let marks = placement.marks.get(&turn).unwrap_or(&no_marks);
        let content = match item {
            Item::Group(range) => {
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                tools::render_activity(range.clone(), entries, &mut doc, cx)
            }
            Item::Edits(range) => {
                self.render_edits(range.clone(), entries, turn, placement, ctx, marks, p, cx)
            }
            Item::Single(ix) => {
                self.render_single(*ix, entries, turn, ctx, marks, hits, row_of, row, p, cx)
            }
        };
        let claimed = self.block_texts.take_claimed();
        let draft_here = match self.draft.as_ref().map(|open| &open.anchor) {
            Some(DraftAnchor::Block { key, .. }) => self.block_texts.drawn_since(drawn_from, key),
            _ => false,
        };
        if claimed.is_empty() && !draft_here {
            return content;
        }
        let mut column = v_flex().w_full().gap(theme::sp(1.5)).child(content);
        for group in claimed {
            column = column.child(self.inline_card(ctx, group, "inline", p, cx));
        }
        if draft_here && let Some(open) = &self.draft {
            column = column.child(self.render_open_draft("chat-block-draft", open, p, cx));
        }
        column.into_any_element()
    }

    /// File edits: an "Apply patch" line and diff each, with the threads
    /// on their lines.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_edits(
        &mut self,
        range: Range<usize>,
        entries: &[ChatEntry],
        turn: usize,
        placement: &Placement,
        ctx: &CardCtx,
        marks: &[Mark],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let empty = PatchComments::default();
        let mut column = v_flex().w_full().gap(theme::sp(1.5));
        for ix in range {
            let entry = &entries[ix];
            let ChatEntry::ToolCall { tool_use_id, .. } = entry else {
                continue;
            };
            let on_lines = placement.patches.get(tool_use_id).unwrap_or(&empty);
            let element = {
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                match tools::render_patch(
                    ix,
                    entry,
                    &mut doc,
                    self.patch_pick.as_ref(),
                    &on_lines.lines,
                    cx,
                ) {
                    Some(element) => element,
                    None => tools::render_activity(ix..ix + 1, entries, &mut doc, cx),
                }
            };
            column = column.child(element);
            for &group in &on_lines.groups {
                column = column.child(self.inline_card(ctx, group, "inline", p, cx));
            }
            if let Some(open) = &self.draft
                && matches!(&open.anchor, DraftAnchor::Patch { tool_use_id: id } if id == tool_use_id)
            {
                column = column.child(self.render_open_draft("chat-patch-draft", open, p, cx));
            }
        }
        column.into_any_element()
    }

    /// One entry as a block.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_single(
        &mut self,
        ix: usize,
        entries: &[ChatEntry],
        turn: usize,
        ctx: &CardCtx,
        marks: &[Mark],
        hits: &mut Vec<SelectionHit>,
        row_of: &mut HashMap<MessageId, usize>,
        row: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entry = &entries[ix];
        match entry {
            ChatEntry::User { text, message_id } => {
                self.render_prompt(ix, text, message_id.as_ref(), ctx, hits, row_of, row, p, cx)
            }
            ChatEntry::Approval(card) => render_approval(card, p, cx),
            ChatEntry::Assistant {
                markdown,
                message_id: Some(id),
                ..
            } if self.remote_authors.contains_key(id) => {
                let (name, role) = self.remote_authors[id].clone();
                if role == Role::Assistant && self.comments_enabled() {
                    let body = self.message_body(id, markdown, 0, false, ctx, hits, p, cx);
                    row_of.insert(id.clone(), row);
                    self.message_row(id, body, p, cx)
                } else {
                    crate::presence::remote_notice(p, &name, markdown)
                }
            }
            ChatEntry::Assistant {
                markdown,
                streaming: false,
                message_id: Some(id),
            } if self.comments_enabled() && self.stored_len.get(id) == Some(&markdown.len()) => {
                let body = self.message_body(id, markdown, 0, false, ctx, hits, p, cx);
                row_of.insert(id.clone(), row);
                self.message_row(id, body, p, cx)
            }
            ChatEntry::Assistant {
                markdown,
                streaming,
                ..
            } => div()
                .w_full()
                .font_family(theme::PROSE_FONT)
                .text_size(theme::text_prose())
                .line_height(relative(1.6))
                .text_color(p.text)
                .child(
                    TextView::markdown(("assistant-md", ix), markdown.clone())
                        .stream_fade(*streaming),
                )
                .into_any_element(),
            ChatEntry::ToolCall {
                tool_use_id,
                name,
                input_json,
                output,
                finished,
                is_error,
                ..
            } if tools::tool_kind(name) == ToolKind::Subthread => {
                let mut link =
                    tools::subthread_link(name, input_json, output.as_ref(), &self.known_threads);
                crate::subagents::link_spawned_child(&mut link, tool_use_id, &self.thread_index);
                let (label, detail) = if link.message {
                    ("Message to agent:", link.title.clone())
                } else {
                    let child = link.thread.as_ref();
                    (
                        "Agent spawned:",
                        tools::agent_detail(
                            &link.title,
                            child
                                .and_then(|id| self.thread_index.profile(id))
                                .map(|profile| profile.label()),
                            child.and_then(|id| self.thread_index.model(id)),
                        ),
                    )
                };
                let trailing = match link
                    .thread
                    .as_ref()
                    .and_then(|id| self.thread_index.status(id))
                {
                    Some(status) => vec![crate::subagents::status_glyph(status, 12., p)],
                    None if !*finished || *is_error => {
                        vec![tools::status_icon(*finished, *is_error, p)]
                    }
                    None => Vec::new(),
                };
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                tools::render_agent(
                    ("agent-call", ix),
                    label,
                    &detail,
                    format!("agent-{tool_use_id}"),
                    link.thread,
                    trailing,
                    None,
                    &mut doc,
                    cx,
                )
            }
            ChatEntry::Annotation(note) => self.render_note(ix, note, turn, marks, p, cx),
            ChatEntry::ToolCall { name, .. } if tools::tool_kind(name) == ToolKind::Edit => {
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                let empty = HashSet::new();
                tools::render_patch(ix, entry, &mut doc, self.patch_pick.as_ref(), &empty, cx)
                    .unwrap_or_else(|| tools::render_activity(ix..ix + 1, entries, &mut doc, cx))
            }
            ChatEntry::ToolCall { .. } | ChatEntry::Thinking { .. } => {
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                tools::render_activity(ix..ix + 1, entries, &mut doc, cx)
            }
        }
    }

    /// A prompt as plain document text, a little stronger than prose: the
    /// comments it delivered, attached terminal snapshots, then the text.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_prompt(
        &mut self,
        ix: usize,
        text: &str,
        message_id: Option<&MessageId>,
        ctx: &CardCtx,
        hits: &mut Vec<SelectionHit>,
        row_of: &mut HashMap<MessageId, usize>,
        row: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut block = v_flex().w_full().gap(theme::sp(1.5));
        if let Some(note) = message_id.and_then(|id| self.received.get(id)).cloned() {
            block = block.child(crate::subagents::agent_message_header(
                ix,
                &note,
                &self.thread_index,
                p,
                cx,
            ));
        }
        match message_id {
            Some(id) if self.comments_enabled() => {
                let split = comments::split_prompt_block(text);
                let base = split.as_ref().map_or(0, |split| split.body_start);
                if let Some(split) = &split {
                    block = block.child(self.sent_block(id, text, split, p, cx));
                }
                let context = snapshot::split_context(&text[base..]);
                if !context.is_empty() {
                    block = block.child(snapshot::render_context(
                        id,
                        &context,
                        &self.open_snapshots,
                        p,
                        cx,
                    ));
                }
                let base = base + context.text.start;
                let body = &text[base..base + context.text.len()];
                if !body.trim().is_empty() {
                    let content = self.message_body(id, body, base, true, ctx, hits, p, cx);
                    block = block.child(self.message_row(id, content, p, cx));
                }
                row_of.insert(id.clone(), row);
            }
            _ => {
                block = block.child(
                    div()
                        .font_family(theme::PROSE_FONT)
                        .text_size(theme::text_prose())
                        .line_height(relative(1.6))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.text)
                        .child(comments::prompt_body(text).to_string()),
                );
            }
        }
        block.into_any_element()
    }

    /// A stored note linking another thread: "Agent spawned: …", "Agent
    /// completed: …", or an agent message sent or received.
    pub(super) fn render_note(
        &mut self,
        ix: usize,
        note: &crate::subagents::AnnotationRow,
        turn: usize,
        marks: &[Mark],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::subagents::AnnotationKind;
        let known = self.thread_index.title(&note.thread_id).is_some();
        let thread = known.then(|| note.thread_id.clone());
        let title = self
            .thread_index
            .title(&note.thread_id)
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&note.title)
            .to_string();
        let key = format!("note-{}", note.message_id);
        match &note.kind {
            AnnotationKind::SubagentStarted(profile) => {
                let detail = tools::agent_detail(
                    &title,
                    Some(profile.label()),
                    self.thread_index.model(&note.thread_id),
                );
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                tools::render_agent(
                    ("agent-note", ix),
                    tools::agent_label(None, true),
                    &detail,
                    key,
                    thread,
                    Vec::new(),
                    Some(crate::subagents::row_text(note, &self.thread_index)),
                    &mut doc,
                    cx,
                )
            }
            AnnotationKind::SubagentFinished(status) => {
                let detail = match &note.body {
                    Some(body) => format!("{title} \u{2014} {body}"),
                    None => title,
                };
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                tools::render_agent(
                    ("agent-note", ix),
                    tools::agent_label(Some(*status), false),
                    &detail,
                    key,
                    thread,
                    vec![crate::subagents::status_glyph(*status, 12., p)],
                    Some(crate::subagents::row_text(note, &self.thread_index)),
                    &mut doc,
                    cx,
                )
            }
            AnnotationKind::Sent | AnnotationKind::Received => {
                let header =
                    crate::subagents::agent_message_header(ix, note, &self.thread_index, p, cx);
                let body = note.body.clone().map(|body| {
                    let view = self.block_texts.view(
                        &key,
                        &crate::doc_text::escape_markdown(&body),
                        &body,
                        comments::KIND_SUBAGENT,
                        turn,
                        marks,
                        cx,
                    );
                    div()
                        .w_full()
                        .pl(theme::sp(3.))
                        .border_l_2()
                        .border_color(p.border_strong)
                        .text_size(theme::text_ui())
                        .line_height(relative(1.5))
                        .text_color(p.text_muted)
                        .child(view)
                });
                v_flex()
                    .w_full()
                    .gap(theme::sp(1.))
                    .child(header)
                    .children(body)
                    .into_any_element()
            }
        }
    }

    /// A stored message with its hover actions (comment, fork) right of
    /// the text column.
    pub(super) fn message_row(
        &self,
        message_id: &MessageId,
        content: AnyElement,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hover: SharedString = format!("msg-{message_id}").into();
        let id = message_id.clone();
        let add = div()
            .id(SharedString::from(format!("comment-add-{message_id}")))
            .size(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::radius_sm())
            .cursor_pointer()
            .hover(|style| style.bg(p.hover))
            .tooltip(|window, cx| {
                Tooltip::new(
                    "Comment on the selection or the whole message. Tip: select text anywhere and start typing.",
                )
                .build(window, cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.comment_on_message(id.clone(), window, cx);
            }))
            .child(
                Icon::new(IconName::MessageSquarePlus)
                    .with_size(px(13.))
                    .text_color(p.text_faint),
            );
        div()
            .relative()
            .w_full()
            .group(hover.clone())
            .child(content)
            .when(self.uses_daemon(), |this| {
                let fork =
                    crate::forks::fork_button(message_id, self.fork_index.state(message_id), p, cx);
                this.child(
                    v_flex()
                        .absolute()
                        .top_0()
                        .right(px(-30.))
                        .gap(theme::sp(0.5))
                        .opacity(0.)
                        .group_hover(hover, |style| style.opacity(1.))
                        .child(add)
                        .child(fork),
                )
            })
            .into_any_element()
    }

    /// "N comments ⌄" at the end of a turn: the passage of each thread on
    /// it, quoted, with the thread's comments and replies.
    pub(super) fn turn_comments(
        &self,
        turn: usize,
        list: &[usize],
        ctx: &CardCtx,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let default_open = list
            .iter()
            .any(|&group| ctx.groups[group].phase() == Phase::Pending);
        let open = self
            .turn_comments_open
            .get(&turn)
            .copied()
            .unwrap_or(default_open);
        let total: usize = list.iter().map(|&group| ctx.groups[group].len()).sum();
        let header = h_flex()
            .id(("turn-comments", turn))
            .min_h(px(22.))
            .gap(theme::sp(1.5))
            .items_center()
            .cursor_pointer()
            .text_size(theme::text_ui())
            .text_color(p.text_muted)
            .hover(|style| style.text_color(p.text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.turn_comments_open.insert(turn, !open);
                cx.notify();
            }))
            .child(comments::plural(total, "comment", "comments"))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .with_size(px(12.))
                .text_color(p.text_faint),
            );
        let mut column = v_flex().w_full().gap(theme::sp(2.)).child(header);
        if open {
            for &index in list {
                let group = &ctx.groups[index];
                let quote = match comments::parse_proxy(&group.root.body) {
                    Some(proxy) => proxy.excerpt,
                    None => group
                        .root
                        .excerpt
                        .clone()
                        .unwrap_or_else(|| comments::target_label(&group.root.target)),
                };
                column = column.child(
                    v_flex()
                        .w_full()
                        .p(theme::sp(2.))
                        .gap(theme::sp(2.))
                        .rounded(theme::radius())
                        .border_1()
                        .border_color(p.border)
                        .child(
                            div()
                                .w_full()
                                .truncate()
                                .px(theme::sp(1.))
                                .rounded(theme::radius_sm())
                                .bg(comments::mark_color(p).opacity(0.14))
                                .text_size(theme::text_ui())
                                .line_height(relative(1.6))
                                .text_color(p.text)
                                .child(comments::one_line(&quote, 160)),
                        )
                        .child(self.inline_card(ctx, index, "turn", p, cx)),
                );
            }
        }
        column.into_any_element()
    }
}

// -- turn rail --------------------------------------------------------------
