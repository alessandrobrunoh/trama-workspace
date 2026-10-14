//! Plain chat turns through a user-added API provider (ADR 0015).
//!
//! The prompt is stored exactly like a Claude Code turn; the answer streams
//! as `TextDelta` events and is stored as an assistant message. There are
//! no tools, files, approvals or subagents on this path: the provider only
//! sees the thread's text history.

use std::sync::Arc;

use tokio::sync::Notify;
use trama_domain::message::ContentBlock;
use trama_infra::llm::{ChatMessage, LlmError};

use super::*;
use crate::providers::ChatRoute;

/// Most recent messages sent as history.
const MAX_HISTORY_MESSAGES: usize = 60;
/// History is trimmed (oldest first) to about this many characters.
const MAX_HISTORY_CHARS: usize = 400_000;

const SYSTEM_PROMPT: &str = "You are a helpful assistant inside TramaWorkspace, \
a desktop app for coding with AI agents. In this conversation you are a plain chat \
model: you cannot run tools, read or edit files, or run commands. Answer from the \
conversation only and say so when a task needs file or tool access.";

impl SessionManager {
    /// The API provider route for the thread's effective model, `None` for
    /// Claude Code.
    pub(super) async fn chat_route(&self, thread_id: &ThreadId) -> Result<Option<ChatRoute>> {
        let thread = self
            .db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let model = self.thread_model_override(&thread).await?;
        self.providers.route(model.as_deref())
    }

    async fn provider_history(&self, thread_id: &ThreadId, prompt: &str) -> Vec<ChatMessage> {
        let stored = self
            .db
            .list_messages_by_thread(thread_id)
            .await
            .unwrap_or_default();
        let mut history: Vec<ChatMessage> = stored
            .iter()
            .filter_map(|message| {
                let role = match message.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    _ => return None,
                };
                let text: Vec<&str> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let text = text.join("\n");
                (!text.trim().is_empty()).then(|| ChatMessage::new(role, text))
            })
            .collect();
        // The stored prompt is the last user message; send what the turn
        // actually carries (it may have a preamble).
        if history.last().is_some_and(|m| m.role == "user") {
            history.pop();
        }
        history.push(ChatMessage::new("user", prompt));
        if history.len() > MAX_HISTORY_MESSAGES {
            history.drain(..history.len() - MAX_HISTORY_MESSAGES);
        }
        while history.len() > 1
            && history.iter().map(|m| m.content.len()).sum::<usize>() > MAX_HISTORY_CHARS
        {
            history.remove(0);
        }
        // Providers expect the conversation to start with a user turn.
        while history.len() > 1 && history[0].role != "user" {
            history.remove(0);
        }
        history.insert(0, ChatMessage::new("system", SYSTEM_PROMPT));
        history
    }

    fn on_provider_delta(&self, thread_id: &ThreadId, text: &str) {
        if let Some(thread) = self.lock().threads.get_mut(thread_id) {
            thread.acc_text.push_str(text);
        }
        self.broadcast_agent_event(
            thread_id,
            AgentEvent::TextDelta {
                text: text.to_owned(),
            },
        );
    }

    pub(super) async fn run_provider_turn(
        &self,
        thread_id: ThreadId,
        prompt: String,
        route: ChatRoute,
    ) {
        let cancel = Arc::new(Notify::new());
        {
            let mut inner = self.lock();
            if let Some(thread) = inner.threads.get_mut(&thread_id) {
                if thread.interrupt_requested {
                    cancel.notify_one();
                }
                thread.provider_cancel = Some(cancel.clone());
            }
        }
        self.broadcast_agent_event(
            &thread_id,
            AgentEvent::SessionStarted {
                external_session_id: None,
                model: Some(format!("{} · {}", route.provider_name, route.model)),
            },
        );
        let history = self.provider_history(&thread_id, &prompt).await;
        let manager = self.clone();
        let delta_thread = thread_id.clone();
        let result = route
            .client
            .stream_chat(
                &route.model,
                &history,
                |delta| manager.on_provider_delta(&delta_thread, delta),
                async move { cancel.notified().await },
            )
            .await;
        if let Some(thread) = self.lock().threads.get_mut(&thread_id) {
            thread.provider_cancel = None;
        }
        match result {
            Ok(completion) => {
                self.finish_provider_turn(&thread_id, completion.text, &route)
                    .await
            }
            Err(err) => {
                let message = match err {
                    LlmError::Cancelled => "interrupted by the user".to_owned(),
                    other => format!("{} ({}): {other}", route.provider_name, route.model),
                };
                self.broadcast_agent_event(
                    &thread_id,
                    AgentEvent::Failed {
                        message: message.clone(),
                    },
                );
                self.finish_failed_turn(&thread_id, &message).await;
            }
        }
    }

    async fn finish_provider_turn(&self, thread_id: &ThreadId, text: String, route: &ChatRoute) {
        {
            let mut inner = self.lock();
            if let Some(thread) = inner.threads.get_mut(thread_id) {
                thread.acc_text.clear();
            }
        }
        self.broadcast_agent_event(
            thread_id,
            AgentEvent::TurnCompleted {
                num_turns: 1,
                cost_usd_micros: None,
            },
        );
        let text = if text.trim().is_empty() {
            format!("({} returned an empty answer.)", route.provider_name)
        } else {
            text
        };
        let assistant = Message::new_assistant(
            thread_id.clone(),
            AgentSessionId::generate(),
            vec![ContentBlock::Text { text }],
        );
        let persisted = self.db.append_message(&assistant).await;
        match &persisted {
            Err(err) => crate::log::warn(format!("failed to persist assistant message: {err}")),
            Ok(()) => {
                if let Err(err) = self
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
            }
        }
        let answer_id = assistant.id.clone();
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
        self.release_turn(thread_id);
        self.after_turn(thread_id, agents::TurnEnd::Completed);
    }
}
