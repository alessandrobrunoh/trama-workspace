//! Real Claude Code: a subagent through Trama's MCP tools, then a fork at a
//! message. Ignored by default: it spends a little of the logged-in user's
//! Claude subscription (haiku).
//!
//! ```sh
//! cargo test -p trama-daemon --test claude_subagent_smoke -- --ignored
//! ```
//!
//! Only `claude` uses the real HOME (for its login and its transcripts).
//! Trama uses an in-memory database, a throwaway Git repository and a
//! socket in a temp directory; the MCP helper and the approval hook are this
//! crate's `trama-daemon` binary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;
use trama_daemon::SessionManager;
use trama_daemon::approval::{APPROVAL_HOOK_SUBCOMMAND, ApprovalHookSettings};
use trama_daemon::mcp::{MCP_SUBCOMMAND, McpSettings};
use trama_db::TramaDb;
use trama_domain::agent::AgentEvent;
use trama_domain::message::Role;
use trama_domain::subagent::{SubagentProfile, SubagentStatus};
use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership};
use trama_protocol::{
    ClientMessage, DaemonMessage, PROTOCOL_VERSION, SubthreadKind, decode_json_line,
    encode_json_line,
};

fn git(cwd: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Trama Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Trama Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[tokio::test]
#[ignore = "runs the real `claude` CLI against the user's subscription"]
async fn real_claude_spawns_a_trama_subagent_and_forks_at_a_message() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("repo");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    std::fs::write(source.join("README.md"), "smoke\n").unwrap();
    git(&source, &["add", "README.md"]);
    git(&source, &["commit", "-m", "initial"]);

    let db = TramaDb::open_in_memory().unwrap();
    let mut workspace = Workspace::new("Smoke");
    workspace.settings.default_model = Some("haiku".into());
    db.create_workspace(&workspace).await.unwrap();
    let repo = Repository::new(None, Some(source), Some("main".into()));
    db.create_repository(&repo).await.unwrap();
    db.add_membership(&WorkspaceMembership {
        workspace_id: workspace.id.clone(),
        repository_id: repo.id,
        alias: Some("repo".into()),
        default_enabled: true,
    })
    .await
    .unwrap();

    let socket = dir.path().join("daemon.sock");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_trama-daemon"));
    let sessions = SessionManager::with_checkout_root(
        db,
        Arc::new(trama_infra::ClaudeAdapter::new().expect("claude on PATH")),
        dir.path().join("managed"),
    )
    .with_approval_hook(ApprovalHookSettings {
        command: binary.clone(),
        args: vec![APPROVAL_HOOK_SUBCOMMAND.into()],
        socket_path: socket.clone(),
        timeout: Duration::from_secs(60),
    })
    .with_mcp(McpSettings {
        command: binary,
        args: vec![MCP_SUBCOMMAND.into()],
        socket_path: socket.clone(),
    });
    let server_socket = socket.clone();
    tokio::spawn(async move {
        let _ = trama_daemon::serve_on_socket(&server_socket, sessions).await;
    });
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let stream = trama_ipc::connect(&socket).await.unwrap();
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    async fn send(write: &mut tokio::io::WriteHalf<trama_ipc::IpcStream>, msg: ClientMessage) {
        write
            .write_all(&encode_json_line(&msg).unwrap())
            .await
            .unwrap();
    }
    let mut next = async || -> DaemonMessage {
        let line = timeout(Duration::from_secs(300), lines.next_line())
            .await
            .expect("daemon reply")
            .unwrap()
            .expect("open");
        decode_json_line(&line).unwrap()
    };

    send(
        &mut write,
        ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "smoke".into(),
        },
    )
    .await;
    assert!(matches!(
        next().await,
        DaemonMessage::Welcome { accepted: true, .. }
    ));
    send(
        &mut write,
        ClientMessage::CreateThread {
            workspace_id: workspace.id.clone(),
            title: "smoke".into(),
        },
    )
    .await;
    let DaemonMessage::ThreadCreated(parent) = next().await else {
        panic!("thread")
    };
    send(
        &mut write,
        ClientMessage::SubscribeThread {
            thread_id: parent.id.clone(),
        },
    )
    .await;
    send(
        &mut write,
        ClientMessage::SendPrompt {
            thread_id: parent.id.clone(),
            text: "Call the spawn_subagent tool once with title \"smoke scout\", profile \
                   \"scout\", wait true, and prompt \"Reply with the single word PONG.\". \
                   Then reply with the single word DONE."
                .into(),
            mentions: Vec::new(),
            terminals: Vec::new(),
        },
    )
    .await;

    let mut tools = Vec::new();
    let mut statuses = Vec::new();
    let answer = loop {
        match next().await {
            DaemonMessage::AgentEventEnvelope { thread_id, event } if thread_id == parent.id => {
                match event {
                    AgentEvent::ToolCallStarted { name, .. } => tools.push(name),
                    AgentEvent::TurnCompleted { .. } => {}
                    AgentEvent::Failed { message } => panic!("turn failed: {message}"),
                    _ => {}
                }
            }
            DaemonMessage::SubagentStatusChanged { status, .. } => statuses.push(status),
            DaemonMessage::MessageAppended { thread_id, message }
                if thread_id == parent.id && message.role == Role::Assistant =>
            {
                break message;
            }
            DaemonMessage::Error { message, .. } => panic!("daemon error: {message}"),
            _ => {}
        }
    };
    assert!(
        tools.iter().any(|name| name.ends_with("spawn_subagent")),
        "Claude did not call spawn_subagent: {tools:?}"
    );
    assert!(
        !tools.iter().any(|name| name == "Agent" || name == "Task"),
        "the built-in subagent tool ran: {tools:?}"
    );
    assert!(
        statuses.contains(&SubagentStatus::Completed),
        "{statuses:?}"
    );
    send(
        &mut write,
        ClientMessage::ListSubthreads {
            parent_thread_id: parent.id.clone(),
        },
    )
    .await;
    let children = loop {
        if let DaemonMessage::Subthreads { threads, .. } = next().await {
            break threads;
        }
    };
    assert_eq!(children.len(), 1);
    let subagent = children[0].subagent.as_ref().expect("a Trama subagent");
    assert_eq!(subagent.spawned_by.profile, SubagentProfile::Scout);

    // Fork at the parent's answer and continue the forked conversation.
    assert!(answer.claude_uuid.is_some(), "no transcript uuid recorded");
    send(
        &mut write,
        ClientMessage::ForkThread {
            thread_id: parent.id.clone(),
            at_message_id: Some(answer.id.clone()),
            title: Some("fork".into()),
            initial_prompt: None,
            kind: SubthreadKind::Plain,
        },
    )
    .await;
    let fork = loop {
        match next().await {
            DaemonMessage::SubthreadCreated(snapshot) if snapshot.title == "fork" => {
                break snapshot;
            }
            DaemonMessage::Error { message, .. } => panic!("fork failed: {message}"),
            _ => {}
        }
    };
    send(
        &mut write,
        ClientMessage::SubscribeThread {
            thread_id: fork.id.clone(),
        },
    )
    .await;
    send(
        &mut write,
        ClientMessage::SendPrompt {
            thread_id: fork.id.clone(),
            text: "What was the title of the subagent you spawned? Reply with the title only."
                .into(),
            mentions: Vec::new(),
            terminals: Vec::new(),
        },
    )
    .await;
    loop {
        match next().await {
            DaemonMessage::AgentEventEnvelope {
                thread_id,
                event: AgentEvent::Failed { message },
            } if thread_id == fork.id => panic!("forked turn failed: {message}"),
            // Inherited parent messages are replayed first; skip them.
            DaemonMessage::MessageAppended { message, .. }
                if message.thread_id == fork.id && message.role == Role::Assistant =>
            {
                assert!(
                    message.text().to_lowercase().contains("smoke scout"),
                    "the fork lost the conversation: {}",
                    message.text()
                );
                break;
            }
            _ => {}
        }
    }
}
