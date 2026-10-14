//! Trama subagents through the MCP tool entry point, with the fake adapter.

use super::*;
use crate::claude::mcp::{self, McpSettings};
use crate::testing::{FakeAdapter, scripted_events};
use serde_json::json;
use std::path::Path;
use tokio::time::{Duration, timeout};
use trama_domain::message::MessageAnnotation;
use trama_domain::subagent::{SubagentProfile, SubagentStatus};
use trama_domain::subthread::PullFileOutcome;
use trama_domain::thread::ThreadKind;
use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership};

pub(super) fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Trama Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Trama Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

pub(super) struct Fixture {
    pub(super) dir: tempfile::TempDir,
    pub(super) workspace: WorkspaceId,
}

/// One workspace with one committed source repository.
pub(super) async fn fixture() -> (Fixture, TramaDb) {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open(&dir.path().join("trama.db")).unwrap();
    let workspace = Workspace::new("agents");
    db.create_workspace(&workspace).await.unwrap();
    let source = dir.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    std::fs::write(source.join("same.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(source.join("shared.txt"), "base\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "initial"]);
    let repo = Repository::new(None, Some(source), Some("main".into()));
    db.create_repository(&repo).await.unwrap();
    db.add_membership(&WorkspaceMembership {
        workspace_id: workspace.id.clone(),
        repository_id: repo.id,
        alias: Some("app".into()),
        default_enabled: true,
    })
    .await
    .unwrap();
    (
        Fixture {
            dir,
            workspace: workspace.id,
        },
        db,
    )
}

/// A manager with the Trama MCP server attached (the socket is never
/// dialed: tests call the tool entry point directly).
pub(super) fn manager(fixture: &Fixture, db: TramaDb, fake: &FakeAdapter) -> SessionManager {
    SessionManager::with_checkout_root(
        db,
        Arc::new(fake.clone()),
        fixture.dir.path().join("managed"),
    )
    .with_mcp(McpSettings {
        command: PathBuf::from("/usr/bin/false"),
        args: vec![mcp::MCP_SUBCOMMAND.into()],
        socket_path: fixture.dir.path().join("daemon.sock"),
    })
    .with_claude_projects_dir(fixture.dir.path().join("claude-projects"))
}

pub(super) async fn checkout(mgr: &SessionManager, thread_id: &ThreadId) -> PathBuf {
    let thread = mgr.db.get_thread(thread_id).await.unwrap().unwrap();
    mgr.resolve_checkouts(&thread).await.unwrap().remove(0).path
}

/// Run one prompt to completion.
pub(super) async fn run_prompt(mgr: &SessionManager, thread_id: &ThreadId, text: &str) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sub = mgr.subscribe(thread_id, tx).await.unwrap();
    while rx.try_recv().is_ok() {}
    mgr.send_prompt(thread_id.clone(), text.into())
        .await
        .unwrap();
    loop {
        let msg = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("turn event")
            .expect("channel open");
        match msg {
            DaemonMessage::UsageSnapshot { .. } => break,
            DaemonMessage::AgentEventEnvelope {
                event: AgentEvent::Failed { message },
                ..
            } => panic!("turn failed: {message}"),
            _ => {}
        }
    }
    mgr.unsubscribe(thread_id, sub).unwrap();
    settle(mgr, thread_id).await;
}

