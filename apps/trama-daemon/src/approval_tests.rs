//! Approval broker end to end: a fake adapter session, the real connection
//! handler, and the real hook client (`approval::request_over`).

use crate::approval::{self, ApprovalHookSettings, HookDecision};
use crate::connection::handle_connection;
use crate::session::SessionManager;
use crate::testing::FakeAdapter;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use trama_db::TramaDb;
use trama_domain::agent::{AgentEvent, ApprovalDecision, BillingMode};
use trama_domain::event::{Aggregate, DomainEvent, EventStore};
use trama_domain::ids::ThreadId;
use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership};
use trama_protocol::DaemonMessage;

struct Harness {
    mgr: SessionManager,
    db: TramaDb,
    fake: FakeAdapter,
    thread: ThreadId,
    token: String,
    events: mpsc::UnboundedReceiver<DaemonMessage>,
    _dir: tempfile::TempDir,
}

fn git(cwd: &std::path::Path, args: &[&str]) {
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

/// A thread with a live fake session (one completed turn) and a subscriber.
async fn harness(approval_timeout: Duration) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let workspace = Workspace::new("Default");
    db.create_workspace(&workspace).await.unwrap();
    let source = dir.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    std::fs::write(source.join("same.txt"), "source\n").unwrap();
    git(&source, &["add", "same.txt"]);
    git(&source, &["commit", "-m", "initial"]);
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

    let fake = FakeAdapter::scripted();
    let mgr = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(fake.clone()),
        dir.path().join("managed"),
    )
    .with_approval_hook(ApprovalHookSettings {
        command: PathBuf::from("/opt/trama/trama-daemon"),
        args: vec![approval::APPROVAL_HOOK_SUBCOMMAND.into()],
        socket_path: dir.path().join("daemon.sock"),
        timeout: approval_timeout,
    });
    let thread = mgr
        .create_thread(workspace.id, "approvals".into())
        .await
        .unwrap()
        .id;
    let (tx, mut events) = mpsc::unbounded_channel();
    mgr.subscribe(&thread, tx).await.unwrap();
    mgr.send_prompt(thread.clone(), "hi".into()).await.unwrap();
    loop {
        let msg = timeout(Duration::from_secs(3), events.recv())
            .await
            .expect("turn")
            .expect("open");
        if matches!(msg, DaemonMessage::UsageSnapshot { .. }) {
            break;
        }
    }
    let token = fake.start_configs.lock().unwrap()[0]
        .env
        .get(approval::ENV_TOKEN)
        .cloned()
        .expect("token in child env");
    Harness {
        mgr,
        db,
        fake,
        thread,
        token,
        events,
        _dir: dir,
    }
}

/// What Claude Code's hook process does: connect, ask, wait.
fn hook(mgr: &SessionManager, token: &str, tool: &str) -> JoinHandle<HookDecision> {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let sessions = mgr.clone();
    let (shutdown, _) = watch::channel(false);
    tokio::spawn(async move {
        let _ = handle_connection(server, sessions, shutdown).await;
    });
    let token = token.to_string();
    let tool = tool.to_string();
    tokio::spawn(async move {
        approval::request_over(client, &token, &tool, serde_json::json!({"command": "ls"})).await
    })
}

