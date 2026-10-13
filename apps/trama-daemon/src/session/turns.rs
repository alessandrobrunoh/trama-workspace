use super::*;

impl SessionManager {
    /// Persist the user message, then spawn the adapter turn. The thread's
    /// pending comments go out with it, as a block before `text`; the stored
    /// message is exactly the prompt sent. A second prompt on a busy or
    /// migrating thread returns [`DaemonError::Busy`].
    pub async fn send_prompt(&self, thread_id: ThreadId, text: String) -> Result<Message> {
        self.start_turn(thread_id, text, false, PromptContext::default())
            .await
    }

    /// [`Self::send_prompt`] with `@` mentions and terminal snapshots,
    /// expanded into a context block (see [`context`]).
    pub async fn send_prompt_with_context(
        &self,
        thread_id: ThreadId,
        text: String,
        context: PromptContext,
    ) -> Result<Message> {
        self.start_turn(thread_id, text, false, context).await
    }

    /// Deliver the pending comments as a turn of their own. Fails with
    /// [`DaemonError::Invalid`] when nothing is pending.
    pub async fn send_comments(&self, thread_id: ThreadId) -> Result<Message> {
        self.start_turn(thread_id, String::new(), true, PromptContext::default())
            .await
    }

    pub(super) async fn start_turn(
        &self,
        thread_id: ThreadId,
        text: String,
        comments_only: bool,
        attached: PromptContext,
    ) -> Result<Message> {
        self.start_turn_with(thread_id, text, comments_only, attached, None, None)
            .await
    }

    /// [`Self::send_prompt`] with `preamble` placed before the prompt for the
    /// agent only: the stored message is the participant's text. Used for
    /// the digest of a shared thread's other participants (ADR 0013), which
    /// is derived from the transcript the participant already sees.
    pub(crate) async fn send_prompt_with_preamble(
        &self,
        thread_id: ThreadId,
        text: String,
        preamble: Option<String>,
    ) -> Result<Message> {
        self.start_turn_with(
            thread_id,
            text,
            false,
            PromptContext::default(),
            None,
            preamble,
        )
        .await
    }

    /// [`Self::start_turn`]; `origin` attributes the stored prompt to another
    /// thread's agent instead of the local user.
    pub(super) async fn start_turn_with(
        &self,
        thread_id: ThreadId,
        text: String,
        comments_only: bool,
        attached: PromptContext,
        origin: Option<agents::PromptOrigin>,
        preamble: Option<String>,
    ) -> Result<Message> {
        if self.db.get_thread(&thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        {
            let mut inner = self.lock();
            let thread = inner.threads.entry(thread_id.clone()).or_default();
            if thread.reservation.is_some() {
                return Err(DaemonError::Busy);
            }
            thread.reservation = Some(Reservation::Turn);
            thread.interrupt_requested = false;
            thread.acc_text.clear();
            thread.comments_delivered_with = None;
            thread.last_claude_uuid = None;
            thread.activity = Default::default();
        }
        // Reject a prompt that cannot run before it is stored, so a failed send
        // is not replayed or started later without another user action. The
        // turn reservation is held meanwhile, so a migration cannot retarget
        // the thread's checkouts under this prompt.
        if let Err(err) = self.session_config(&thread_id).await {
            self.release_turn(&thread_id);
            return Err(err);
        }

        let attached = match self.prompt_context(&thread_id, &attached).await {
            Ok(block) => block,
            Err(err) => {
                self.release_turn(&thread_id);
                return Err(err);
            }
        };
        let text = context::compose_with_context(&text, &attached);
        // A slash command must reach Claude first and unchanged; pending
        // comments wait for the next prompt.
        let delivery = if !comments_only && context::is_slash_command(&text) {
            None
        } else {
            match self.pending_delivery(&thread_id).await {
                Ok(delivery) => delivery,
                Err(err) => {
                    self.release_turn(&thread_id);
                    return Err(err);
                }
            }
        };
        if comments_only && delivery.is_none() {
            self.release_turn(&thread_id);
            return Err(DaemonError::Invalid(
                "there are no pending comments to send".into(),
            ));
        }
        let prompt = comments::compose_prompt(
            delivery.as_ref().map(|delivery| delivery.block.as_str()),
            &text,
        );
        let mut message =
            Message::new_user(thread_id.clone(), comments::LOCAL_USER, prompt.clone());
        if let Some(origin) = origin {
            message.actor = origin.actor;
            message.annotation = Some(origin.annotation);
        }
        let delivered_ids = match &delivery {
            None => {
                if let Err(err) = self.db.append_message(&message).await {
                    self.release_turn(&thread_id);
                    return Err(err.into());
                }
                if let Err(err) = self
                    .append_event(
                        Aggregate::Thread {
                            id: thread_id.clone(),
                        },
                        message.actor.clone(),
                        DomainEvent::MessageAdded {
                            message: message.clone(),
                        },
                    )
                    .await
                {
                    crate::log::warn(format!("failed to append MessageAdded: {err}"));
                }
                Vec::new()
            }
            Some(delivery) => {
                if let Err(err) = self.persist_delivery(&message, &delivery.ids).await {
                    self.release_turn(&thread_id);
                    return Err(err);
                }
                let mut inner = self.lock();
                if let Some(thread) = inner.threads.get_mut(&thread_id) {
                    thread.comments_delivered_with = Some(message.id.clone());
                }
                delivery.ids.clone()
            }
        };

        self.broadcast(
            &thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message: message.clone(),
            },
        );
        self.broadcast_comment_updates(&thread_id, &delivered_ids)
            .await;

        let manager = self.clone();
        let prompt = match preamble.filter(|p| !p.trim().is_empty()) {
            Some(preamble) if !context::is_slash_command(&prompt) => {
                format!("{preamble}\n\n{prompt}")
            }
            _ => prompt,
        };
        tokio::spawn(async move {
            if let Err(failure) = manager.run_turn(thread_id.clone(), prompt).await {
                manager.handle_turn_failure(&thread_id, failure).await;
            }
        });
        Ok(message)
    }