/// Wait until `thread_id` has no turn and no pending subagent work, then
/// let the turn-end bookkeeping (its own task) finish.
pub(super) async fn settle(mgr: &SessionManager, thread_id: &ThreadId) {
    timeout(
        Duration::from_secs(20),
        mgr.wait_for(std::slice::from_ref(thread_id), Duration::from_secs(20)),
    )
    .await
    .expect("settled");
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// A top-level thread with one completed turn (so it has a live session
/// and an MCP token).
pub(super) async fn live_thread(
    mgr: &SessionManager,
    fixture: &Fixture,
    title: &str,
) -> (ThreadId, String) {
    let thread = mgr
        .create_thread(fixture.workspace.clone(), title.into())
        .await
        .unwrap();
    run_prompt(mgr, &thread.id, "start").await;
    let token = mgr.mcp_token(&thread.id).expect("token for a live session");
    (thread.id, token)
}

async fn call(mgr: &SessionManager, token: &str, tool: &str, args: serde_json::Value) -> String {
    let (is_error, text) = mgr
        .mcp_tool_call(token, tool, args, Some("toolu_test".into()))
        .await;
    assert!(!is_error, "{tool} failed: {text}");
    text
}

async fn call_err(
    mgr: &SessionManager,
    token: &str,
    tool: &str,
    args: serde_json::Value,
) -> String {
    let (is_error, text) = mgr.mcp_tool_call(token, tool, args, None).await;
    assert!(is_error, "{tool} unexpectedly succeeded: {text}");
    text
}

/// The id a `spawn_subagent` (wait=false) result names.
fn spawned_id(text: &str) -> ThreadId {
    let start = text.find("thread ").expect("thread id") + "thread ".len();
    ThreadId::from(text[start..].split_whitespace().next().unwrap())
}

async fn children(mgr: &SessionManager, parent: &ThreadId) -> Vec<ThreadSnapshot> {
    mgr.list_subthreads(parent).await.unwrap()
}

async fn status(mgr: &SessionManager, thread_id: &ThreadId) -> SubagentStatus {
    mgr.subagent_snapshot(thread_id)
        .await
        .unwrap()
        .expect("subagent")
        .status
}

pub(super) fn configs(fake: &FakeAdapter) -> Vec<SessionConfig> {
    fake.start_configs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn config_for(fake: &FakeAdapter, cwd: &Path) -> SessionConfig {
    configs(fake)
        .into_iter()
        .rev()
        .find(|config| same_directory(&config.cwd, cwd))
        .expect("a session started in that checkout")
}

fn prompts(fake: &FakeAdapter) -> Vec<String> {
    fake.prompts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

pub(super) fn fast_fake() -> FakeAdapter {
    FakeAdapter::new(scripted_events(), Duration::from_millis(1))
}

#[tokio::test]
async fn spawned_subagents_are_child_threads_with_profile_semantics() {
    let (fixture, db) = fixture().await;
    let fake = fast_fake();
    let mgr = manager(&fixture, db, &fake);
    let (parent, token) = live_thread(&mgr, &fixture, "main").await;
    // Every Trama session gets the tools and loses the built-in subagent.
    let parent_config = configs(&fake).remove(0);
    assert!(
        parent_config
            .allowed_tools
            .contains(&mcp::ALLOW_RULE.to_owned())
    );
    for tool in mcp::BUILTIN_SUBAGENT_TOOLS {
        assert!(parent_config.disallowed_tools.contains(&tool.to_owned()));
    }
    assert!(
        parent_config
            .append_system_prompt
            .as_deref()
            .unwrap()
            .contains("spawn_subagent")
    );
    let mcp_file = parent_config.mcp_config_path.clone().unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp_file).unwrap()).unwrap();
    assert_eq!(
        written["mcpServers"][mcp::SERVER_NAME]["env"][mcp::ENV_MCP_TOKEN],
        token.as_str()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&mcp_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let parent_path = checkout(&mgr, &parent).await;
    std::fs::write(parent_path.join("same.txt"), "uncommitted parent edit\n").unwrap();

    // Worker: its own copy of the parent's current state, fresh conversation.
    let text = call(
        &mgr,
        &token,
        mcp::TOOL_SPAWN,
        json!({"title": "Worker A", "prompt": "Implement the thing", "profile": "worker"}),
    )
    .await;
    let worker = spawned_id(&text);
    settle(&mgr, &worker).await;
    let worker_snapshot = children(&mgr, &parent)
        .await
        .into_iter()
        .find(|t| t.id == worker)
        .unwrap();
    assert_eq!(worker_snapshot.kind, ThreadKind::Sub);
    assert_eq!(worker_snapshot.depth, 1);
    let subagent = worker_snapshot.subagent.unwrap();
    assert_eq!(subagent.spawned_by.parent_thread_id, parent);
    assert_eq!(subagent.spawned_by.profile, SubagentProfile::Worker);
    assert_eq!(
        subagent.spawned_by.tool_use_id.as_deref(),
        Some("toolu_test")
    );
    assert_eq!(subagent.status, SubagentStatus::Completed);
    let worker_path = checkout(&mgr, &worker).await;
    assert_ne!(worker_path, parent_path);
    assert_eq!(
        std::fs::read_to_string(worker_path.join("same.txt")).unwrap(),
        "uncommitted parent edit\n"
    );
    let worker_config = config_for(&fake, &worker_path);
    assert!(worker_config.resume_external_session.is_none());
    assert!(!worker_config.fork_session);
    assert!(!worker_config.disallowed_tools.contains(&"Edit".to_owned()));
    let worker_prompt = prompts(&fake)
        .into_iter()
        .find(|p| p.contains("Implement the thing"))
        .unwrap();
    assert!(worker_prompt.contains("worker subagent"));
    // The task prompt is attributed to the parent's agent.
    let worker_messages = mgr.db.list_messages_by_thread(&worker).await.unwrap();
    assert!(matches!(worker_messages[0].actor, Actor::Agent { .. }));
    assert!(matches!(
        &worker_messages[0].annotation,
        Some(MessageAnnotation::AgentMessageReceived { from_thread_id, .. }) if from_thread_id == &parent
    ));

    // Scout: works directly in the parent's checkout, no edit tools.
    let text = call(
        &mgr,
        &token,
        mcp::TOOL_SPAWN,
        json!({"title": "Look around", "prompt": "Where is X?", "profile": "scout", "wait": true}),
    )
    .await;
    assert!(text.contains("completed"), "{text}");
    assert!(text.contains("Hello from the mock agent."), "{text}");
    let scout = children(&mgr, &parent)
        .await
        .into_iter()
        .find(|t| t.title == "Look around")
        .unwrap();
    assert_eq!(scout.kind, ThreadKind::Sub);
    assert_eq!(checkout(&mgr, &scout.id).await, parent_path);
    let scout_config = config_for(&fake, &parent_path);
    for tool in ["Edit", "Write", "NotebookEdit"] {
        assert!(scout_config.disallowed_tools.contains(&tool.to_owned()));
    }
    assert_eq!(
        mgr.thread_checkouts(&scout.id).await.unwrap()[0]
            .checkout_path
            .as_deref(),
        Some(parent_path.as_path())
    );

    // Reviewer: a review child on a copy, with the change-guide prompt.
    let text = call(
        &mgr,
        &token,
        mcp::TOOL_SPAWN,
        json!({"title": "Review", "prompt": "Focus on errors", "profile": "reviewer", "wait": true}),
    )
    .await;
    assert!(text.contains("completed"), "{text}");
    let review = children(&mgr, &parent)
        .await
        .into_iter()
        .find(|t| t.kind == ThreadKind::Review)
        .unwrap();
    assert_ne!(checkout(&mgr, &review.id).await, parent_path);
    assert!(
        prompts(&fake)
            .iter()
            .any(|p| p.contains("change guide") && p.contains("Focus on errors"))
    );

    // The parent transcript links every child.
    let rows = mgr.db.list_messages_by_thread(&parent).await.unwrap();
    let spawned: Vec<_> = rows
        .iter()
        .filter_map(|m| match &m.annotation {
            Some(MessageAnnotation::SubagentSpawned {
                child_thread_id, ..
            }) => Some(child_thread_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(spawned.len(), 3);
    assert!(spawned.contains(&worker) && spawned.contains(&scout.id));
    assert!(rows.iter().any(|m| matches!(
        &m.annotation,
        Some(MessageAnnotation::SubagentFinished { child_thread_id, status: SubagentStatus::Completed, .. })
            if child_thread_id == &review.id
    )));
    // Bad arguments are reported to the agent.
    call_err(&mgr, &token, mcp::TOOL_SPAWN, json!({"title": "x"})).await;
    call_err(
        &mgr,
        &token,
        mcp::TOOL_SPAWN,
        json!({"title": "x", "prompt": "y", "profile": "boss"}),
    )
    .await;
    call_err(&mgr, &token, "rm_rf", json!({})).await;
}

#[tokio::test]
async fn worker_changes_are_pulled_on_success_and_conflicts_reported() {
    let (fixture, db) = fixture().await;
    // The agent "edits" its checkout when it receives a prompt. Turns last
    // long enough for a spawn call to return before its worker finishes.
    let fake = FakeAdapter::new(scripted_events(), Duration::from_millis(30)).with_on_send(
        |cwd, prompt| {
            if prompt.contains("TASK-CLEAN") {
                std::fs::write(cwd.join("worker.txt"), "from worker\n").unwrap();
            }
            if prompt.contains("TASK-CONFLICT") {
                std::fs::write(cwd.join("shared.txt"), "worker version\n").unwrap();
            }
        },
    );
    let mgr = manager(&fixture, db, &fake);
    let (parent, token) = live_thread(&mgr, &fixture, "main").await;
    let parent_path = checkout(&mgr, &parent).await;
    let parent_head = git(&parent_path, &["rev-parse", "HEAD"]);

    // Parent idle: merged as soon as the worker finishes; a waiting spawn
    // returns after the merge.
    let text = call(
        &mgr,
        &token,
        mcp::TOOL_SPAWN,
        json!({"title": "Clean", "prompt": "TASK-CLEAN", "wait": true}),
    )
    .await;
    assert!(text.contains("completed"), "{text}");
    assert!(text.contains("merged into the parent"), "{text}");
    assert_eq!(
        std::fs::read_to_string(parent_path.join("worker.txt")).unwrap(),
        "from worker\n"
    );
    assert_eq!(git(&parent_path, &["rev-parse", "HEAD"]), parent_head);

    // Parent mid-turn and not waiting: the worker waits to sync.
    mgr.lock().threads.get_mut(&parent).unwrap().reservation = Some(Reservation::Turn);
    let text = call(
        &mgr,
        &token,
        mcp::TOOL_SPAWN,
        json!({"title": "Conflicting", "prompt": "TASK-CONFLICT"}),
    )
    .await;
    let worker = spawned_id(&text);
    timeout(Duration::from_secs(10), async {
        while status(&mgr, &worker).await != SubagentStatus::WaitingToSync {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("waiting to sync");
    std::fs::write(parent_path.join("shared.txt"), "parent version\n").unwrap();
    // The parent's agent blocks in a Trama tool: the worker merges now.
    let text = call(
        &mgr,
        &token,
        mcp::TOOL_WAIT,
        json!({"ids": [worker.to_string()], "timeout": 10}),
    )
    .await;
    assert!(text.contains("conflicts in: shared.txt"), "{text}");
    assert_eq!(status(&mgr, &worker).await, SubagentStatus::Completed);
    assert!(
        std::fs::read_to_string(parent_path.join("shared.txt"))
            .unwrap()
            .contains("<<<<<<<")
    );
    let events = mgr
        .db
        .for_aggregate(&Aggregate::Thread { id: parent.clone() }, None, 500)
        .await
        .unwrap();
    let pull = events
        .iter()
        .rev()
        .find(|e| e.kind == "changes.pulled")
        .and_then(|e| e.decode_domain().unwrap());
    let Some(DomainEvent::ChangesPulled { repos, .. }) = pull else {
        panic!("no pull event");
    };
    assert!(matches!(
        repos[0]
            .files
            .iter()
            .find(|f| f.path == "shared.txt")
            .unwrap()
            .outcome,
        PullFileOutcome::Conflict { .. }
    ));
    assert_eq!(git(&parent_path, &["rev-parse", "HEAD"]), parent_head);
    mgr.lock().threads.get_mut(&parent).unwrap().reservation = None;
}

#[tokio::test]
async fn limits_queue_subagents_per_thread_and_in_total() {
    let (fixture, db) = fixture().await;
    // Subagent turns ("work") are held until the test releases them.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let fake = fast_fake().with_gate("work", Arc::clone(&gate));
    let mgr = manager(&fixture, db, &fake).with_subagent_settings(SubagentSettings {
        max_running_per_thread: 2,
        max_running_total: 3,
        allow_builtin_subagents: false,
    });
    let (a, token_a) = live_thread(&mgr, &fixture, "a").await;
    let (_b, token_b) = live_thread(&mgr, &fixture, "b").await;
    let mut spawned = Vec::new();
    for (token, title) in [
        (&token_a, "a0"),
        (&token_a, "a1"),
        (&token_a, "a2"),
        (&token_b, "b0"),
        (&token_b, "b1"),
    ] {
        let text = call(
            &mgr,
            token,
            mcp::TOOL_SPAWN,
            json!({"title": title, "prompt": "work", "profile": "scout"}),
        )
        .await;
        spawned.push(spawned_id(&text));
    }
    // Per thread: two of a's run and the third waits; in total three run.
    let mut statuses = Vec::new();
    for id in &spawned {
        statuses.push(status(&mgr, id).await);
    }
    assert_eq!(
        statuses,
        [
            SubagentStatus::Running,
            SubagentStatus::Running,
            SubagentStatus::Queued,
            SubagentStatus::Running,
            SubagentStatus::Queued,
        ]
    );
    let listed = call(&mgr, &token_a, mcp::TOOL_LIST, json!({})).await;
    assert!(listed.contains("status=queued"), "{listed}");
    gate.add_permits(spawned.len());
    // A thread blocked in a wait does not hold a slot, so everything runs.
    let ids: Vec<String> = spawned.iter().map(ToString::to_string).collect();
    let text = call(
        &mgr,
        &token_a,
        mcp::TOOL_WAIT,
        json!({"ids": ids, "timeout": 60}),
    )
    .await;
    assert!(!text.contains("Timed out"), "{text}");
    for id in &spawned {
        assert_eq!(status(&mgr, id).await, SubagentStatus::Completed);
    }
    // A wait on a thread that stays busy times out and says so.
    let busy = spawned[2].clone();
    mgr.lock().threads.get_mut(&busy).unwrap().reservation = Some(Reservation::Turn);
    let text = call(
        &mgr,
        &token_a,
        mcp::TOOL_WAIT,
        json!({"ids": [busy.to_string()], "timeout": 1}),
    )
    .await;
    assert!(text.starts_with("Timed out"), "{text}");
    mgr.lock().threads.get_mut(&busy).unwrap().reservation = None;
    assert_eq!(
        mgr.db
            .get_thread(&busy)
            .await
            .unwrap()
            .unwrap()
            .parent_thread_id,
        Some(a)
    );
}

#[tokio::test]
async fn agent_messages_appear_in_both_transcripts() {
    let (fixture, db) = fixture().await;
    let fake = FakeAdapter::new(scripted_events(), Duration::from_millis(30));
    let mgr = manager(&fixture, db, &fake);
    let (parent, token) = live_thread(&mgr, &fixture, "main").await;
    let text = call(
        &mgr,
        &token,
        mcp::TOOL_SPAWN,
        json!({"title": "Helper", "prompt": "help", "profile": "scout", "wait": true}),
    )
    .await;
    assert!(text.contains("completed"));
    let child = children(&mgr, &parent).await.remove(0).id;

    let text = call(
        &mgr,
        &token,
        mcp::TOOL_SEND,
        json!({"thread_id": child.to_string(), "message": "Please also check Y"}),
    )
    .await;
    assert!(text.contains("Delivered"), "{text}");
    // Busy now: the next message waits for the running turn to end.
    let text = call(
        &mgr,
        &token,
        mcp::TOOL_SEND,
        json!({"thread_id": child.to_string(), "message": "And Z"}),
    )
    .await;
    assert!(text.contains("busy"), "{text}");
    timeout(Duration::from_secs(20), async {
        loop {
            let answered = mgr
                .db
                .list_messages_by_thread(&child)
                .await
                .unwrap()
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .count();
            if answered == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both messages answered");
    settle(&mgr, &child).await;

    let sent: Vec<_> = mgr
        .db
        .list_messages_by_thread(&parent)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| {
            matches!(&m.annotation, Some(MessageAnnotation::AgentMessageSent { to_thread_id, .. }) if to_thread_id == &child)
        })
        .collect();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].text(), "Please also check Y");
    assert!(matches!(sent[0].actor, Actor::Agent { .. }));
    let received: Vec<_> = mgr
        .db
        .list_messages_by_thread(&child)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| {
            m.role == Role::User
                && matches!(&m.annotation, Some(MessageAnnotation::AgentMessageReceived { from_thread_id, .. }) if from_thread_id == &parent)
        })
        .collect();
    // The spawn prompt, then both messages in order.
    assert_eq!(received.len(), 3);
    assert!(received[1].text().ends_with("Please also check Y"));
    assert!(received[2].text().ends_with("And Z"));
    assert!(received[1].text().contains(&parent.to_string()));
    let events = mgr
        .db
        .for_aggregate(&Aggregate::Thread { id: parent.clone() }, None, 500)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == "agent.message_sent")
            .count(),
        2
    );
    // The child can answer its parent.
    let child_token = mgr.mcp_token(&child).unwrap();
    let text = call(
        &mgr,
        &child_token,
        mcp::TOOL_SEND,
        json!({"thread_id": parent.to_string(), "message": "Done"}),
    )
    .await;
    assert!(text.contains("Delivered"), "{text}");
    settle(&mgr, &parent).await;
    assert!(
        mgr.db
            .list_messages_by_thread(&parent)
            .await
            .unwrap()
            .iter()
            .any(|m| m.role == Role::User && m.text().ends_with("Done"))
    );
    // Not to itself.
    call_err(
        &mgr,
        &token,
        mcp::TOOL_SEND,
        json!({"thread_id": parent.to_string(), "message": "me"}),
    )
    .await;
    // Not to a shared thread: its turns go through the collab log.
    mgr.db
        .link_collab_thread(&parent, "http://127.0.0.1:1", "collab-thread")
        .await
        .unwrap();
    let text = call_err(
        &mgr,
        &child_token,
        mcp::TOOL_SEND,
        json!({"thread_id": parent.to_string(), "message": "Again"}),
    )
    .await;
    assert!(text.contains("shared"), "{text}");
}

#[tokio::test]
async fn subagents_nest_three_deep_and_tokens_are_isolated() {
    let (fixture, db) = fixture().await;
    let fake = fast_fake();
    let mgr = manager(&fixture, db.clone(), &fake);
    let (top, token) = live_thread(&mgr, &fixture, "top").await;
    let mut parent_token = token.clone();
    let mut chain = vec![top.clone()];
    for depth in 1..=3u32 {
        let text = call(
            &mgr,
            &parent_token,
            mcp::TOOL_SPAWN,
            json!({"title": format!("level {depth}"), "prompt": "go deeper", "wait": true}),
        )
        .await;
        assert!(text.contains("completed"), "{text}");
        let child = children(&mgr, chain.last().unwrap()).await.remove(0);
        assert_eq!(child.depth, depth);
        assert_eq!(child.parent_thread_id.as_ref(), chain.last());
        parent_token = mgr.mcp_token(&child.id).expect("child has a live session");
        chain.push(child.id);
    }
    let all = mgr.list_threads(&fixture.workspace).await.unwrap();
    assert_eq!(all.iter().find(|t| t.id == chain[3]).unwrap().depth, 3);
    let listed = call(&mgr, &token, mcp::TOOL_LIST, json!({})).await;
    assert!(listed.contains("depth=3"), "{listed}");
    assert!(listed.contains("(you)"));
    for pair in chain.windows(2) {
        assert_eq!(status(&mgr, &pair[1]).await, SubagentStatus::Completed);
    }

    // Tokens are capabilities of one session's thread.
    call_err(&mgr, "not-a-token", mcp::TOOL_LIST, json!({})).await;
    // A spawn always lands under the caller's own thread.
    let deep_token = mgr.mcp_token(&chain[2]).unwrap();
    let text = call(
        &mgr,
        &deep_token,
        mcp::TOOL_SPAWN,
        json!({"title": "mine", "prompt": "p", "profile": "scout"}),
    )
    .await;
    let mine = spawned_id(&text);
    settle(&mgr, &mine).await;
    assert_eq!(
        mgr.db
            .get_thread(&mine)
            .await
            .unwrap()
            .unwrap()
            .parent_thread_id,
        Some(chain[2].clone())
    );
    // Threads of another workspace are invisible.
    let other = Workspace::new("other");
    db.create_workspace(&other).await.unwrap();
    let mut foreign = Thread::new_for_workspace(&other, &[], Some("foreign".into()));
    foreign.repo_attachments.clear();
    db.create_thread(&foreign).await.unwrap();
    for (tool, args) in [
        (
            mcp::TOOL_STATUS,
            json!({"thread_id": foreign.id.to_string()}),
        ),
        (
            mcp::TOOL_SEND,
            json!({"thread_id": foreign.id.to_string(), "message": "hi"}),
        ),
        (mcp::TOOL_WAIT, json!({"ids": [foreign.id.to_string()]})),
    ] {
        let text = call_err(&mgr, &token, tool, args).await;
        assert!(text.contains("not in this workspace"), "{text}");
    }
    // When the session ends, its token stops working.
    fake.close_all_event_streams();
    timeout(Duration::from_secs(5), async {
        while mgr.mcp_token(&top).is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("token revoked");
    let text = call_err(&mgr, &token, mcp::TOOL_LIST, json!({})).await;
    assert!(text.contains("does not belong"), "{text}");
}

#[tokio::test]
async fn builtin_subagent_tool_can_be_re_enabled_and_needs_the_server() {
    let (fixture, db) = fixture().await;
    let fake = fast_fake();
    let mgr = manager(&fixture, db, &fake).with_subagent_settings(SubagentSettings {
        allow_builtin_subagents: true,
        ..SubagentSettings::default()
    });
    live_thread(&mgr, &fixture, "builtin").await;
    let config = configs(&fake).remove(0);
    assert!(!config.disallowed_tools.iter().any(|t| t == "Agent"));
    assert!(config.mcp_config_path.is_some());

    // Without the server, sessions are left as they were.
    let (fixture, db) = super::agents_tests::fixture().await;
    let fake = fast_fake();
    let plain = SessionManager::with_checkout_root(
        db,
        Arc::new(fake.clone()),
        fixture.dir.path().join("managed"),
    );
    let thread = plain
        .create_thread(fixture.workspace.clone(), "plain".into())
        .await
        .unwrap();
    run_prompt(&plain, &thread.id, "hi").await;
    let config = configs(&fake).remove(0);
    assert!(config.mcp_config_path.is_none());
    assert!(config.disallowed_tools.is_empty());
    assert!(config.append_system_prompt.is_none());
    assert!(plain.mcp_token(&thread.id).is_none());
}
