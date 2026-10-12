//! Turn activity: the in-flight turn's tool calls, thinking and subagent
//! lifecycle are accumulated with the event stream, stored with the message
//! that ends the turn, and replayed to subscribers before that message.

use super::*;
use trama_domain::activity::ActivityItem;

impl SessionManager {
    /// Forward an agent event to the thread's subscribers, recording it in
    /// the in-flight turn's activity under the same lock, so a subscriber
    /// sees each step exactly once (in its replay or live).
    pub(super) fn broadcast_agent_event(&self, thread_id: &ThreadId, event: AgentEvent) {
        let mut inner = self.lock();
        let Some(thread) = inner.threads.get_mut(thread_id) else {
            return;
        };
        if thread.reservation == Some(Reservation::Turn) {
            thread.activity.apply(&event, Utc::now());
        }
        let message = DaemonMessage::AgentEventEnvelope {
            thread_id: thread_id.clone(),
            event,
        };
        thread
            .subscribers
            .retain(|sub| sub.tx.send(message.clone()).is_ok());
    }

    /// Store a finished turn's activity next to `message_id`. A store
    /// failure is logged: the transcript itself is already saved.
    pub(super) async fn record_activity(
        &self,
        thread_id: &ThreadId,
        message_id: &MessageId,
        actor: Actor,
        items: Vec<ActivityItem>,
    ) {
        if items.is_empty() {
            return;
        }
        if let Err(err) = self
            .db
            .record_turn_activity(thread_id, message_id, actor, items)
            .await
        {
            crate::log::warn(format!("failed to store turn activity: {err}"));
        }
    }

    /// The replay of `history`: each message, preceded by the activity of
    /// the turn it ends (inherited messages included).
    pub(super) async fn history_frames(
        &self,
        thread_id: &ThreadId,
        history: Vec<Message>,
    ) -> Vec<DaemonMessage> {
        let mut owners: Vec<ThreadId> = Vec::new();
        for message in &history {
            if !owners.contains(&message.thread_id) {
                owners.push(message.thread_id.clone());
            }
        }
        let mut activity = match self.db.turn_activity_for_threads(&owners).await {
            Ok(activity) => activity,
            Err(err) => {
                crate::log::warn(format!("failed to load turn activity: {err}"));
                Default::default()
            }
        };
        let mut frames = Vec::with_capacity(history.len());
        for message in history {
            if let Some(items) = activity.remove(&message.id) {
                frames.push(DaemonMessage::TurnActivity {
                    thread_id: thread_id.clone(),
                    message_id: Some(message.id.clone()),
                    items,
                });
            }
            frames.push(DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message,
            });
        }
        frames
    }
}