async fn next_approval_event(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> AgentEvent {
    loop {
        let msg = timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("approval event")
            .expect("open");
        if let DaemonMessage::AgentEventEnvelope { event, .. } = msg {
            if matches!(
                event,
                AgentEvent::ApprovalRequested { .. } | AgentEvent::ApprovalResolved { .. }
            ) {
                return event;
            }
        }
    }
}

async fn requested_id(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> String {
    match next_approval_event(rx).await {
        AgentEvent::ApprovalRequested {
            request_id,
            tool_name,
            input_json,
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(input_json["command"], "ls");
            request_id
        }
        other => panic!("expected approval request, got {other:?}"),
    }
}

async fn decided(handle: JoinHandle<HookDecision>) -> HookDecision {
    timeout(Duration::from_secs(3), handle)
        .await
        .expect("hook answered")
        .unwrap()
}

async fn approval_kinds(db: &TramaDb, thread: &ThreadId) -> Vec<String> {
    let session = db
        .latest_agent_session_for_thread(thread)
        .await
        .unwrap()
        .expect("session row");
    EventStore::for_aggregate(db, &Aggregate::Session { id: session.id }, None, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.kind.starts_with("agent.approval_"))
        .map(|r| r.kind)
        .collect()
}

#[tokio::test]
async fn session_config_carries_hook_token_and_subscription_billing() {
    let h = harness(Duration::from_secs(60)).await;
    let config = h.fake.start_configs.lock().unwrap()[0].clone();
    assert_eq!(config.billing, BillingMode::Subscription);
    let hook = config.approval_hook.expect("hook configured");
    assert_eq!(hook.command, PathBuf::from("/opt/trama/trama-daemon"));
    assert_eq!(hook.args, vec!["approval-hook".to_string()]);
    assert!(hook.timeout_secs > 60, "hook outlives the daemon timeout");
    assert!(config.env.contains_key(approval::ENV_SOCKET));
}

#[tokio::test]
async fn allow_once_then_allow_session_is_remembered_per_tool() {
    let mut h = harness(Duration::from_secs(60)).await;

    let first = hook(&h.mgr, &h.token, "Bash");
    let id = requested_id(&mut h.events).await;
    h.mgr
        .respond_approval(&h.thread, &id, ApprovalDecision::AllowOnce)
        .await
        .unwrap();
    assert_eq!(
        decided(first).await,
        HookDecision::allow(ApprovalDecision::AllowOnce)
    );
    assert!(matches!(
        next_approval_event(&mut h.events).await,
        AgentEvent::ApprovalResolved {
            decision: ApprovalDecision::AllowOnce,
            reason: None,
            ..
        }
    ));

    // allow_once is not remembered.
    let second = hook(&h.mgr, &h.token, "Bash");
    let id = requested_id(&mut h.events).await;
    // A stale or foreign id is rejected and leaves the request pending.
    assert!(
        h.mgr
            .respond_approval(&h.thread, "apr-missing", ApprovalDecision::AllowOnce)
            .await
            .is_err()
    );
    h.mgr
        .respond_approval(&h.thread, &id, ApprovalDecision::AllowSession)
        .await
        .unwrap();
    assert!(decided(second).await.is_allow());
    let _ = next_approval_event(&mut h.events).await;

    // Remembered for this session: answered without asking anyone.
    let third = hook(&h.mgr, &h.token, "Bash");
    assert_eq!(
        decided(third).await,
        HookDecision::allow(ApprovalDecision::AllowSession)
    );
    // A different tool still asks.
    let other = hook(&h.mgr, &h.token, "Write");
    match next_approval_event(&mut h.events).await {
        AgentEvent::ApprovalRequested {
            tool_name,
            request_id,
            ..
        } => {
            assert_eq!(tool_name, "Write");
            h.mgr
                .respond_approval(&h.thread, &request_id, ApprovalDecision::Deny)
                .await
                .unwrap();
        }
        other => panic!("expected Write request, got {other:?}"),
    }
    let denied = decided(other).await;
    assert_eq!(denied.decision, ApprovalDecision::Deny);
    assert!(denied.message.unwrap().contains("denied"));

    assert_eq!(
        approval_kinds(&h.db, &h.thread).await,
        vec![
            "agent.approval_requested",
            "agent.approval_resolved",
            "agent.approval_requested",
            "agent.approval_resolved",
            "agent.approval_resolved",
            "agent.approval_requested",
            "agent.approval_resolved",
        ]
    );
}

#[tokio::test]
async fn interrupt_denies_pending_with_interrupt_flag() {
    let mut h = harness(Duration::from_secs(60)).await;
    let pending = hook(&h.mgr, &h.token, "Bash");
    let _ = requested_id(&mut h.events).await;
    h.mgr.interrupt(&h.thread).await.unwrap();
    let decision = decided(pending).await;
    assert_eq!(decision.decision, ApprovalDecision::Deny);
    assert!(decision.interrupt);
    assert!(matches!(
        next_approval_event(&mut h.events).await,
        AgentEvent::ApprovalResolved {
            decision: ApprovalDecision::Deny,
            reason: Some(_),
            ..
        }
    ));
}

#[tokio::test]
async fn timeout_denies_and_withdraws_the_request() {
    let mut h = harness(Duration::from_millis(150)).await;
    let pending = hook(&h.mgr, &h.token, "Bash");
    let id = requested_id(&mut h.events).await;
    let decision = decided(pending).await;
    assert_eq!(decision.decision, ApprovalDecision::Deny);
    assert!(decision.message.unwrap().contains("No decision"));
    match next_approval_event(&mut h.events).await {
        AgentEvent::ApprovalResolved {
            request_id, reason, ..
        } => {
            assert_eq!(request_id, id);
            assert!(reason.is_some());
        }
        other => panic!("{other:?}"),
    }
    // A late answer finds nothing to resolve.
    assert!(
        h.mgr
            .respond_approval(&h.thread, &id, ApprovalDecision::AllowOnce)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn hook_disconnect_withdraws_the_request() {
    let mut h = harness(Duration::from_secs(60)).await;
    let pending = hook(&h.mgr, &h.token, "Bash");
    let id = requested_id(&mut h.events).await;
    // Claude Code cancels a hook by killing it: the connection drops.
    pending.abort();
    match next_approval_event(&mut h.events).await {
        AgentEvent::ApprovalResolved {
            request_id,
            decision,
            ..
        } => {
            assert_eq!(request_id, id);
            assert_eq!(decision, ApprovalDecision::Deny);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn session_end_releases_blocked_hooks_and_invalidates_token() {
    let mut h = harness(Duration::from_secs(60)).await;
    let pending = hook(&h.mgr, &h.token, "Bash");
    let _ = requested_id(&mut h.events).await;
    h.fake.close_all_event_streams();
    let decision = decided(pending).await;
    assert_eq!(decision.decision, ApprovalDecision::Deny);
    assert!(decision.message.unwrap().contains("ended"));
    let after = decided(hook(&h.mgr, &h.token, "Bash")).await;
    assert_eq!(after.decision, ApprovalDecision::Deny);
}

#[tokio::test]
async fn unknown_token_is_denied_without_asking() {
    let h = harness(Duration::from_secs(60)).await;
    let decision = decided(hook(&h.mgr, "not-a-token", "Bash")).await;
    assert_eq!(decision.decision, ApprovalDecision::Deny);
    assert!(
        h.mgr
            .respond_approval(&h.thread, "x", ApprovalDecision::Deny)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn late_subscriber_sees_pending_request() {
    let mut h = harness(Duration::from_secs(60)).await;
    let pending = hook(&h.mgr, &h.token, "Bash");
    let id = requested_id(&mut h.events).await;
    let (tx, mut late) = mpsc::unbounded_channel();
    h.mgr.subscribe(&h.thread, tx).await.unwrap();
    assert_eq!(requested_id(&mut late).await, id);
    h.mgr
        .respond_approval(&h.thread, &id, ApprovalDecision::AllowOnce)
        .await
        .unwrap();
    assert!(decided(pending).await.is_allow());
}

#[tokio::test]
async fn approval_events_decode_from_the_store() {
    let mut h = harness(Duration::from_secs(60)).await;
    let pending = hook(&h.mgr, &h.token, "Bash");
    let id = requested_id(&mut h.events).await;
    h.mgr
        .respond_approval(&h.thread, &id, ApprovalDecision::Deny)
        .await
        .unwrap();
    let _ = decided(pending).await;
    let session =
        h.db.latest_agent_session_for_thread(&h.thread)
            .await
            .unwrap()
            .unwrap();
    let records =
        EventStore::for_aggregate(&h.db, &Aggregate::Session { id: session.id }, None, 100)
            .await
            .unwrap();
    let resolved = records
        .iter()
        .filter_map(|r| r.decode_domain().ok().flatten())
        .find_map(|event| match event {
            DomainEvent::ApprovalResolved {
                request_id,
                decision,
                tool_name,
                ..
            } => Some((request_id, decision, tool_name)),
            _ => None,
        })
        .expect("resolved event persisted");
    assert_eq!(resolved, (id, ApprovalDecision::Deny, "Bash".to_string()));
}