    /// Store the prompt carrying comments and mark them delivered, atomically.
    pub(super) async fn persist_delivery(
        &self,
        message: &Message,
        ids: &[CommentId],
    ) -> Result<()> {
        let thread_id = &message.thread_id;
        let turn = self.db.count_user_prompts(thread_id).await? + 1;
        let aggregate = Aggregate::Thread {
            id: thread_id.clone(),
        };
        let events = vec![
            EventRecord::new(
                aggregate.clone(),
                message.actor.clone(),
                DomainEvent::MessageAdded {
                    message: message.clone(),
                },
                vec![],
            )?,
            EventRecord::new(
                aggregate,
                message.actor.clone(),
                DomainEvent::CommentsDelivered {
                    thread_id: thread_id.clone(),
                    comment_ids: ids.to_vec(),
                    message_id: message.id.clone(),
                    turn,
                },
                vec![],
            )?,
        ];
        self.db
            .deliver_comments(message, ids, turn, &events)
            .await?;
        Ok(())
    }

    pub async fn interrupt(&self, thread_id: &ThreadId) -> Result<bool> {
        if self.db.get_thread(thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        let session = {
            let mut inner = self.lock();
            let thread = inner.threads.entry(thread_id.clone()).or_default();
            thread.interrupt_requested = true;
            thread.live_session.clone()
        };
        // Release a hook blocked on a person first; `interrupt` tells Claude
        // Code to stop rather than try another approach.
        self.cancel_thread_approvals(thread_id, "The user interrupted the turn in Trama.", true)
            .await;
        match session {
            Some(session) => {
                self.adapter.interrupt(&session).await?;
                Ok(true)
            }
            None => Ok(true),
        }
    }

    pub(super) fn broadcast(&self, thread_id: &ThreadId, message: DaemonMessage) {
        self.observe(thread_id, &message);
        let mut inner = self.lock();
        let Some(thread) = inner.threads.get_mut(thread_id) else {
            return;
        };
        thread
            .subscribers
            .retain(|sub| sub.tx.send(message.clone()).is_ok());
    }

    pub(super) async fn append_event(
        &self,
        aggregate: Aggregate,
        actor: Actor,
        event: DomainEvent,
    ) -> Result<()> {
        let record = EventRecord::new(aggregate, actor, event, vec![])?;
        EventStore::append(&self.db, vec![record]).await?;
        Ok(())
    }

    /// Persist one system message (row plus `MessageAdded` event) and
    /// broadcast it, so the outcome is visible after a restart. A store
    /// failure is logged; it must not lose the in-memory turn state cleanup.
    pub(crate) async fn append_system_notice(&self, thread_id: &ThreadId, text: String) {
        self.append_system_notice_with_activity(thread_id, text, Vec::new())
            .await;
    }

    /// [`Self::append_system_notice`] for a turn that ended without an
    /// answer: `activity` (what the turn did) is stored with the notice.
    pub(super) async fn append_system_notice_with_activity(
        &self,
        thread_id: &ThreadId,
        text: String,
        activity: Vec<trama_domain::activity::ActivityItem>,
    ) {
        let message = Message {
            id: MessageId::generate(),
            thread_id: thread_id.clone(),
            parent_id: None,
            actor: Actor::System,
            role: Role::System,
            content: vec![ContentBlock::Text { text }],
            created_at: Utc::now(),
            claude_uuid: None,
            annotation: None,
        };
        if let Err(err) = self.db.append_message(&message).await {
            crate::log::warn(format!("failed to persist system message: {err}"));
            return;
        }
        if let Err(err) = self
            .append_event(
                Aggregate::Thread {
                    id: thread_id.clone(),
                },
                Actor::System,
                DomainEvent::MessageAdded {
                    message: message.clone(),
                },
            )
            .await
        {
            crate::log::warn(format!("failed to append MessageAdded: {err}"));
        }
        self.record_activity(thread_id, &message.id, Actor::System, activity)
            .await;
        self.broadcast(
            thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message,
            },
        );
    }

