//! Real Claude Code approval round trip. Ignored by default: it spends a tiny
//! amount of the logged-in user's Claude subscription.
//!
//! ```sh
//! cargo test -p trama-daemon --test claude_approval_smoke -- --ignored
//! ```
//!
//! Only `claude` uses the real HOME (for its login). The daemon side uses an
//! in-memory database, a throwaway Git repository, and a socket in a temp
//! directory, and the hook is this crate's `trama-daemon approval-hook`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;
use trama_daemon::SessionManager;
use trama_daemon::claude::approval::{APPROVAL_HOOK_SUBCOMMAND, ApprovalHookSettings};
use trama_db::TramaDb;
use trama_domain::agent::{AgentEvent, ApprovalDecision, BillingSource};
use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership};
use trama_protocol::{
    ClientMessage, DaemonMessage, PROTOCOL_VERSION, decode_json_line, encode_json_line,
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
async fn real_claude_asks_trama_before_running_bash() {
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
    let sessions = SessionManager::with_checkout_root(
        db,
        Arc::new(trama_infra::ClaudeAdapter::new().expect("claude on PATH")),
        dir.path().join("managed"),
    )
    .with_approval_hook(ApprovalHookSettings {
        command: PathBuf::from(env!("CARGO_BIN_EXE_trama-daemon")),
        args: vec![APPROVAL_HOOK_SUBCOMMAND.into()],
        socket_path: socket.clone(),
        timeout: Duration::from_secs(120),
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
        let line = timeout(Duration::from_secs(180), lines.next_line())
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
    let DaemonMessage::ThreadCreated(thread) = next().await else {
        panic!("thread")
    };
    send(
        &mut write,
        ClientMessage::SubscribeThread {
            thread_id: thread.id.clone(),
        },
    )
    .await;
    send(
        &mut write,
        ClientMessage::SendPrompt {
            thread_id: thread.id.clone(),
            text: "Use the Bash tool to run exactly: touch approved-by-trama.txt \
                   Then reply with the single word DONE."
                .into(),
            mentions: Vec::new(),
            terminals: Vec::new(),
        },
    )
    .await;

    let mut billing = None;
    let mut asked = false;
    loop {
        match next().await {
            DaemonMessage::AgentEventEnvelope { event, .. } => match event {
                AgentEvent::SessionInfo { billing: b, .. } => billing = Some(b),
                AgentEvent::ApprovalRequested {
                    request_id,
                    tool_name,
                    ..
                } => {
                    assert_eq!(tool_name, "Bash");
                    asked = true;
                    send(
                        &mut write,
                        ClientMessage::RespondApproval {
                            thread_id: thread.id.clone(),
                            request_id,
                            decision: ApprovalDecision::AllowOnce,
                        },
                    )
                    .await;
                }
                AgentEvent::TurnCompleted { .. } => break,
                AgentEvent::Failed { message } => panic!("turn failed: {message}"),
                _ => {}
            },
            DaemonMessage::Error { message, .. } => panic!("daemon error: {message}"),
            _ => {}
        }
    }
    assert!(asked, "Claude never asked for approval");
    assert_eq!(billing, Some(BillingSource::Subscription));
    let created = walk_for(&dir.path().join("managed"), "approved-by-trama.txt");
    assert!(
        created,
        "the approved command did not run in the managed checkout"
    );
}

fn walk_for(root: &Path, name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == name) {
            return true;
        }
        path.is_dir() && !path.ends_with(".git") && walk_for(&path, name)
    })
}
