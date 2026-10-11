use crate::ids::{AgentSessionId, MessageId, ThreadId};
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
}