    /// Keep a failed turn visible after restart. The prompt is not retried.
    /// The wording distinguishes a turn that never started (for example
    /// Claude is missing) from a session that started but rejected the
    /// prompt, so a transcript never claims the agent did not run when it
    /// did.
    pub(super) async fn handle_turn_failure(&self, thread_id: &ThreadId, failure: TurnFailure) {
        let text = if failure.agent_started {
            format!(
                "The agent session started but the prompt was not accepted: {}. \
                 No output was produced for this prompt",
                failure.error
            )
        } else {
            format!("The agent did not run: {}", failure.error)
        };
        self.append_system_notice(thread_id, text).await;
        if failure.agent_started {
            // The session may have read part of the prompt; never redeliver
            // silently, but the comments are not answered either.
            self.take_turn_delivery(thread_id);
        } else {
            self.return_undelivered_comments(thread_id).await;
        }
        self.release_turn(thread_id);
        self.broadcast(
            thread_id,
            DaemonMessage::AgentEventEnvelope {
                thread_id: thread_id.clone(),
                event: AgentEvent::Failed {
                    message: failure.error.to_string(),
                },
            },
        );
        self.broadcast(
            thread_id,
            DaemonMessage::Error {
                code: failure.error.code().into(),
                message: failure.error.to_string(),
            },
        );
        self.after_turn(
            thread_id,
            agents::TurnEnd::Failed(failure.error.to_string()),
        );
    }

    pub(super) async fn run_turn(
        &self,
        thread_id: ThreadId,
        text: String,
    ) -> std::result::Result<(), TurnFailure> {
        // Best-effort "last turn" baseline. It runs beside session start and
        // the model's first round trip instead of delaying the prompt; an
        // edit landing within that window would count as pre-turn state.
        let snapshot = self.clone();
        let snapshot_thread = thread_id.clone();
        tokio::spawn(async move { snapshot.snapshot_turn_start(&snapshot_thread).await });
        let session = match self.ensure_session(&thread_id).await {
            Ok(session) => session,
            Err(err) => {
                return Err(TurnFailure {
                    error: err,
                    agent_started: false,
                });
            }
        };
        let interrupted = {
            let inner = self.lock();
            inner
                .threads
                .get(&thread_id)
                .map(|t| t.interrupt_requested)
                .unwrap_or(false)
        };
        if interrupted {
            let _ = self.adapter.interrupt(&session).await;
            self.return_undelivered_comments(&thread_id).await;
            self.release_turn(&thread_id);
            self.after_turn(
                &thread_id,
                agents::TurnEnd::Failed("interrupted before it started".into()),
            );
            return Ok(());
        }
        let session_id = session.id.clone();
        if let Err(err) = self
            .append_event(
                Aggregate::Session {
                    id: session_id.clone(),
                },
                Actor::Agent {
                    session: session_id.clone(),
                },
                DomainEvent::AgentTurnStarted {
                    thread_id: thread_id.clone(),
                    session_id,
                    agent_kind: agent_kind_label(self.adapter.kind()).into(),
                },
            )
            .await
        {
            crate::log::warn(format!("failed to append AgentTurnStarted: {err}"));
        }
        if let Err(err) = self.adapter.send(&session, AgentPrompt { text }).await {
            return Err(TurnFailure {
                error: err.into(),
                agent_started: true,
            });
        }
        Ok(())
    }

