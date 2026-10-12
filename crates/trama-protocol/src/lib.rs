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
//! daemon must not silently ignore a variant it does not understand. Because of that,
//! a new variant is a breaking handshake change: an older peer would accept `Hello`
//! and then fail the command. [`PROTOCOL_VERSION`] **2** added workspace creation,
//! repository membership, and legacy-thread migration. **3** added thread
//! checkout discovery. **4** adds the approval channel
//! ([`ClientMessage::ApprovalHook`], [`DaemonMessage::ApprovalHookDecision`])
//! and the `session_info` / `approval_resolved` agent events. Each bump is
//! transport-only. Persisted domain ids and SQLite rows are not rewritten.
//!
//! A version mismatch fails the handshake (`Welcome.accepted = false`). Extra object
//! fields inside a known payload are ignored (serde default). Bump [`PROTOCOL_VERSION`]
//! again for further tag or payload shape changes.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::PathBuf;

pub use trama_domain::agent::{AgentEvent, ApprovalDecision, BillingSource, UsageReport};
pub use trama_domain::ids::{RepositoryId, ThreadId, WorkspaceId};
pub use trama_domain::message::Message;

/// Wire protocol version carried in `Hello` / `Welcome`.
///
/// Version 4 adds the Claude approval channel and the `session_info` /
/// `approval_resolved` agent events (3 added thread checkout discovery). It
/// does not migrate stored data; peers with a different version are rejected
/// at handshake time.
pub const PROTOCOL_VERSION: u32 = 4;

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
    /// Create a workspace in the database. Does not edit `config.json`.
    CreateWorkspace {
        name: String,
    },
    /// Attach an existing local Git working tree. Not a remote clone.
    AddRepository {
        workspace_id: WorkspaceId,
        path: PathBuf,
        alias: String,
    },
    ListRepositories {
        workspace_id: WorkspaceId,
    },
    /// Explicit command. Prepares isolated clones and records checkout metadata.
    /// Does not run the agent and does not modify the source working trees.
    MigrateLegacyThread {
        thread_id: ThreadId,
    },
    GetThreadCheckouts {
        thread_id: ThreadId,
    },
    /// Sent only by the `trama-daemon approval-hook` helper that Claude Code
    /// runs as its `PermissionRequest` hook. `token` identifies the agent
    /// session (from the hook's environment). The daemon answers once with
    /// [`DaemonMessage::ApprovalHookDecision`]; closing the connection first
    /// cancels the request. UI clients never send this.
    ApprovalHook {
        token: String,
        tool_name: String,
        tool_input: serde_json::Value,
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
    WorkspaceCreated(WorkspaceSnapshot),
    Repositories {
        workspace_id: WorkspaceId,
        repositories: Vec<RepositorySnapshot>,
    },
    RepositoryAdded {
        workspace_id: WorkspaceId,
        repository: RepositorySnapshot,
    },
    ThreadMigrated(ThreadSnapshot),
    /// Reply to [`ClientMessage::ApprovalHook`], sent only on that hook's
    /// connection. `message` explains a denial to the agent; `interrupt`
    /// asks the agent to stop the turn (set when the user interrupted).
    ApprovalHookDecision {
        decision: ApprovalDecision,
        message: Option<String>,
        interrupt: bool,
    },
    ThreadCheckouts {
        thread_id: ThreadId,
        checkouts: Vec<CheckoutSnapshot>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutSnapshot {
    pub repository_id: RepositoryId,
    pub title: String,
    pub source_path: PathBuf,
    pub checkout_path: Option<PathBuf>,
    pub status: CheckoutStatus,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutStatus {
    Ready,
    NeedsPreparation,
    Missing,
    Invalid,
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

/// One workspace membership. `title` is the membership alias when set.
/// `path` is the canonical local working tree, not a remote URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySnapshot {
    pub id: RepositoryId,
    pub title: String,
    pub path: PathBuf,
    pub default_enabled: bool,
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

    fn snapshot_repo() -> RepositorySnapshot {
        RepositorySnapshot {
            id: RepositoryId::from("01hrepo000000000000000000001"),
            title: "app".into(),
            path: std::path::PathBuf::from("/src/app"),
            default_enabled: true,
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
                workspace_id: wid.clone(),
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
                thread_id: tid.clone(),
                request_id: "req-1".into(),
                decision: ApprovalDecision::AllowOnce,
            },
            ClientMessage::CreateWorkspace {
                name: "LaFraternita".into(),
            },
            ClientMessage::AddRepository {
                workspace_id: wid,
                path: std::path::PathBuf::from("/src/app"),
                alias: "app".into(),
            },
            ClientMessage::ListRepositories {
                workspace_id: workspace_id(),
            },
            ClientMessage::MigrateLegacyThread { thread_id: tid },
            ClientMessage::ApprovalHook {
                token: "tok".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
            },
            ClientMessage::Shutdown,
        ];
        for msg in &kinds {
            roundtrip_eq(msg);
            roundtrip_json(msg);
        }
        assert_eq!(kinds.len(), 15);
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
            DaemonMessage::WorkspaceCreated(snapshot_ws()),
            DaemonMessage::Repositories {
                workspace_id: workspace_id(),
                repositories: vec![snapshot_repo()],
            },
            DaemonMessage::RepositoryAdded {
                workspace_id: workspace_id(),
                repository: snapshot_repo(),
            },
            DaemonMessage::ThreadMigrated(snapshot_th()),
            DaemonMessage::ApprovalHookDecision {
                decision: ApprovalDecision::Deny,
                message: Some("denied".into()),
                interrupt: false,
            },
        ];
        for msg in &kinds {
            roundtrip_json(msg);
        }
        assert_eq!(kinds.len(), 13);
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
            protocol_version: PROTOCOL_VERSION,
            client_name: "ui".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"type":"hello","payload":{{"protocol_version":{PROTOCOL_VERSION},"client_name":"ui"}}}}"#
            )
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
    fn onboarding_and_migration_tags_keep_existing_shape() {
        let workspace = workspace_id();
        let repo = snapshot_repo();
        assert_eq!(
            serde_json::to_string(&ClientMessage::CreateWorkspace {
                name: "Demo".into(),
            })
            .unwrap(),
            r#"{"type":"create_workspace","payload":{"name":"Demo"}}"#
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::AddRepository {
                workspace_id: workspace.clone(),
                path: std::path::PathBuf::from("/src/app"),
                alias: "app".into(),
            })
            .unwrap(),
            format!(
                r#"{{"type":"add_repository","payload":{{"workspace_id":"{workspace}","path":"/src/app","alias":"app"}}}}"#
            )
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::ListRepositories {
                workspace_id: workspace.clone(),
            })
            .unwrap(),
            format!(r#"{{"type":"list_repositories","payload":{{"workspace_id":"{workspace}"}}}}"#)
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::MigrateLegacyThread {
                thread_id: thread_id(),
            })
            .unwrap(),
            format!(
                r#"{{"type":"migrate_legacy_thread","payload":{{"thread_id":"{}"}}}}"#,
                thread_id()
            )
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::WorkspaceCreated(snapshot_ws())).unwrap(),
            format!(
                r#"{{"type":"workspace_created","payload":{{"id":"{workspace}","title":"LaFraternita","repo_count":2}}}}"#
            )
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::Repositories {
                workspace_id: workspace.clone(),
                repositories: vec![repo.clone()],
            })
            .unwrap(),
            format!(
                r#"{{"type":"repositories","payload":{{"workspace_id":"{workspace}","repositories":[{{"id":"{}","title":"app","path":"/src/app","default_enabled":true}}]}}}}"#,
                repo.id
            )
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::RepositoryAdded {
                workspace_id: workspace.clone(),
                repository: repo.clone(),
            })
            .unwrap(),
            format!(
                r#"{{"type":"repository_added","payload":{{"workspace_id":"{workspace}","repository":{{"id":"{}","title":"app","path":"/src/app","default_enabled":true}}}}}}"#,
                repo.id
            )
        );
        let thread = thread_id();
        assert_eq!(
            serde_json::to_string(&DaemonMessage::ThreadMigrated(snapshot_th())).unwrap(),
            format!(
                r#"{{"type":"thread_migrated","payload":{{"id":"{thread}","title":"Fix auth","repo_count":1}}}}"#
            )
        );
        roundtrip_eq(&repo);
    }

    #[test]
    fn approval_channel_json_shape() {
        assert_eq!(
            serde_json::to_string(&ClientMessage::ApprovalHook {
                token: "tok".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
            })
            .unwrap(),
            r#"{"type":"approval_hook","payload":{"token":"tok","tool_name":"Bash","tool_input":{"command":"ls"}}}"#
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::ApprovalHookDecision {
                decision: ApprovalDecision::AllowSession,
                message: None,
                interrupt: false,
            })
            .unwrap(),
            r#"{"type":"approval_hook_decision","payload":{"decision":"allow_session","message":null,"interrupt":false}}"#
        );
        let envelope = DaemonMessage::AgentEventEnvelope {
            thread_id: thread_id(),
            event: AgentEvent::ApprovalRequested {
                request_id: "apr-1".into(),
                tool_name: "Bash".into(),
                input_json: serde_json::json!({"command": "ls"}),
            },
        };
        roundtrip_json(&envelope);
    }

    #[test]
    fn protocol_version_is_four() {
        assert_eq!(PROTOCOL_VERSION, 4);
    }

    #[test]
    fn thread_checkout_discovery_contract_uses_snake_case_status() {
        let message = DaemonMessage::ThreadCheckouts {
            thread_id: ThreadId::from("thread-1"),
            checkouts: vec![CheckoutSnapshot {
                repository_id: RepositoryId::from("repo-1"),
                title: "backend".into(),
                source_path: PathBuf::from("/src/backend"),
                checkout_path: Some(PathBuf::from("/src/backend/.trama/worktrees/t/c")),
                status: CheckoutStatus::Ready,
                detail: None,
            }],
        };
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["type"], "thread_checkouts");
        assert_eq!(json["payload"]["checkouts"][0]["status"], "ready");
        let decoded = serde_json::from_value::<DaemonMessage>(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), json);
    }
}
