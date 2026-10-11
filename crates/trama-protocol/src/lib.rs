//! Versioned JSON-lines IPC between the desktop UI, the local daemon, and (later) runners.
//!
//! # Framing
//!
//! Each message is a single JSON object encoded as UTF-8 and terminated by a newline
//! (`\n`). There is no length prefix and no embedded raw newlines inside a value
//! (`serde_json::to_string` emits compact JSON). A transport is any reliable byte
//! stream (Unix domain socket today; TCP/SSH for remote runners later).
//!
//! Decode one message per line; ignore empty lines. A line that is not valid JSON
//! or does not match a known schema is a protocol error.
//!
//! # Tagging
//!
//! [`ClientMessage`] and [`DaemonMessage`] are **adjacently tagged** serde enums:
//! `{"type":"<snake_case_variant>","payload": ... }`. Unit variants omit `payload`.
//!
//! # Unknown-tag policy
//!
//! Unknown `type` tags are **denied**. Serde rejects them at deserialize time; the
//! daemon must not silently ignore a variant it does not understand (forward-incompatible
//! clients fail the handshake or receive `DaemonMessage::Error` after it). Extra object
//! fields inside a known payload are ignored (serde default), so additive field changes
//! within an existing variant stay compatible. Bump [`PROTOCOL_VERSION`] for breaking
//! tag or payload shape changes.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub use trama_domain::agent::{AgentEvent, ApprovalDecision, UsageReport};
pub use trama_domain::ids::{ThreadId, WorkspaceId};
pub use trama_domain::message::Message;

/// Wire protocol version carried in `Hello` / `Welcome`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Encode `value` as one JSON line (compact JSON plus a trailing `\n`).
pub fn encode_json_line<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    Ok(buf)
}

/// Decode a single JSON line. A trailing `\n` / `\r\n` is tolerated.
pub fn decode_json_line<T: DeserializeOwned>(line: &str) -> serde_json::Result<T> {
    serde_json::from_str(line.trim_end_matches(['\r', '\n']))
}

/// UI → daemon (and later runner).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        client_name: String,
    },
    ListWorkspaces,
    ListThreads {
        workspace_id: WorkspaceId,
    },
    CreateThread {
        workspace_id: WorkspaceId,
        title: String,
    },
    SubscribeThread {
        thread_id: ThreadId,
    },
    UnsubscribeThread {
        thread_id: ThreadId,
    },
    SendPrompt {
        thread_id: ThreadId,
        text: String,
    },
    Interrupt {
        thread_id: ThreadId,
    },
    RespondApproval {
        thread_id: ThreadId,
        request_id: String,
        decision: ApprovalDecision,
    },
    Shutdown,
}

/// Daemon → UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum DaemonMessage {
    Welcome {
        protocol_version: u32,
        daemon_version: String,
        accepted: bool,
    },
    Workspaces(Vec<WorkspaceSnapshot>),
    Threads(Vec<ThreadSnapshot>),
    ThreadCreated(ThreadSnapshot),
    MessageAppended {
        thread_id: ThreadId,
        message: Message,
    },
    AgentEventEnvelope {
        thread_id: ThreadId,
        event: AgentEvent,
    },
    UsageSnapshot {
        thread_id: ThreadId,
        totals: UsageReport,
    },
    Error {
        code: String,
        message: String,
    },
}

/// Minimal workspace view for IPC. Not the domain `Workspace` aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: WorkspaceId,
    pub title: String,
    pub repo_count: u32,
}

