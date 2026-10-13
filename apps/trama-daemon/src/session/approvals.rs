use super::*;

impl SessionManager {
    pub async fn respond_approval(
        &self,
        thread_id: &ThreadId,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<()> {
        if self.db.get_thread(thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        if let Some((answered, remembered)) = self.take_answered(thread_id, request_id, decision) {
            let hook = match decision {
                ApprovalDecision::Deny => HookDecision::deny(USER_DENIED),
                allow => HookDecision::allow(allow),
            };
            self.finish_approvals(
                vec![answered],
                &hook,
                None,
                Actor::Human { id: "user".into() },
            )
            .await;
            // Other requests for the same tool that were already waiting.
            self.finish_approvals(
                remembered,
                &HookDecision::allow(ApprovalDecision::AllowSession),
                Some(REMEMBERED_FOR_SESSION.into()),
                Actor::System,
            )
            .await;
            return Ok(());
        }
        // Brokered sessions (Claude Code, or any session given the hook)
        // answer only through the broker above. Other runtimes may hold the
        // request in-band.
        if self.approval_hook.is_some() || self.adapter.kind() == AgentKind::ClaudeCode {
            return Err(DaemonError::NotFound(format!(
                "approval request {request_id} is not pending (already answered, timed out, or the session ended)"
            )));
        }
        let session = {
            let inner = self.lock();
            inner
                .threads
                .get(thread_id)
                .and_then(|t| t.live_session.clone())
        };
        match session {
            Some(session) => {
                self.adapter
                    .respond_approval(&session, request_id, decision)
                    .await?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Remove the answered request (only if it belongs to `thread_id`). For
    /// `allow_session`, remember the tool for the live session and also take
    /// every other pending request for that tool on the thread.
    #[allow(clippy::type_complexity)]
    pub(super) fn take_answered(
        &self,
        thread_id: &ThreadId,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Option<((String, PendingApproval), Vec<(String, PendingApproval)>)> {
        let mut inner = self.lock();
        if inner
            .pending_approvals
            .get(request_id)
            .is_none_or(|p| &p.thread_id != thread_id)
        {
            return None;
        }
        let answered = inner.pending_approvals.remove(request_id)?;
        let mut remembered = Vec::new();
        if decision == ApprovalDecision::AllowSession {
            let tool = answered.tool_name.clone();
            if let Some(thread) = inner.threads.get_mut(thread_id) {
                if thread
                    .live_session
                    .as_ref()
                    .is_some_and(|s| s.id == answered.session_id)
                {
                    thread.session_allowed_tools.insert(tool.clone());
                }
            }
            let siblings: Vec<String> = inner
                .pending_approvals
                .iter()
                .filter(|(_, p)| {
                    &p.thread_id == thread_id
                        && p.tool_name == tool
                        && p.session_id == answered.session_id
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in siblings {
                if let Some(p) = inner.pending_approvals.remove(&id) {
                    remembered.push((id, p));
                }
            }
        }
        Some(((request_id.to_string(), answered), remembered))
    }

    /// Called for every `PermissionRequest` hook. Unknown tokens and ended
    /// sessions are denied; tools remembered for the session are allowed
    /// without asking; anything else is shown to subscribers.
    pub async fn request_approval(
        &self,
        token: &str,
        tool_name: String,
        input_json: serde_json::Value,
    ) -> ApprovalWait {
        enum Next {
            Deny(&'static str),
            Remembered(ThreadId, AgentSessionId),
            Ask(ThreadId, AgentSessionId),
        }
        let request_id = format!("apr-{}", MessageId::generate());
        let (tx, rx) = oneshot::channel();
        let next = {
            let mut inner = self.lock();
            match inner.approval_tokens.get(token).cloned() {
                None => Next::Deny("This approval token does not belong to a live Trama session."),
                Some(thread_id) => {
                    let live = inner.threads.get(&thread_id).and_then(|thread| {
                        let session = thread.live_session.as_ref()?;
                        Some((
                            session.id.clone(),
                            thread.session_allowed_tools.contains(&tool_name),
                        ))
                    });
                    match live {
                        None => Next::Deny("The agent session has ended in Trama."),
                        Some((session_id, true)) => Next::Remembered(thread_id, session_id),
                        Some((session_id, false)) => {
                            inner.pending_approvals.insert(
                                request_id.clone(),
                                PendingApproval {
                                    thread_id: thread_id.clone(),
                                    session_id: session_id.clone(),
                                    tool_name: tool_name.clone(),
                                    input_json: input_json.clone(),
                                    responder: tx,
                                },
                            );
                            Next::Ask(thread_id, session_id)
                        }
                    }
                }
            }
        };

        match next {
            Next::Deny(message) => ApprovalWait::Decided(HookDecision::deny(message)),
            Next::Remembered(thread_id, session_id) => {
                self.record_approval(
                    &session_id,
                    Actor::System,
                    DomainEvent::ApprovalResolved {
                        thread_id: thread_id.clone(),
                        session_id: session_id.clone(),
                        request_id,
                        tool_name,
                        decision: ApprovalDecision::AllowSession,
                        reason: Some(REMEMBERED_FOR_SESSION.into()),
                    },
                )
                .await;
                ApprovalWait::Decided(HookDecision::allow(ApprovalDecision::AllowSession))
            }
            Next::Ask(thread_id, session_id) => {
                self.record_approval(
                    &session_id,
                    Actor::Agent {
                        session: session_id.clone(),
                    },
                    DomainEvent::ApprovalRequested {
                        thread_id: thread_id.clone(),
                        session_id: session_id.clone(),
                        request_id: request_id.clone(),
                        tool_name: tool_name.clone(),
                        input_json: input_json.clone(),
                    },
                )
                .await;
                self.broadcast(
                    &thread_id,
                    DaemonMessage::AgentEventEnvelope {
                        thread_id: thread_id.clone(),
                        event: AgentEvent::ApprovalRequested {
                            request_id: request_id.clone(),
                            tool_name,
                            input_json,
                        },
                    },
                );
                ApprovalWait::Pending { request_id, rx }
            }
        }
    }

    /// Deny a pending request that timed out or whose hook went away.
    /// Returns `false` when it was already resolved, so the caller should
    /// report that resolution instead.
    pub async fn cancel_approval(&self, request_id: &str, message: &str) -> bool {
        let taken = self.lock().pending_approvals.remove(request_id);
        let Some(pending) = taken else {
            return false;
        };
        self.finish_approvals(
            vec![(request_id.to_string(), pending)],
            &HookDecision::deny(message),
            Some(message.to_string()),
            Actor::System,
        )
        .await;
        true
    }

    /// Deny every pending request on a thread (interrupt, session end).
    pub(super) async fn cancel_thread_approvals(
        &self,
        thread_id: &ThreadId,
        message: &str,
        interrupt: bool,
    ) {
        let taken: Vec<(String, PendingApproval)> = {
            let mut inner = self.lock();
            let ids: Vec<String> = inner
                .pending_approvals
                .iter()
                .filter(|(_, p)| &p.thread_id == thread_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| inner.pending_approvals.remove(&id).map(|p| (id, p)))
                .collect()
        };
        if taken.is_empty() {
            return;
        }
        let mut decision = HookDecision::deny(message);
        decision.interrupt = interrupt;
        self.finish_approvals(taken, &decision, Some(message.to_string()), Actor::System)
            .await;
    }

    /// Unblock each hook, persist the resolution, and tell subscribers.
    pub(super) async fn finish_approvals(
        &self,
        resolved: Vec<(String, PendingApproval)>,
        decision: &HookDecision,
        reason: Option<String>,
        actor: Actor,
    ) {
        for (request_id, pending) in resolved {
            // The hook may already be gone; its connection task then cancels
            // nothing, because the request is no longer pending.
            let _ = pending.responder.send(decision.clone());
            self.record_approval(
                &pending.session_id,
                actor.clone(),
                DomainEvent::ApprovalResolved {
                    thread_id: pending.thread_id.clone(),
                    session_id: pending.session_id.clone(),
                    request_id: request_id.clone(),
                    tool_name: pending.tool_name,
                    decision: decision.decision,
                    reason: reason.clone(),
                },
            )
            .await;
            self.broadcast(
                &pending.thread_id,
                DaemonMessage::AgentEventEnvelope {
                    thread_id: pending.thread_id.clone(),
                    event: AgentEvent::ApprovalResolved {
                        request_id,
                        decision: decision.decision,
                        reason: reason.clone(),
                    },
                },
            );
        }
    }

    pub(super) async fn record_approval(
        &self,
        session_id: &AgentSessionId,
        actor: Actor,
        event: DomainEvent,
    ) {
        let kind = event.kind();
        if let Err(err) = self
            .append_event(
                Aggregate::Session {
                    id: session_id.clone(),
                },
                actor,
                event,
            )
            .await
        {
            crate::log::warn(format!("failed to append {kind}: {err}"));
        }
    }

    /// Give a new session's config the hook, the daemon socket, and a fresh
    /// token mapped to `thread_id`. Returns the token so a failed start can
    /// drop it.
    pub(super) fn attach_approval_hook(
        &self,
        thread_id: &ThreadId,
        config: &mut SessionConfig,
    ) -> Option<String> {
        let settings = self.approval_hook.as_ref()?;
        let token = approval::generate_token();
        config.env.insert(
            approval::ENV_SOCKET.into(),
            settings.socket_path.to_string_lossy().into_owned(),
        );
        config.env.insert(approval::ENV_TOKEN.into(), token.clone());
        config.approval_hook = Some(settings.hook_config());
        let mut inner = self.lock();
        inner
            .approval_tokens
            .insert(token.clone(), thread_id.clone());
        let thread = inner.threads.entry(thread_id.clone()).or_default();
        if let Some(old) = thread.approval_token.replace(token.clone()) {
            inner.approval_tokens.remove(&old);
        }
        Some(token)
    }

    pub(super) fn drop_approval_token(&self, thread_id: &ThreadId, token: Option<String>) {
        let Some(token) = token else {
            return;
        };
        let mut inner = self.lock();
        inner.approval_tokens.remove(&token);
        if let Some(thread) = inner.threads.get_mut(thread_id) {
            if thread.approval_token.as_deref() == Some(token.as_str()) {
                thread.approval_token = None;
            }
        }
    }
}
