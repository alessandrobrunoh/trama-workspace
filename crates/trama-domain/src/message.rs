use crate::ids::{AgentSessionId, MessageId, ThreadId};
use crate::subagent::{SubagentProfile, SubagentStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    Human { id: String },
    Agent { session: AgentSessionId },
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: serde_json::Value,
        #[serde(default)]
        is_error: bool,
    },
}

/// What a transcript row means beyond its text, so a client can render it
/// as a link to another thread. Serialized with a `kind` tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageAnnotation {
    /// This thread's agent sent the text to another thread's agent.
    AgentMessageSent {
        to_thread_id: ThreadId,
        to_title: String,
    },
    /// The prompt came from another thread's agent, not from a person.
    AgentMessageReceived {
        from_thread_id: ThreadId,
        from_title: String,
    },
    /// This thread's agent spawned a subagent thread.
    SubagentSpawned {
        child_thread_id: ThreadId,
        title: String,
        profile: SubagentProfile,
    },
    /// A subagent's task ended (the row links to the child thread).
    SubagentFinished {
        child_thread_id: ThreadId,
        title: String,
        status: SubagentStatus,
    },
}

/// Immutable message. Append-only: corrections are new messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub parent_id: Option<MessageId>,
    pub actor: Actor,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub created_at: DateTime<Utc>,
    /// Claude Code transcript uuid of the last assistant entry of the turn
    /// this message ends; the point a fork at this message cuts the
    /// conversation. `None` for other messages and older rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<MessageAnnotation>,
}

impl Message {
    pub fn new_user(
        thread_id: ThreadId,
        actor_human_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        let text = text.into();
        Self {
            id: MessageId::generate(),
            thread_id,
            parent_id: None,
            actor: Actor::Human {
                id: actor_human_id.into(),
            },
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
            created_at: Utc::now(),
            claude_uuid: None,
            annotation: None,
        }
    }

    /// A system notice (no agent, no person).
    pub fn new_system(thread_id: ThreadId, text: impl Into<String>) -> Self {
        Self {
            id: MessageId::generate(),
            thread_id,
            parent_id: None,
            actor: Actor::System,
            role: Role::System,
            content: vec![ContentBlock::Text { text: text.into() }],
            created_at: Utc::now(),
            claude_uuid: None,
            annotation: None,
        }
    }

    pub fn new_assistant(
        thread_id: ThreadId,
        session: AgentSessionId,
        blocks: Vec<ContentBlock>,
    ) -> Self {
        Self {
            id: MessageId::generate(),
            thread_id,
            parent_id: None,
            actor: Actor::Agent { session },
            role: Role::Assistant,
            content: blocks,
            created_at: Utc::now(),
            claude_uuid: None,
            annotation: None,
        }
    }

    /// Concatenated text blocks (display convenience).
    pub fn text(&self) -> String {
        self.content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.as_str(),
                _ => "",
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_user_sets_human_actor_and_text_block() {
        let thread_id = ThreadId::generate();
        let message = Message::new_user(thread_id.clone(), "alice", "hello");
        assert!(!message.id.0.is_empty());
        assert_eq!(message.thread_id, thread_id);
        assert!(message.parent_id.is_none());
        assert_eq!(message.actor, Actor::Human { id: "alice".into() });
        assert_eq!(message.role, Role::User);
        assert_eq!(
            message.content,
            vec![ContentBlock::Text {
                text: "hello".into()
            }]
        );
        assert_eq!(message.text(), "hello");
    }

    #[test]
    fn new_assistant_sets_agent_actor_and_blocks() {
        let thread_id = ThreadId::generate();
        let session = AgentSessionId::generate();
        let blocks = vec![
            ContentBlock::Thinking {
                text: "plan".into(),
            },
            ContentBlock::Text {
                text: "done".into(),
            },
        ];
        let message = Message::new_assistant(thread_id.clone(), session.clone(), blocks.clone());
        assert_eq!(message.thread_id, thread_id);
        assert_eq!(message.actor, Actor::Agent { session });
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.content, blocks);
        assert_eq!(message.text(), "done");
    }

    #[test]
    fn new_fields_are_optional_on_the_wire() {
        let message = Message::new_system(ThreadId::from("t"), "note");
        let json = serde_json::to_value(&message).unwrap();
        assert!(json.get("claude_uuid").is_none());
        assert!(json.get("annotation").is_none());
        let back: Message = serde_json::from_value(json).unwrap();
        assert!(back.claude_uuid.is_none() && back.annotation.is_none());

        let mut annotated = message;
        annotated.annotation = Some(MessageAnnotation::AgentMessageSent {
            to_thread_id: ThreadId::from("u"),
            to_title: "Worker".into(),
        });
        annotated.claude_uuid = Some("uuid-1".into());
        let json = serde_json::to_value(&annotated).unwrap();
        assert_eq!(
            json["annotation"],
            serde_json::json!({"kind":"agent_message_sent","to_thread_id":"u","to_title":"Worker"})
        );
        assert_eq!(json["claude_uuid"], "uuid-1");
        assert_eq!(
            serde_json::to_value(MessageAnnotation::SubagentFinished {
                child_thread_id: ThreadId::from("c"),
                title: "x".into(),
                status: SubagentStatus::WaitingToSync,
            })
            .unwrap(),
            serde_json::json!({"kind":"subagent_finished","child_thread_id":"c","title":"x","status":"waiting_to_sync"})
        );
    }
}