/// Minimal thread view for IPC. Not the domain `Thread` aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSnapshot {
    pub id: ThreadId,
    pub title: String,
    pub repo_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama_domain::agent::AgentEvent;
    use trama_domain::ids::ThreadId;
    use trama_domain::message::Message;

    fn roundtrip_eq<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value);
        let again = serde_json::to_string(&back).unwrap();
        assert_eq!(again, json);
    }

    fn roundtrip_json<T>(value: &T)
    where
        T: Serialize + DeserializeOwned,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            serde_json::to_value(&back).unwrap()
        );
        let line = encode_json_line(value).unwrap();
        assert_eq!(*line.last().unwrap(), b'\n');
        assert_eq!(line.iter().filter(|b| **b == b'\n').count(), 1);
        let decoded: T = decode_json_line(std::str::from_utf8(&line).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            serde_json::to_value(&decoded).unwrap()
        );
    }

    fn workspace_id() -> WorkspaceId {
        WorkspaceId::from("01hworkspace000000000000001")
    }

    fn thread_id() -> ThreadId {
        ThreadId::from("01hthread0000000000000000001")
    }

    fn snapshot_ws() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: workspace_id(),
            title: "LaFraternita".into(),
            repo_count: 2,
        }
    }

    fn snapshot_th() -> ThreadSnapshot {
        ThreadSnapshot {
            id: thread_id(),
            title: "Fix auth".into(),
            repo_count: 1,
        }
    }

    #[test]
    fn client_message_roundtrip_every_kind() {
        let tid = thread_id();
        let wid = workspace_id();
        let kinds = [
            ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "trama-desktop".into(),
            },
            ClientMessage::ListWorkspaces,
            ClientMessage::ListThreads {
                workspace_id: wid.clone(),
            },
            ClientMessage::CreateThread {
                workspace_id: wid,
                title: "New thread".into(),
            },
            ClientMessage::SubscribeThread {
                thread_id: tid.clone(),
            },
            ClientMessage::UnsubscribeThread {
                thread_id: tid.clone(),
            },
            ClientMessage::SendPrompt {
                thread_id: tid.clone(),
                text: "hello".into(),
            },
            ClientMessage::Interrupt {
                thread_id: tid.clone(),
            },
            ClientMessage::RespondApproval {
                thread_id: tid,
                request_id: "req-1".into(),
                decision: ApprovalDecision::AllowOnce,
            },
            ClientMessage::Shutdown,
        ];
        for msg in &kinds {
            roundtrip_eq(msg);
            roundtrip_json(msg);
        }
        assert_eq!(kinds.len(), 10);
    }

    #[test]
    fn daemon_message_roundtrip_every_kind() {
        let tid = thread_id();
        let message = Message::new_user(tid.clone(), "alice", "hi");
        let kinds = [
            DaemonMessage::Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "0.1.0".into(),
                accepted: true,
            },
            DaemonMessage::Workspaces(vec![snapshot_ws()]),
            DaemonMessage::Threads(vec![snapshot_th()]),
            DaemonMessage::ThreadCreated(snapshot_th()),
            DaemonMessage::MessageAppended {
                thread_id: tid.clone(),
                message,
            },
            DaemonMessage::AgentEventEnvelope {
                thread_id: tid.clone(),
                event: AgentEvent::TextDelta {
                    text: "chunk".into(),
                },
            },
            DaemonMessage::UsageSnapshot {
                thread_id: tid,
                totals: UsageReport {
                    model: "mock".into(),
                    input_tokens: 3,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 1,
                },
            },
            DaemonMessage::Error {
                code: "not_found".into(),
                message: "thread missing".into(),
            },
        ];
        for msg in &kinds {
            roundtrip_json(msg);
        }
        assert_eq!(kinds.len(), 8);
    }

    #[test]
    fn unknown_tags_are_denied() {
        let client =
            serde_json::from_str::<ClientMessage>(r#"{"type":"launch_nukes","payload":{}}"#);
        assert!(client.is_err(), "unknown client tags must be denied");
        let daemon =
            serde_json::from_str::<DaemonMessage>(r#"{"type":"secret_backdoor","payload":null}"#);
        assert!(daemon.is_err(), "unknown daemon tags must be denied");
        let client_err = client.unwrap_err().to_string();
        assert!(
            client_err.contains("unknown variant") || client_err.contains("launch_nukes"),
            "{client_err}"
        );
    }

    #[test]
    fn hello_json_shape_is_adjacently_tagged() {
        let json = serde_json::to_string(&ClientMessage::Hello {
            protocol_version: 1,
            client_name: "ui".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"hello","payload":{"protocol_version":1,"client_name":"ui"}}"#
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::ListWorkspaces).unwrap(),
            r#"{"type":"list_workspaces"}"#
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::Workspaces(vec![])).unwrap(),
            r#"{"type":"workspaces","payload":[]}"#
        );
    }

    #[test]
    fn protocol_version_is_one() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