    pub(super) async fn ensure_session(&self, thread_id: &ThreadId) -> Result<AgentSession> {
        if let Some(session) = {
            let inner = self.lock();
            inner
                .threads
                .get(thread_id)
                .and_then(|t| t.live_session.clone())
        } {
            // A model change applies from the next turn: restart the
            // session, which then resumes the conversation with `--model`.
            if !self.live_model_outdated(thread_id).await? {
                return Ok(session);
            }
            self.retire_live_session(thread_id, session).await;
        }

        let mut config = self.session_config(thread_id).await?;
        let started_model = config.model.clone();
        let token = self.attach_approval_hook(thread_id, &mut config);
        self.attach_trama_tools(thread_id, &mut config).await;
        let (tx, rx) = async_channel::unbounded();
        let session = match self.adapter.start(config, tx).await {
            Ok(session) => session,
            Err(err) => {
                self.drop_approval_token(thread_id, token);
                self.drop_trama_tools(thread_id);
                return Err(DaemonError::Agent(format!(
                    "{err}. Install Claude Code or set CLAUDE_BIN to the `claude` binary."
                )));
            }
        };
        if let Err(err) = self
            .db
            .create_agent_session(thread_id, &session, agent_kind_label(self.adapter.kind()))
            .await
        {
            self.drop_approval_token(thread_id, token);
            self.drop_trama_tools(thread_id);
            return Err(err.into());
        }
        {
            let mut inner = self.lock();
            let thread = inner.threads.entry(thread_id.clone()).or_default();
            thread.live_session = Some(session.clone());
            thread.session_allowed_tools.clear();
            thread.session_info = None;
        }
        self.composer.note_live_model(thread_id, started_model);

        let manager = self.clone();
        let pump_thread = thread_id.clone();
        let pump_session = session.id.clone();
        tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                manager.on_agent_event(&pump_thread, event).await;
            }
            if manager.composer.take_retired(&pump_session) {
                return;
            }
            manager.on_pump_closed(&pump_thread).await;
        });
        Ok(session)
    }

    pub(super) async fn session_config(&self, thread_id: &ThreadId) -> Result<SessionConfig> {
        let thread = self
            .db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        if thread.repo_attachments.is_empty() {
            return Err(DaemonError::Invalid(
                "thread has no repository paths. Add a local Git repository before starting the agent"
                    .into(),
            ));
        }
        // A scout works in its parent's checkouts.
        let owner = self.checkout_owner(&thread).await?;
        // Checkouts removed to free disk come back before the turn.
        self.ensure_checkouts_present(&owner).await?;
        let mut paths = Vec::with_capacity(owner.repo_attachments.len());
        let mut validations = Vec::new();
        for attachment in &owner.repo_attachments {
            let repository = self
                .db
                .get_repository(&attachment.repository_id)
                .await?
                .ok_or_else(|| {
                    DaemonError::NotFound(format!("repository {}", attachment.repository_id))
                })?;
            let source = repository.local_path.ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "repository {} has no local path",
                    attachment.repository_id
                ))
            })?;
            match attachment.attach_mode {
                AttachMode::Isolated => {
                    // Only validated here: legacy threads must be migrated first.
                    let _checkout_id = attachment.checkout_id.as_ref().ok_or_else(|| {
                        DaemonError::Invalid(format!(
                            "thread {} predates managed checkouts. Migrate it explicitly (migrate_legacy_thread) before sending. The original files are left untouched, and this prompt was not run",
                            thread.id
                        ))
                    })?;
                    let base_revision = attachment.base_revision.as_deref().ok_or_else(|| {
                        DaemonError::Invalid("managed checkout has no recorded base commit".into())
                    })?;
                    let (path, repo_local) =
                        self.checkout_path(&owner.id, attachment, &source).await?;
                    validations.push((
                        source.clone(),
                        path.clone(),
                        base_revision.to_owned(),
                        repo_local,
                    ));
                    paths.push(path);
                }
                AttachMode::Adopted => {
                    return Err(DaemonError::Invalid(format!(
                        "thread {} has an adopted repository attachment. Migrate it explicitly (migrate_legacy_thread) to an isolated checkout before sending. The agent will not run on the original working tree",
                        thread.id
                    )));
                }
            }
        }
        let checkout_manager = self.checkout_manager.clone();
        tokio::task::spawn_blocking(move || {
            for (source, path, base_revision, repo_local) in validations {
                validate_managed_checkout(
                    &checkout_manager,
                    &source,
                    &path,
                    &base_revision,
                    repo_local,
                )?;
            }
            Ok::<_, trama_infra::checkout::CheckoutError>(())
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("checkout validation task failed: {error}")))?
        .map_err(|error| DaemonError::Invalid(error.to_string()))?;
        let cwd = paths.remove(0);
        let extra_dirs = paths;

        let workspace: Option<Workspace> = match &thread.workspace_id {
            Some(id) => self.db.get_workspace(id).await?,
            None => None,
        };
        let (permission_mode, workspace_model) = match &workspace {
            Some(ws) => (
                permission_mode_from_settings(&ws.settings),
                ws.settings
                    .default_model
                    .clone()
                    .filter(|model| !model.is_empty())
                    .unwrap_or_else(|| DEFAULT_CLAUDE_MODEL.to_string()),
            ),
            None => (
                trama_domain::agent::PermissionMode::Default,
                DEFAULT_CLAUDE_MODEL.to_string(),
            ),
        };
        // The thread's own choice (or its parent's) wins over the workspace,
        // which falls back to DEFAULT_CLAUDE_MODEL.
        let model = Some(
            self.thread_model_override(&thread)
                .await?
                .unwrap_or(workspace_model),
        );

        let own_session = self
            .db
            .latest_agent_session_for_thread(thread_id)
            .await?
            .filter(|session| same_directory(&session.cwd, &cwd))
            .and_then(|session| session.external_session_id);
        // A subthread's first session inherits its parent's conversation by
        // forking the parent's session; afterwards it resumes its own.
        let (resume_external_session, fork_session) = match own_session {
            Some(id) => (Some(id), false),
            None => match self.inherited_session(&thread).await? {
                Some(parent) => (Some(parent), true),
                None => (None, false),
            },
        };

        Ok(SessionConfig {
            cwd,
            model,
            permission_mode,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            extra_dirs,
            mcp_config_path: None,
            append_system_prompt: None,
            max_turns: None,
            env: BTreeMap::new(),
            resume_external_session,
            billing: self.billing,
            approval_hook: None,
            fork_session,
        })
    }

    pub(super) async fn on_agent_event(&self, thread_id: &ThreadId, event: AgentEvent) {
        if let AgentEvent::TranscriptEntry { uuid } = event {
            // Kept for the turn's stored answer (fork point); not for clients.
            if let Some(thread) = self.lock().threads.get_mut(thread_id) {
                thread.last_claude_uuid = Some(uuid);
            }
            return;
        }
        if let AgentEvent::SessionStarted {
            external_session_id,
            model,
        } = &event
        {
            let session_id = {
                let mut inner = self.lock();
                if let Some(thread) = inner.threads.get_mut(thread_id) {
                    if let Some(session) = thread.live_session.as_mut() {
                        if external_session_id.is_some() {
                            session.external_session_id = external_session_id.clone();
                        }
                        if model.is_some() {
                            session.model = model.clone();
                        }
                        Some((
                            session.id.clone(),
                            session.external_session_id.clone(),
                            session.model.clone(),
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some((id, external, model)) = session_id {
                if let Err(err) = self
                    .db
                    .set_agent_session_external(&id, external, model)
                    .await
                {
                    crate::log::warn(format!("failed to persist external session id: {err}"));
                }
            }
        }

        if let AgentEvent::CommandsReported {
            slash_commands,
            skills,
        } = &event
        {
            self.record_reported_commands(thread_id, slash_commands, skills)
                .await;
        }

        if matches!(event, AgentEvent::SessionInfo { .. }) {
            let mut inner = self.lock();
            if let Some(thread) = inner.threads.get_mut(thread_id) {
                thread.session_info = Some(event.clone());
            }
        }

        if let AgentEvent::TextDelta { text } = &event {
            let mut inner = self.lock();
            if let Some(thread) = inner.threads.get_mut(thread_id) {
                thread.acc_text.push_str(text);
            }
        }

        if let AgentEvent::UsageReported { report } = &event {
            let session_id = {
                let inner = self.lock();
                inner
                    .threads
                    .get(thread_id)
                    .and_then(|t| t.live_session.as_ref().map(|s| s.id.clone()))
            };
            if let Some(session_id) = session_id {
                if let Err(err) = self.db.record_usage(&session_id, thread_id, report).await {
                    crate::log::warn(format!("failed to record usage: {err}"));
                }
            }
        }

        let turn_completed = match &event {
            AgentEvent::TurnCompleted {
                num_turns,
                cost_usd_micros,
            } => Some((*num_turns, *cost_usd_micros)),
            _ => None,
        };
        let failure_text = match &event {
            AgentEvent::Failed { message } => Some(message.clone()),
            _ => None,
        };

        self.broadcast_agent_event(thread_id, event);

        if let Some((num_turns, cost_usd_micros)) = turn_completed {
            self.finish_turn(thread_id, num_turns, cost_usd_micros)
                .await;
        } else if let Some(failure) = failure_text {
            self.finish_failed_turn(thread_id, &failure).await;
        }
    }

    /// A terminal `Failed` event ended the turn. Persist one accurate system
    /// message — including any partial output that would otherwise be lost —
    /// and release the thread. Once a turn has completed, its reservation is
    /// already released, so a late `Failed` event appends nothing.
    pub(super) async fn finish_failed_turn(&self, thread_id: &ThreadId, failure: &str) {
        let (reserved, interrupted, partial, activity) = {
            let mut inner = self.lock();
            match inner.threads.get_mut(thread_id) {
                Some(thread) if thread.reservation == Some(Reservation::Turn) => {
                    thread.reservation = None;
                    (
                        true,
                        thread.interrupt_requested,
                        std::mem::take(&mut thread.acc_text),
                        std::mem::take(&mut thread.activity),
                    )
                }
                _ => (false, false, String::new(), Default::default()),
            }
        };
        if !reserved {
            return;
        }
        // The agent received the prompt: its comments stay delivered, unanswered.
        self.take_turn_delivery(thread_id);
        let mut text = if interrupted {
            format!("The agent turn was interrupted: {failure}")
        } else {
            format!("The agent turn failed: {failure}")
        };
        if !partial.is_empty() {
            text.push_str("\n\nPartial output before the failure:\n");
            text.push_str(&partial);
        }
        self.append_system_notice_with_activity(thread_id, text, activity.into_items())
            .await;
        self.after_turn(thread_id, agents::TurnEnd::Failed(failure.to_owned()));
    }

    pub(super) async fn finish_turn(
        &self,
        thread_id: &ThreadId,
        num_turns: u32,
        cost_usd_micros: Option<u64>,
    ) {
        let (text, session, claude_uuid, activity) = {
            let mut inner = self.lock();
            match inner.threads.get_mut(thread_id) {
                Some(thread) => (
                    std::mem::take(&mut thread.acc_text),
                    thread.live_session.clone(),
                    thread.last_claude_uuid.take(),
                    std::mem::take(&mut thread.activity),
                ),
                None => return,
            }
        };
        let Some(session) = session else {
            self.release_turn(thread_id);
            self.after_turn(
                thread_id,
                agents::TurnEnd::Failed("the agent session ended before the turn".into()),
            );
            return;
        };
        let land_result = subthreads::parse_land_result(&text);

        let mut assistant = Message::new_assistant(
            thread_id.clone(),
            session.id.clone(),
            vec![ContentBlock::Text { text }],
        );
        assistant.claude_uuid = claude_uuid;
        let persisted = self.db.append_message(&assistant).await;
        if let Err(err) = &persisted {
            crate::log::warn(format!("failed to persist assistant message: {err}"));
        } else if let Err(err) = self
            .append_event(
                Aggregate::Thread {
                    id: thread_id.clone(),
                },
                assistant.actor.clone(),
                DomainEvent::MessageAdded {
                    message: assistant.clone(),
                },
            )
            .await
        {
            crate::log::warn(format!("failed to append assistant MessageAdded: {err}"));
        }

        let answer_id = assistant.id.clone();
        if persisted.is_ok() {
            self.record_activity(
                thread_id,
                &answer_id,
                assistant.actor.clone(),
                activity.into_items(),
            )
            .await;
            // Before the turn is released: no other turn can edit meanwhile.
            self.record_turn_checkpoint(thread_id, &answer_id).await;
        }
        self.broadcast(
            thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message: assistant,
            },
        );
        if persisted.is_ok() {
            self.link_comment_answers(thread_id, &answer_id).await;
        } else {
            self.take_turn_delivery(thread_id);
        }

        if let Err(err) = self
            .append_event(
                Aggregate::Session {
                    id: session.id.clone(),
                },
                Actor::Agent {
                    session: session.id.clone(),
                },
                DomainEvent::AgentTurnCompleted {
                    thread_id: thread_id.clone(),
                    session_id: session.id.clone(),
                    num_turns,
                    cost_usd_micros,
                },
            )
            .await
        {
            crate::log::warn(format!("failed to append AgentTurnCompleted: {err}"));
        }

        let totals = self
            .db
            .usage_totals_for_thread(thread_id)
            .await
            .unwrap_or_else(|_| UsageReport {
                model: session.model.clone().unwrap_or_default(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            });
        self.broadcast(
            thread_id,
            DaemonMessage::UsageSnapshot {
                thread_id: thread_id.clone(),
                totals,
            },
        );
        self.release_turn(thread_id);
        if let Some((outcome, summary)) = land_result {
            self.record_land_result(thread_id, outcome, summary).await;
        }
        self.after_turn(thread_id, agents::TurnEnd::Completed);
    }

    /// The adapter event stream closed. If a turn was still in flight it never
    /// reached a terminal event, so exactly one accurate interruption notice
    /// is persisted — with any partial output — and the thread is freed. When
    /// the stream closes after a completed (or failed) turn, nothing is
    /// appended and only the live session is dropped.
    pub(super) async fn on_pump_closed(&self, thread_id: &ThreadId) {
        let (in_flight, partial, activity) = {
            let mut inner = self.lock();
            let token = inner
                .threads
                .get_mut(thread_id)
                .and_then(|thread| thread.approval_token.take());
            if let Some(token) = token {
                inner.approval_tokens.remove(&token);
            }
            match inner.threads.get_mut(thread_id) {
                Some(thread) => {
                    thread.live_session = None;
                    thread.session_allowed_tools.clear();
                    thread.session_info = None;
                    if thread.reservation == Some(Reservation::Turn) {
                        thread.reservation = None;
                        (
                            true,
                            std::mem::take(&mut thread.acc_text),
                            std::mem::take(&mut thread.activity),
                        )
                    } else {
                        (false, String::new(), Default::default())
                    }
                }
                None => (false, String::new(), Default::default()),
            }
        };
        // A hook still waiting would otherwise block until its timeout.
        self.cancel_thread_approvals(
            thread_id,
            "The agent session ended before this approval was answered.",
            false,
        )
        .await;
        self.drop_trama_tools(thread_id);
        if !in_flight {
            return;
        }
        self.take_turn_delivery(thread_id);
        let mut text = "The agent session ended unexpectedly before the turn completed.".to_owned();
        if !partial.is_empty() {
            text.push_str("\n\nPartial output before the session ended:\n");
            text.push_str(&partial);
        }
        self.append_system_notice_with_activity(thread_id, text, activity.into_items())
            .await;
        self.broadcast(
            thread_id,
            DaemonMessage::AgentEventEnvelope {
                thread_id: thread_id.clone(),
                event: AgentEvent::Failed {
                    message: "The agent session ended unexpectedly.".into(),
                },
            },
        );
        self.after_turn(
            thread_id,
            agents::TurnEnd::Failed("the agent session ended unexpectedly".into()),
        );
    }
}
