use super::*;

impl ChatPanel {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        client: DaemonClient,
        comments: Entity<CommentStore>,
    ) -> Self {
        let composer = Composer::new(window, cx);
        let enter = composer.subscribe_enter(window, cx, |this, window, cx| {
            this.submit(window, cx);
        });
        let comments_observer = cx.observe(&comments, |_, _, cx| cx.notify());
        let popup_input = composer.input.clone();
        let popup_client = client.clone();
        let popup = cx.new(|cx| ComposerPopup::new(popup_client, popup_input, cx));
        let picker_client = client.clone();
        let model_picker = cx.new(|_| ModelPicker::new(picker_client));
        let terminal_client = client.clone();
        let terminals = cx.new(|cx| TerminalPanel::new(terminal_client, cx));
        let composer_extras = vec![
            cx.observe(&popup, |_, _, cx| cx.notify()),
            cx.observe(&model_picker, |_, _, cx| cx.notify()),
            cx.observe(&terminals, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &terminals,
                window,
                |this, _, event, window, cx| match event {
                    TerminalEvent::Hidden => this.focus_composer(window, cx),
                },
            ),
        ];

        Self {
            entries: Vec::new(),
            comments,
            message_views: HashMap::new(),
            stored_len: HashMap::new(),
            selection: None,
            selection_point: None,
            draft: None,
            thread_open: HashMap::new(),
            open_blocks: HashSet::new(),
            reveal: None,
            flash: None,
            _flash_task: None,
            _comments_observer: comments_observer,
            composer,
            agent: Box::new(FakeAgent),
            client,
            connected: false,
            selected_thread: None,
            drafts: HashMap::new(),
            restore_thread_draft: false,
            lifecycle: TurnLifecycle::Idle,
            final_assistant_persisted: false,
            pending_prompt: None,
            seen_message_ids: HashSet::new(),
            connection_error: None,
            status: AgentStatus::Idle,
            usage: None,
            cost_usd_micros: None,
            turns: None,
            model: None,
            session_info: None,
            completed_turns: 0,
            last_failed: false,
            scroll: ScrollHandle::new(),
            tool_views: ToolViewState::default(),
            rail: RailState::default(),
            bookmarks: Bookmarks::load(),
            prompt_times: HashMap::new(),
            known_threads: Vec::new(),
            thread_index: Default::default(),
            fork_index: Default::default(),
            received: HashMap::new(),
            collab: None,
            remote_authors: HashMap::new(),
            popup,
            model_picker,
            terminals,
            open_snapshots: HashSet::new(),
            block_texts: BlockTexts::default(),
            patch_pick: None,
            patch_dragging: false,
            doc_focus: cx.focus_handle(),
            turn_comments_open: HashMap::new(),
            checkouts: Vec::new(),
            pane_mode: false,
            demo: false,
            _composer_extras: composer_extras,
            _enter: enter,
            _event_task: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.lifecycle != TurnLifecycle::Idle
    }

    pub fn set_collab(&mut self, collab: Entity<crate::collab::CollabStore>) {
        self.collab = Some(collab);
    }

    /// The composer's text state, for relaying live drafts.
    pub fn composer_input(&self) -> Entity<gpui_kit::component::input::TextareaState> {
        self.composer.input.clone()
    }

    pub fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.input.focus_handle(cx).focus(window, cx);
    }

    /// Approvals still waiting for the user, oldest first.
    pub fn open_approvals(&self) -> impl Iterator<Item = &ApprovalCard> {
        self.entries.iter().filter_map(|entry| match entry {
            ChatEntry::Approval(card) if card.state == ApprovalState::Pending => Some(card),
            _ => None,
        })
    }

    pub(super) fn has_open_approval(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry, ChatEntry::Approval(card) if card.is_open()))
    }

    /// Answer the oldest pending approval (keyboard shortcuts).
    pub fn answer_oldest_approval(&mut self, decision: ApprovalDecision, cx: &mut Context<Self>) {
        let request_id = self
            .open_approvals()
            .next()
            .map(|card| card.request_id.clone());
        if let Some(request_id) = request_id {
            self.answer_approval(&request_id, decision, cx);
        }
    }

    pub fn answer_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.selected_thread.clone() else {
            return;
        };
        let Some(message) = answer_approval(&mut self.entries, &thread_id, request_id, decision)
        else {
            return;
        };
        if self.client.send(message) == SendStatus::Rejected {
            set_approval_state(&mut self.entries, request_id, ApprovalState::Pending);
            self.connection_error =
                Some("The daemon is not connected; the approval was not sent.".into());
        }
        cx.notify();
    }

    /// Take the composer draft (for "send to subthread").
    pub fn take_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
        let draft = self.composer.take_prompt(window, cx);
        cx.notify();
        draft
    }

    /// Dot shown next to the selected thread in the sidebar.
    pub fn thread_status(&self) -> ThreadStatus {
        if self.has_open_approval() {
            ThreadStatus::NeedsAttention
        } else if self.is_running() {
            ThreadStatus::Running
        } else if self.last_failed {
            ThreadStatus::Error
        } else {
            ThreadStatus::Idle
        }
    }

    /// Short label and color for the composer status pill.
    pub(super) fn status_pill(&self, p: &Palette) -> (&'static str, Hsla) {
        if self.has_open_approval() {
            return ("Waiting for approval", p.warning);
        }
        match self.lifecycle {
            TurnLifecycle::Running => ("Running", p.accent),
            TurnLifecycle::Stopping => ("Stopping", p.warning),
            TurnLifecycle::AwaitingAssistant => ("Finishing", p.accent),
            TurnLifecycle::Idle if self.last_failed => ("Failed", p.danger),
            TurnLifecycle::Idle if !self.connected => ("Offline", p.text_faint),
            TurnLifecycle::Idle => ("Ready", p.success),
        }
    }

    pub fn set_connected(&mut self, connected: bool, cx: &mut Context<Self>) {
        let was_connected = self.connected;
        self.connected = connected;
        if connected && !was_connected && !self.is_running() {
            // Re-subscribe replay (stored tool activity included) will
            // refill; drop anything streamed while offline.
            self.clear_transcript();
        }
        if !connected && was_connected {
            self.connection_error =
                Some("Daemon disconnected; waiting to reconcile this turn.".into());
        }
        if connected {
            self.connection_error = None;
        }
        if connected && !was_connected {
            self.popup.update(cx, |popup, _| popup.reconnected());
            self.model_picker
                .update(cx, |picker, _| picker.reconnected());
            self.terminals
                .update(cx, |terminals, cx| terminals.reconnected(cx));
        }
        self.model_picker
            .update(cx, |picker, _| picker.enabled = connected);
        cx.notify();
    }

    pub fn set_thread(&mut self, thread: Option<ThreadId>, cx: &mut Context<Self>) {
        if self.selected_thread == thread {
            return;
        }
        if let Some(current_thread) = &self.selected_thread {
            let mut draft = self.composer.value(cx);
            if let Some(prompt) = self.pending_prompt.take()
                && !prompt.is_empty()
            {
                draft = if draft.is_empty() {
                    prompt
                } else {
                    format!("{prompt}\n{draft}")
                };
            }
            if !draft.is_empty() {
                self.drafts.insert(current_thread.clone(), draft);
            }
        }
        self.selected_thread = thread;
        self.restore_thread_draft = self.selected_thread.is_some();
        let (thread, connected) = (self.selected_thread.clone(), self.connected);
        self.popup
            .update(cx, |popup, cx| popup.set_thread(thread.clone(), cx));
        // The side pane only talks to its subagent: no terminals, and the
        // model is the parent's choice.
        if !self.pane_mode {
            self.model_picker
                .update(cx, |picker, cx| picker.set_thread(thread.clone(), cx));
            self.terminals.update(cx, |terminals, cx| {
                terminals.set_thread(thread, connected, cx)
            });
        }
        self.open_snapshots.clear();
        self.clear_transcript();
        self.draft = None;
        self.thread_open.clear();
        self.open_blocks.clear();
        self.connection_error = None;
        cx.notify();
    }

    pub(super) fn clear_transcript(&mut self) {
        self.entries.clear();
        self.message_views.clear();
        self.stored_len.clear();
        self.selection = None;
        self.selection_point = None;
        self.reveal = None;
        self.flash = None;
        self.seen_message_ids.clear();
        self.lifecycle = TurnLifecycle::Idle;
        self.final_assistant_persisted = false;
        self.pending_prompt = None;
        self.status = AgentStatus::Idle;
        self.usage = None;
        self.cost_usd_micros = None;
        self.turns = None;
        self.model = None;
        self.session_info = None;
        self.last_failed = false;
        self._event_task = None;
        self.tool_views.clear();
        self.rail = RailState::default();
        self.prompt_times.clear();
        self.remote_authors.clear();
        self.fork_index.clear();
        self.received.clear();
        self.block_texts.clear();
        self.patch_pick = None;
        self.patch_dragging = false;
        self.turn_comments_open.clear();
    }

    pub(super) fn stop(&mut self, cx: &mut Context<Self>) {
        let (lifecycle, message) = stop_transition(self.lifecycle, self.selected_thread.as_ref());
        if self.lifecycle == lifecycle {
            return;
        }
        if let Some(message) = message {
            self.client.send(message);
        }
        self.lifecycle = lifecycle;
        cx.notify();
    }

    pub fn restore_prompt(
        &mut self,
        thread_id: &ThreadId,
        prompt: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.thread_matches(thread_id) {
            return;
        }
        self.stage_prompt(thread_id, prompt);
        let restored = self.drafts.remove(thread_id).unwrap_or_default();
        self.composer.set_prompt(&restored, window, cx);
        self.restore_thread_draft = false;
        if self.pending_prompt.as_deref() == Some(prompt) {
            self.pending_prompt = None;
            if let Some(ChatEntry::User {
                text,
                message_id: None,
            }) = self.entries.last()
                && text == prompt
            {
                self.entries.pop();
            }
            self.lifecycle = TurnLifecycle::Idle;
            self.final_assistant_persisted = false;
            self.status = AgentStatus::Idle;
        }
        self.connection_error = Some("Message was not sent; the draft was restored.".into());
        cx.notify();
    }

    pub(super) fn stage_prompt(&mut self, thread_id: &ThreadId, prompt: &str) {
        let current = self.drafts.remove(thread_id).unwrap_or_default();
        let restored = if current.trim().is_empty() {
            prompt.to_string()
        } else if current == prompt {
            current
        } else {
            format!("{prompt}\n{current}")
        };
        self.drafts.insert(thread_id.clone(), restored);
        self.restore_thread_draft = true;
    }

    pub fn handle_daemon(&mut self, message: DaemonMessage, cx: &mut Context<Self>) {
        if self.popup.update(cx, |popup, cx| popup.apply(&message, cx))
            || !self.pane_mode
                && (self
                    .model_picker
                    .update(cx, |picker, cx| picker.apply(&message, cx))
                    || self
                        .terminals
                        .update(cx, |terminals, cx| terminals.apply(&message, cx)))
        {
            return;
        }
        if let DaemonMessage::AgentEventEnvelope {
            thread_id,
            event: AgentEvent::CommandsReported { .. },
        } = &message
            && self.thread_matches(thread_id)
        {
            self.popup.update(cx, |popup, _| popup.refresh_skills());
        }
        match message {
            DaemonMessage::MessageAppended { thread_id, message } => {
                if !self.thread_matches(&thread_id) {
                    return;
                }
                if !self.seen_message_ids.insert(message.id.clone()) {
                    return;
                }
                self.fork_index.note(&message, &thread_id);
                if let Some(row) = crate::subagents::annotation_row(&message) {
                    if crate::subagents::is_standalone(&message) {
                        // Shown even mid-turn: a subagent starts while its
                        // parent's turn is still running.
                        self.entries.push(ChatEntry::Annotation(row));
                        self.scroll.scroll_to_bottom();
                        cx.notify();
                        return;
                    }
                    self.received.insert(message.id.clone(), row);
                }
                if let crate::collab::Speaker::Remote(author) =
                    crate::collab::Speaker::of(&message.actor)
                {
                    self.append_remote_message(message, author);
                    self.scroll.scroll_to_bottom();
                    cx.notify();
                    return;
                }
                let is_assistant = message.role == Role::Assistant;
                self.append_stored_message(message);
                if is_assistant && self.lifecycle != TurnLifecycle::Idle {
                    self.final_assistant_persisted = true;
                }
                if is_assistant && self.lifecycle == TurnLifecycle::AwaitingAssistant {
                    self.lifecycle = stored_assistant_transition(self.lifecycle);
                    self.status = AgentStatus::Idle;
                }
                self.scroll.scroll_to_bottom();
                cx.notify();
            }
            DaemonMessage::AgentEventEnvelope { thread_id, event } => {
                if !self.thread_matches(&thread_id) {
                    return;
                }
                if let Some(prompt) = take_rejected_prompt(&event, &mut self.pending_prompt) {
                    self.stage_prompt(&thread_id, &prompt);
                    if matches!(
                        self.entries.last(),
                        Some(ChatEntry::User { text, message_id: None }) if text == &prompt
                    ) {
                        self.entries.pop();
                    }
                }
                self.apply_event(event);
                self.scroll.scroll_to_bottom();
                cx.notify();
            }
            DaemonMessage::TurnActivity {
                thread_id,
                message_id,
                items,
            } => {
                if !self.thread_matches(&thread_id) {
                    return;
                }
                self.apply_turn_activity(message_id, &items);
                cx.notify();
            }
            DaemonMessage::UsageSnapshot { thread_id, totals } => {
                if !self.thread_matches(&thread_id) {
                    return;
                }
                self.model = Some(totals.model.clone());
                self.usage = Some(totals);
                cx.notify();
            }
            DaemonMessage::Error { message, .. } => {
                // Errors are not correlated to a thread or turn. Show the
                // message, but don't let a stale error terminate this turn.
                let (lifecycle, banner) = unscoped_error_transition(self.lifecycle, message);
                self.lifecycle = lifecycle;
                self.connection_error = Some(banner);
                cx.notify();
            }
            _ => {}
        }
    }

    pub(super) fn thread_matches(&self, thread_id: &ThreadId) -> bool {
        self.selected_thread.as_ref() == Some(thread_id)
    }

    pub(super) fn uses_daemon(&self) -> bool {
        self.connected && self.selected_thread.is_some()
    }

    pub(super) fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !can_submit(self.lifecycle) {
            return;
        }
        if self.connected && self.selected_thread.is_none() {
            return;
        }
        if self.run_bang_command(window, cx) {
            return;
        }
        let Some((prompt, mentions)) = self.composer.take_content(window, cx) else {
            return;
        };

        if let (true, Some(thread_id)) = (self.connected, self.selected_thread.clone()) {
            let terminals: Vec<String> = self
                .terminals
                .read(cx)
                .attachments()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            if self.client.send(ClientMessage::SendPrompt {
                thread_id: thread_id.clone(),
                text: prompt.clone(),
                mentions,
                terminals,
            }) == SendStatus::Rejected
            {
                self.restore_prompt(&thread_id, &prompt, window, cx);
                return;
            }
            self.terminals
                .update(cx, |terminals, cx| terminals.clear_attachments(cx));
            self.pending_prompt = Some(prompt.clone());
            self._event_task = None;
        } else {
            self.pending_prompt = None;
            let rx = self.agent.send(prompt.clone());
            self._event_task = Some(cx.spawn(async move |this, cx| {
                while let Ok(event) = rx.recv().await {
                    let alive = this.update(cx, |this, cx| {
                        this.apply_event(event);
                        this.scroll.scroll_to_bottom();
                        cx.notify();
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            }));
        }
        self.entries.push(ChatEntry::User {
            text: prompt,
            message_id: None,
        });
        self.last_failed = false;
        self.scroll.scroll_to_bottom();
        self.connection_error = None;
        self.lifecycle = TurnLifecycle::Running;
        self.final_assistant_persisted = false;
        self.status = AgentStatus::Running;
        self.composer.set_disabled(true, cx);
        cx.notify();
    }

    pub(super) fn append_stored_message(&mut self, message: Message) {
        let at = message.created_at.timestamp_millis();
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    self.tool_views
                        .timings
                        .entry(id.clone())
                        .or_insert(ToolTiming {
                            started_ms: at,
                            finished_ms: None,
                        });
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    if let Some(timing) = self.tool_views.timings.get_mut(tool_use_id) {
                        timing.finished_ms.get_or_insert(at);
                    }
                }
                _ => {}
            }
        }
        if message.role == Role::User {
            self.prompt_times
                .insert(message.id.clone(), message.created_at);
        }
        match message.role {
            Role::User => {
                append_stored_user(&mut self.entries, &mut self.pending_prompt, &message);
            }
            Role::Assistant => {
                append_stored_assistant(&mut self.entries, self.lifecycle, &message);
                let text_len = message.text().len();
                let persisted = self.entries.iter().rev().any(|entry| {
                    matches!(
                        entry,
                        ChatEntry::Assistant { message_id: Some(id), markdown, .. }
                            if id == &message.id && markdown.len() == text_len
                    )
                });
                if persisted && text_len > 0 {
                    self.stored_len.insert(message.id.clone(), text_len);
                }
            }
            Role::Tool | Role::System => {
                if !self.is_running() {
                    self.expand_message_blocks(&message);
                }
            }
        }
    }

    /// Another participant's prompt, their Claude's answer, or a notice
    /// (comment, verdict, change summary). Kept apart from this view's own
    /// turn lifecycle: a remote message never ends or adopts a local turn.
    pub(super) fn append_remote_message(&mut self, message: Message, author: String) {
        let text = message.text();
        if text.trim().is_empty() {
            return;
        }
        self.remote_authors
            .insert(message.id.clone(), (author, message.role));
        match message.role {
            Role::User => {
                self.prompt_times
                    .insert(message.id.clone(), message.created_at);
                self.entries.push(ChatEntry::User {
                    text,
                    message_id: Some(message.id),
                });
            }
            _ => {
                if message.role == Role::Assistant {
                    self.stored_len.insert(message.id.clone(), text.len());
                }
                self.entries.push(ChatEntry::Assistant {
                    markdown: text,
                    streaming: false,
                    message_id: Some(message.id),
                });
            }
        }
    }

    pub(super) fn remote_author(&self, id: &MessageId) -> Option<&str> {
        self.remote_authors.get(id).map(|(name, _)| name.as_str())
    }

    pub(super) fn expand_message_blocks(&mut self, message: &Message) {
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    if text.is_empty() {
                        continue;
                    }
                    match self.entries.last_mut() {
                        Some(ChatEntry::Assistant {
                            markdown,
                            streaming,
                            ..
                        }) => {
                            markdown.push_str(text);
                            *streaming = false;
                        }
                        _ => self.entries.push(ChatEntry::Assistant {
                            markdown: text.clone(),
                            streaming: false,
                            message_id: None,
                        }),
                    }
                }
                ContentBlock::ToolUse { id, name, input } => {
                    if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
                        *streaming = false;
                    }
                    self.entries.push(ChatEntry::ToolCall {
                        tool_use_id: id.clone(),
                        name: name.clone(),
                        input_json: input.clone(),
                        finished: false,
                        is_error: false,
                        expanded: false,
                        output: None,
                    });
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    content,
                } => {
                    self.finish_tool(tool_use_id, *is_error, Some(content.clone()));
                }
                ContentBlock::Thinking { text } => push_thinking(&mut self.entries, text),
            }
        }
    }

    pub(super) fn finish_tool(
        &mut self,
        tool_use_id: &str,
        is_error: bool,
        result: Option<serde_json::Value>,
    ) {
        if let Some(call) = self.entries.iter_mut().rev().find_map(|entry| match entry {
            ChatEntry::ToolCall {
                tool_use_id: id, ..
            } if id == tool_use_id => Some(entry),
            _ => None,
        }) {
            if let ChatEntry::ToolCall {
                finished,
                is_error: err,
                output,
                ..
            } = call
            {
                *finished = true;
                *err = is_error;
                if result.is_some() {
                    *output = result;
                }
            }
        }
    }

    pub(super) fn apply_event(&mut self, event: AgentEvent) {
        // Session metadata and approvals are keyed, not turn-ordered: the
        // daemon replays them on subscribe while this view is idle.
        match event {
            AgentEvent::SessionInfo {
                billing,
                api_key_source,
                agent_version,
                permission_mode,
            } => {
                self.session_info = Some(SessionInfo {
                    billing,
                    api_key_source,
                    agent_version,
                    permission_mode,
                });
                return;
            }
            AgentEvent::ApprovalRequested {
                request_id,
                tool_name,
                input_json,
            } => {
                self.adopt_remote_turn();
                push_approval(&mut self.entries, request_id, tool_name, input_json);
                return;
            }
            AgentEvent::ApprovalResolved {
                request_id,
                decision,
                reason,
            } => {
                set_approval_state(
                    &mut self.entries,
                    &request_id,
                    ApprovalState::Resolved { decision, reason },
                );
                return;
            }
            _ => {}
        }
        if starts_remote_turn(self.lifecycle, &event) {
            self.adopt_remote_turn();
        }
        if !accepts_agent_events(self.lifecycle) {
            return;
        }
        match event {
            AgentEvent::SessionStarted { model, .. } => {
                if model.is_some() {
                    self.model = model;
                }
            }
            AgentEvent::TextDelta { text } => match self.entries.last_mut() {
                Some(ChatEntry::Assistant {
                    markdown,
                    streaming: true,
                    ..
                }) => markdown.push_str(&text),
                _ => self.entries.push(ChatEntry::Assistant {
                    markdown: text,
                    streaming: true,
                    message_id: None,
                }),
            },
            AgentEvent::ToolCallStarted {
                tool_use_id,
                name,
                input_json,
            } => {
                if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
                    *streaming = false;
                }
                self.tool_views.timings.insert(
                    tool_use_id.clone(),
                    ToolTiming {
                        started_ms: tools::now_ms(),
                        finished_ms: None,
                    },
                );
                self.entries.push(ChatEntry::ToolCall {
                    tool_use_id,
                    name,
                    input_json,
                    finished: false,
                    is_error: false,
                    expanded: false,
                    output: None,
                });
            }
            AgentEvent::ToolCallFinished {
                tool_use_id,
                output_json,
                is_error,
            } => {
                if let Some(timing) = self.tool_views.timings.get_mut(&tool_use_id) {
                    timing.finished_ms.get_or_insert(tools::now_ms());
                }
                self.finish_tool(&tool_use_id, is_error, Some(output_json));
            }
            AgentEvent::ReasoningDelta { text } => push_thinking(&mut self.entries, &text),
            AgentEvent::UsageReported { report } => {
                self.model = Some(report.model.clone());
                self.usage = Some(report);
            }
            AgentEvent::TurnCompleted {
                num_turns,
                cost_usd_micros,
            } => {
                if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
                    *streaming = false;
                }
                self.turns = Some(num_turns);
                self.cost_usd_micros = cost_usd_micros;
                self.finish_turn();
            }
            AgentEvent::Failed { message } => {
                self.entries.push(ChatEntry::Assistant {
                    markdown: format!("**Failed:** {message}"),
                    streaming: false,
                    message_id: None,
                });
                self.final_assistant_persisted = true;
                self.last_failed = true;
                self.finish_turn();
            }
            AgentEvent::ApprovalRequested { .. }
            | AgentEvent::SessionInfo { .. }
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentFinished { .. }
            | AgentEvent::CommandsReported { .. }
            | AgentEvent::TranscriptEntry { .. } => {}
        }
    }

    /// A turn this view did not start (a subthread's initial prompt, a
    /// review or land run, another client) is streaming: follow it.
    pub(super) fn adopt_remote_turn(&mut self) {
        if self.lifecycle == TurnLifecycle::Idle && self.uses_daemon() {
            self.lifecycle = TurnLifecycle::Running;
            self.final_assistant_persisted = false;
            self.status = AgentStatus::Running;
            self.last_failed = false;
        }
    }

    pub(super) fn finish_turn(&mut self) {
        expire_open_approvals(&mut self.entries);
        self.completed_turns += 1;
        self.lifecycle = terminal_transition(self.uses_daemon(), self.final_assistant_persisted);
        let waiting_for_assistant = self.lifecycle == TurnLifecycle::AwaitingAssistant;
        self.status = if waiting_for_assistant {
            AgentStatus::Running
        } else {
            AgentStatus::Idle
        };
    }

    /// Expand the n-th tool call (dev hook for screenshots).
    pub fn expand_tool_call(&mut self, nth: usize, cx: &mut Context<Self>) {
        let index = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches!(entry, ChatEntry::ToolCall { .. }))
            .nth(nth)
            .map(|(index, _)| index);
        if let Some(index) = index {
            self.toggle_tool(index, cx);
        }
    }

    pub(crate) fn toggle_tool(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(ChatEntry::ToolCall { expanded, .. }) = self.entries.get_mut(index) {
            *expanded = !*expanded;
            cx.notify();
        }
    }

    pub fn toggle_group(&mut self, start: usize, cx: &mut Context<Self>) {
        if !self.tool_views.open_groups.remove(&start) {
            self.tool_views.open_groups.insert(start);
        }
        cx.notify();
    }

    /// Flip an edit row; `open` is what it showed (possibly by default).
    pub fn toggle_edit(&mut self, tool_use_id: &str, open: bool, cx: &mut Context<Self>) {
        self.tool_views
            .edit_open
            .insert(tool_use_id.to_string(), !open);
        cx.notify();
    }

    pub fn toggle_output(&mut self, tool_use_id: &str, cx: &mut Context<Self>) {
        if !self.tool_views.full_output.remove(tool_use_id) {
            self.tool_views.full_output.insert(tool_use_id.to_string());
        }
        cx.notify();
    }

    pub fn open_thread(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        cx.emit(ChatEvent::OpenThread(thread));
    }

    /// Threads the sidebar lists (id, title), for subagent links.
    pub fn set_known_threads(&mut self, threads: Vec<(ThreadId, String)>, cx: &mut Context<Self>) {
        if self.known_threads != threads {
            self.known_threads = threads;
            cx.notify();
        }
    }

    /// Each thread's model override (`ThreadSnapshot.model`).
    pub fn set_thread_models(
        &mut self,
        models: Vec<(ThreadId, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        self.model_picker
            .update(cx, |picker, cx| picker.set_overrides(models, cx));
    }

    /// ⌃` and the composer's terminal button.
    pub fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.uses_daemon() || self.pane_mode {
            return;
        }
        self.terminals
            .update(cx, |terminals, cx| terminals.toggle(window, cx));
        if !self.terminals.read(cx).is_open() {
            self.focus_composer(window, cx);
        }
    }

    /// `!command` in the composer: run it in a new terminal instead of
    /// sending a prompt. `true` when it did.
    pub(super) fn run_bang_command(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.uses_daemon() || self.pane_mode {
            return false;
        }
        let Some(command) = self.popup.read(cx).bang_command() else {
            return false;
        };
        self.composer.set_prompt("", window, cx);
        let command = (!command.is_empty()).then_some(command);
        self.terminals
            .update(cx, |terminals, cx| terminals.open_terminal(command, cx));
        true
    }

    /// Keys the composer popups take before the textarea sees them.
    pub(super) fn composer_key_handlers(&self, element: Div, cx: &mut Context<Self>) -> Div {
        element
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                if this.popup.read(cx).is_open() {
                    this.popup
                        .update(cx, |popup, cx| popup.move_selection(-1, cx));
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if this.popup.read(cx).is_open() {
                    this.popup
                        .update(cx, |popup, cx| popup.move_selection(1, cx));
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if this
                    .popup
                    .update(cx, |popup, cx| popup.is_open() && popup.accept(window, cx))
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, action: &Enter, window, cx| {
                if action.shift {
                    return;
                }
                if this
                    .popup
                    .update(cx, |popup, cx| popup.is_open() && popup.accept(window, cx))
                    || this.run_bang_command(window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                if this.popup.update(cx, |popup, cx| popup.dismiss(cx)) {
                    cx.stop_propagation();
                }
            }))
    }

    /// The composer's terminal button (footer) and attached-terminal chips
    /// (above the text).
    pub(super) fn composer_extras(
        &self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> (Vec<AnyElement>, Vec<AnyElement>) {
        let terminals = self.terminals.read(cx);
        let running = terminals.running_count();
        let open = terminals.is_open();
        let attachments = terminals.attachments();
        let mut tools = Vec::new();
        if self.uses_daemon() && !self.pane_mode {
            tools.push(
                h_flex()
                    .id("composer-terminal")
                    .h(px(22.))
                    .px(theme::sp(1.5))
                    .gap(theme::sp(1.))
                    .items_center()
                    .rounded(theme::radius())
                    .cursor_pointer()
                    .text_size(theme::text_xs())
                    .text_color(if open { p.accent_text } else { p.text_muted })
                    .when(open, |this| this.bg(p.accent_soft))
                    .hover(|style| style.bg(p.hover))
                    .tooltip(|window, cx| {
                        Tooltip::new("Terminal \u{2014} or start a message with ! (\u{2303}`)")
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_terminal(window, cx)))
                    .child(Icon::new(IconName::SquareTerminal).with_size(px(13.)))
                    .when(running > 0 && !open, |this| {
                        this.child(format!("{running} running"))
                    })
                    .into_any_element(),
            );
        }
        let header = attachments
            .into_iter()
            .enumerate()
            .map(|(ix, (id, label))| {
                let terminals = self.terminals.clone();
                terminal_view::attachment_chip(
                    ("attached-terminal", ix),
                    &label,
                    p,
                    move |_, _, cx| {
                        terminals.update(cx, |terminals, cx| terminals.detach(&id, cx));
                    },
                )
            })
            .collect();
        (tools, header)
    }

    pub fn toggle_snapshot(&mut self, key: (MessageId, usize), cx: &mut Context<Self>) {
        if !self.open_snapshots.remove(&key) {
            self.open_snapshots.insert(key);
        }
        cx.notify();
    }

    /// Titles and subagent records of the listed threads, for annotation
    /// rows and links from `spawn_subagent` calls to their child.
    pub fn set_thread_index(
        &mut self,
        index: crate::subagents::ThreadIndex,
        cx: &mut Context<Self>,
    ) {
        if self.thread_index != index {
            self.thread_index = index;
            cx.notify();
        }
    }

    /// A stored turn's (or the running turn's) replayed tool calls and
    /// thinking. They arrive right before the message that ends the turn;
    /// the running turn's come after the history, and its later steps
    /// stream as agent events.
    pub(super) fn apply_turn_activity(
        &mut self,
        message_id: Option<MessageId>,
        items: &[trama_protocol::ActivityItem],
    ) {
        if message_id
            .as_ref()
            .is_some_and(|id| self.seen_message_ids.contains(id))
        {
            return;
        }
        let in_flight = message_id.is_none();
        // Already following this turn (a reconnect mid-turn): only add the
        // tool calls it missed; its thinking is on screen already.
        let following = in_flight && self.is_running();
        if in_flight {
            // Subscribed mid-turn: follow it like a turn started elsewhere.
            self.adopt_remote_turn();
        }
        let shown = crate::activity::shown_tool_ids(&self.entries);
        let mut replayed = crate::activity::replay_entries(items, in_flight, &shown);
        if following {
            replayed
                .entries
                .retain(|entry| matches!(entry, ChatEntry::ToolCall { .. }));
        }
        for (id, timing) in replayed.timings {
            self.tool_views.timings.entry(id).or_insert(timing);
        }
        if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
            *streaming = false;
        }
        self.entries.extend(replayed.entries);
    }

    /// Ask the workspace to fork at `message_id` (the hover action).
    pub fn request_fork(&mut self, message_id: MessageId, cx: &mut Context<Self>) {
        let Some(thread_id) = self.selected_thread.clone() else {
            return;
        };
        if self.fork_index.state(&message_id) != crate::forks::ForkState::Ready {
            return;
        }
        let label = self
            .entries
            .iter()
            .find_map(|entry| match entry {
                ChatEntry::User {
                    text,
                    message_id: Some(id),
                } if id == &message_id => Some(comments::prompt_body(text).to_string()),
                ChatEntry::Assistant {
                    markdown,
                    message_id: Some(id),
                    ..
                } if id == &message_id => Some(markdown.clone()),
                _ => None,
            })
            .map(|text| crate::forks::preview(&text))
            .unwrap_or_default();
        cx.emit(ChatEvent::Fork(crate::forks::ForkTarget {
            thread_id,
            message_id,
            label,
        }));
    }

    /// "Fork from here" (⌘K): the turn under the rail's pointer, else the
    /// turn at the top of the view, when it can be forked.
    pub fn fork_here_target(&self) -> Option<MessageId> {
        if !self.uses_daemon() {
            return None;
        }
        let turn_list = turns::segment_turns(&self.entries);
        let turn = self.rail.hovered.or_else(|| {
            let geometry = self.rail_geometry()?;
            geometry
                .spans
                .iter()
                .rposition(|span| span.top <= geometry.view.top + 1.)
                .or(Some(0))
        })?;
        let range = turn_list.get(turn)?.entries.clone();
        let message = crate::forks::turn_fork_message(&self.entries[range])?;
        (self.fork_index.state(&message) == crate::forks::ForkState::Ready).then_some(message)
    }
}

/// Append streamed or stored thinking, merging with thinking right before.
pub(super) fn push_thinking(entries: &mut Vec<ChatEntry>, text: &str) {
    if text.is_empty() {
        return;
    }
    match entries.last_mut() {
        Some(ChatEntry::Thinking { text: existing }) => existing.push_str(text),
        last => {
            if let Some(ChatEntry::Assistant { streaming, .. }) = last {
                *streaming = false;
            }
            entries.push(ChatEntry::Thinking {
                text: text.to_string(),
            });
        }
    }
}

// -- comments -------------------------------------------------------------
