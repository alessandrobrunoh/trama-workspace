use super::*;

impl SessionManager {
    pub async fn subscribe(
        &self,
        thread_id: &ThreadId,
        tx: mpsc::UnboundedSender<DaemonMessage>,
    ) -> Result<SubscriptionId> {
        if self.db.get_thread(thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        let mut history = self.inherited_messages(thread_id).await?;
        history.extend(self.db.list_messages_by_thread(thread_id).await?);
        let frames = self.history_frames(thread_id, history).await;
        // Registered and replayed under one lock: agent events (and the
        // in-flight turn's activity, updated with them) cannot slip between
        // the replay and the live stream.
        let id = {
            let mut inner = self.lock();
            let id = SubscriptionId(inner.next_sub);
            inner.next_sub += 1;
            let thread = inner.threads.entry(thread_id.clone()).or_default();
            thread.subscribers.push(Subscriber { id, tx: tx.clone() });
            for frame in frames {
                let _ = tx.send(frame);
            }
            if thread.reservation == Some(Reservation::Turn) {
                let _ = tx.send(DaemonMessage::TurnActivity {
                    thread_id: thread_id.clone(),
                    message_id: None,
                    items: thread.activity.items().to_vec(),
                });
            }
            id
        };
        let totals = self.db.usage_totals_for_thread(thread_id).await?;
        if usage_report_present(&totals) {
            let _ = tx.send(DaemonMessage::UsageSnapshot {
                thread_id: thread_id.clone(),
                totals,
            });
        }
        // A client that (re)connects mid-turn must see what the agent is
        // blocked on. Request ids are ULID-based, so sorting keeps order.
        let (session_info, mut pending): (
            Option<AgentEvent>,
            Vec<(String, String, serde_json::Value)>,
        ) = {
            let inner = self.lock();
            (
                inner
                    .threads
                    .get(thread_id)
                    .and_then(|t| t.session_info.clone()),
                inner
                    .pending_approvals
                    .iter()
                    .filter(|(_, p)| &p.thread_id == thread_id)
                    .map(|(id, p)| (id.clone(), p.tool_name.clone(), p.input_json.clone()))
                    .collect(),
            )
        };
        if let Some(event) = session_info {
            let _ = tx.send(DaemonMessage::AgentEventEnvelope {
                thread_id: thread_id.clone(),
                event,
            });
        }
        pending.sort_by(|a, b| a.0.cmp(&b.0));
        for (request_id, tool_name, input_json) in pending {
            let _ = tx.send(DaemonMessage::AgentEventEnvelope {
                thread_id: thread_id.clone(),
                event: AgentEvent::ApprovalRequested {
                    request_id,
                    tool_name,
                    input_json,
                },
            });
        }
        Ok(id)
    }

    pub fn unsubscribe(&self, thread_id: &ThreadId, sub_id: SubscriptionId) -> Result<()> {
        let mut inner = self.lock();
        if let Some(thread) = inner.threads.get_mut(thread_id) {
            thread.subscribers.retain(|s| s.id != sub_id);
            Ok(())
        } else {
            Err(DaemonError::NotFound(format!("thread {thread_id}")))
        }
    }

    /// Receive the thread's live broadcasts without the history replay of
    /// [`Self::subscribe`]. Used by the collab runner to stream a turn.
    pub(crate) fn watch_thread(
        &self,
        thread_id: &ThreadId,
        tx: mpsc::UnboundedSender<DaemonMessage>,
    ) -> SubscriptionId {
        let mut inner = self.lock();
        let id = SubscriptionId(inner.next_sub);
        inner.next_sub += 1;
        let thread = inner.threads.entry(thread_id.clone()).or_default();
        thread.subscribers.push(Subscriber { id, tx });
        id
    }

    /// Any exclusive operation (turn, migration, checkout copy) holds the thread.
    pub(crate) fn thread_busy(&self, thread_id: &ThreadId) -> bool {
        self.lock()
            .threads
            .get(thread_id)
            .is_some_and(|thread| thread.reservation.is_some())
    }

    /// An agent turn is in flight (accepted and not yet terminal).
    pub(crate) fn turn_running(&self, thread_id: &ThreadId) -> bool {
        self.lock()
            .threads
            .get(thread_id)
            .is_some_and(|thread| thread.reservation == Some(Reservation::Turn))
    }

    pub(crate) fn db(&self) -> &TramaDb {
        &self.db
    }

    /// Send `message` to the thread's subscribers.
    pub(crate) fn notify(&self, thread_id: &ThreadId, message: DaemonMessage) {
        self.broadcast(thread_id, message);
    }

    pub fn unsubscribe_all(&self, subs: &[(ThreadId, SubscriptionId)]) {
        let mut inner = self.lock();
        for (thread_id, sub_id) in subs {
            if let Some(thread) = inner.threads.get_mut(thread_id) {
                thread.subscribers.retain(|s| s.id != *sub_id);
            }
        }
    }
}
