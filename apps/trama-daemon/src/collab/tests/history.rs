//! Sharing a thread with its history and its whole family (ADR 0013): two
//! daemons (separate data dirs, fake adapters) against the real service.

use super::http::Endpoint;
use super::tests::{AGENT_TEXT, Ipc, Node, Server, WAIT, bare, eventually};
use super::*;
use crate::testing::FakeAdapter;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use trama_collab::{AuthConfig, AuthService, router};
use trama_collab_protocol::EventKind;
use trama_domain::activity::ActivityItem;
use trama_domain::agent::AgentEvent;
use trama_domain::comment::CommentTarget;
use trama_domain::message::{Actor, Message, MessageAnnotation, Role};
use trama_domain::subagent::{SubagentProfile, SubagentStatus};
use trama_domain::subthread::ReviewVerdict;
use trama_protocol::{ClientMessage, DaemonMessage, SubthreadKind};

/// A turn with thinking, an edit and a command, like a real agent's.
fn tool_events() -> Vec<AgentEvent> {
    vec![
        AgentEvent::SessionStarted {
            external_session_id: Some("fake-ext".into()),
            model: Some("fake".into()),
        },
        AgentEvent::ReasoningDelta {
            text: "Plan: edit, then test.".into(),
        },
        AgentEvent::ToolCallStarted {
            tool_use_id: "toolu_edit".into(),
            name: "Edit".into(),
            input_json: serde_json::json!({
                "file_path": "same.txt",
                "old_string": "source",
                "new_string": "edited",
            }),
        },
        AgentEvent::ToolCallFinished {
            tool_use_id: "toolu_edit".into(),
            output_json: serde_json::json!("The file was updated."),
            is_error: false,
        },
        AgentEvent::ToolCallStarted {
            tool_use_id: "toolu_bash".into(),
            name: "Bash".into(),
            input_json: serde_json::json!({"command": "cargo test"}),
        },
        AgentEvent::ToolCallFinished {
            tool_use_id: "toolu_bash".into(),
            output_json: serde_json::json!("test result: ok"),
            is_error: false,
        },
        AgentEvent::TextDelta {
            text: "Hello from ".into(),
        },
        AgentEvent::TextDelta {
            text: "the mock agent.".into(),
        },
        AgentEvent::TurnCompleted {
            num_turns: 1,
            cost_usd_micros: Some(0),
        },
    ]
}

/// Each turn writes a file named after its prompt, like an agent editing.
fn editing_adapter(who: &'static str) -> FakeAdapter {
    FakeAdapter::new(tool_events(), Duration::from_millis(5)).with_on_send(move |cwd, prompt| {
        let name: String = bare(prompt)
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(16)
            .collect();
        let _ = std::fs::write(
            cwd.join(format!("{who}-{name}.txt")),
            format!("written by {who}\n"),
        );
    })
}

fn remote(user: &str) -> Actor {
    Actor::Human {
        id: format!("collab:{user}"),
    }
}

/// (role, text, annotation kind) of a transcript, for comparing two copies.
fn shape(messages: &[Message]) -> Vec<(Role, String, Option<String>)> {
    messages
        .iter()
        .map(|m| {
            (
                m.role,
                m.text(),
                m.annotation.as_ref().map(|a| {
                    serde_json::to_value(a).unwrap()["kind"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                }),
            )
        })
        .collect()
}

fn tool_names(items: &[ActivityItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            ActivityItem::ToolCall { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

impl Ipc {
    async fn create_subthread(
        &mut self,
        parent: &ThreadId,
        kind: SubthreadKind,
        title: &str,
        prompt: Option<&str>,
    ) -> ThreadId {
        self.request(
            ClientMessage::CreateSubthread {
                parent_thread_id: parent.clone(),
                kind,
                title: Some(title.into()),
                initial_prompt: prompt.map(str::to_owned),
            },
            "subthread created",
            |m| match m {
                DaemonMessage::SubthreadCreated(snapshot) if snapshot.title == title => {
                    Some(snapshot.id.clone())
                }
                DaemonMessage::Error { message, .. } => panic!("subthread failed: {message}"),
                _ => None,
            },
        )
        .await
    }

    async fn comment(
        &mut self,
        thread: &ThreadId,
        target: CommentTarget,
        body: &str,
        parent: Option<CommentId>,
    ) -> CommentId {
        self.request(
            ClientMessage::CreateComment {
                thread_id: thread.clone(),
                target,
                body: body.into(),
                parent_comment_id: parent,
            },
            "comment created",
            |m| match m {
                DaemonMessage::CommentCreated(c) if c.body == body => Some(c.id.clone()),
                DaemonMessage::Error { message, .. } => panic!("comment failed: {message}"),
                _ => None,
            },
        )
        .await
    }

    async fn join(&mut self, node: &Node, collab_thread_id: &str) -> ThreadId {
        self.request(
            ClientMessage::JoinSharedThread {
                workspace_id: node.workspace_id.clone(),
                collab_thread_id: collab_thread_id.into(),
                link_token: None,
            },
            "joined",
            |m| match m {
                DaemonMessage::SharedThreadJoined { thread, .. } => Some(thread.id.clone()),
                DaemonMessage::Error { message, .. } => panic!("join failed: {message}"),
                _ => None,
            },
        )
        .await
    }
}

impl Node {
    /// Wait until no turn or subagent task is in flight on `threads`.
    async fn settle(&self, threads: &[ThreadId]) {
        assert!(self.sessions.wait_for(threads, WAIT).await, "settled");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    async fn outbox_drained(&self) -> bool {
        self.sessions
            .db()
            .collab_outbox_pending(1)
            .await
            .unwrap()
            .is_empty()
    }

    async fn children(&self, parent: &ThreadId) -> Vec<trama_protocol::ThreadSnapshot> {
        self.sessions.list_subthreads(parent).await.unwrap()
    }

    async fn activity(&self, message: &MessageId) -> Vec<ActivityItem> {
        self.sessions
            .db()
            .turn_activity_for_message(message)
            .await
            .unwrap()
            .unwrap_or_default()
    }
}

/// Wait until `node` mirrored everything the service has for `threads`.
async fn caught_up(server: &Server, user: &str, node: &Node, threads: &[ThreadId]) {
    let session = server.login(user).await;
    for thread in threads {
        let link = node
            .sessions
            .db()
            .collab_link(thread)
            .await
            .unwrap()
            .expect("linked");
        let head = server.last_seq(&session, &link.collab_thread_id).await;
        eventually("caught up with the shared log", || async {
            node.sessions
                .db()
                .collab_link(thread)
                .await
                .unwrap()
                .is_some_and(|l| l.cursor >= head)
        })
        .await;
    }
}

/// Alice's thread has history, activity, comments, a review, a land
/// subthread and a subagent before it is shared. Bob joins and gets all of
/// it; then a new subagent appears live, Bob works in a child on his own
/// machine, and revoking the root revokes the family.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sharing_publishes_the_whole_history_and_family() {
    let server = Server::start().await;
    let alice = Node::start(editing_adapter("alice")).await;
    let bob = Node::start(editing_adapter("bob")).await;
    let mut alice_ipc = alice.ipc().await;
    let mut bob_ipc = bob.ipc().await;
    alice_ipc.connect(&server, "alice", None).await;
    bob_ipc.connect(&server, "bob", None).await;
    alice.wait_online().await;
    bob.wait_online().await;

    // --- Alice works alone ------------------------------------------------
    let root = alice_ipc.create_thread(&alice.workspace_id, "Family").await;
    alice_ipc.subscribe(&root).await;
    alice
        .sessions
        .send_prompt(root.clone(), "first prompt".into())
        .await
        .unwrap();
    alice.settle(std::slice::from_ref(&root)).await;
    let messages = alice.messages(&root).await;
    let prompt = messages
        .iter()
        .find(|m| m.text() == "first prompt")
        .unwrap();
    let answer = messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .unwrap()
        .clone();
    assert_eq!(
        tool_names(&alice.activity(&answer.id).await),
        ["Edit", "Bash"]
    );
    let on_prompt = alice_ipc
        .comment(
            &root,
            CommentTarget::MessageRange {
                message_id: prompt.id.clone(),
                start_byte: 0,
                end_byte: 5,
            },
            "why first?",
            None,
        )
        .await;
    alice_ipc
        .comment(
            &root,
            CommentTarget::MessageRange {
                message_id: prompt.id.clone(),
                start_byte: 0,
                end_byte: 5,
            },
            "answering myself",
            Some(on_prompt.clone()),
        )
        .await;
    alice_ipc
        .request(
            ClientMessage::ResolveComment {
                comment_id: on_prompt.clone(),
            },
            "resolved",
            |m| match m {
                DaemonMessage::CommentUpdated(c) if c.id == on_prompt => Some(()),
                _ => None,
            },
        )
        .await;
    alice_ipc
        .comment(
            &root,
            CommentTarget::MessageRange {
                message_id: answer.id.clone(),
                start_byte: 0,
                end_byte: 5,
            },
            "nice answer",
            None,
        )
        .await;
    let review = alice_ipc
        .create_subthread(&root, SubthreadKind::Review, "Review it", None)
        .await;
    alice.settle(std::slice::from_ref(&review)).await;
    alice_ipc
        .request(
            ClientMessage::SubmitReviewVerdict {
                review_thread_id: review.clone(),
                verdict: ReviewVerdict::Approve,
            },
            "verdict posted",
            |m| match m {
                DaemonMessage::ReviewVerdictPosted { .. } => Some(()),
                DaemonMessage::Error { message, .. } => panic!("verdict failed: {message}"),
                _ => None,
            },
        )
        .await;
    let land = alice_ipc
        .create_subthread(&root, SubthreadKind::Land, "Land it", None)
        .await;
    alice.settle(std::slice::from_ref(&land)).await;
    let worker = alice
        .sessions
        .spawn_subagent(
            &root,
            "Helper".into(),
            "write the helper".into(),
            SubagentProfile::Worker,
            None,
        )
        .await
        .unwrap();
    alice.settle(&[worker.clone(), root.clone()]).await;
    eventually("worker merged", || async {
        alice.children(&root).await.iter().any(|c| {
            c.id == worker && c.subagent.as_ref().unwrap().status == SubagentStatus::Completed
        })
    })
    .await;
    assert!(
        alice
            .raw_prompts()
            .iter()
            .all(|p| !p.contains("<shared-thread-digest>"))
    );

    // --- Share and join ---------------------------------------------------
    let shared = alice_ipc.share(&root, vec!["bob".into()]).await;
    let bob_root = bob_ipc.join(&bob, &shared.collab_thread_id).await;
    bob_ipc.subscribe(&bob_root).await;
    eventually("alice published everything", || alice.outbox_drained()).await;
    caught_up(&server, "bob", &bob, std::slice::from_ref(&bob_root)).await;
    eventually("bob mirrored three children", || async {
        bob.children(&bob_root).await.len() == 3
    })
    .await;
    let bob_children = bob.children(&bob_root).await;
    let bob_child_ids: Vec<ThreadId> = bob_children.iter().map(|c| c.id.clone()).collect();
    caught_up(&server, "bob", &bob, &bob_child_ids).await;

    // Identical transcript: same messages, same order, Alice as author.
    let alice_root_messages = alice.messages(&root).await;
    let bob_root_messages = bob.messages(&bob_root).await;
    assert_eq!(shape(&bob_root_messages), shape(&alice_root_messages));
    assert!(bob_root_messages.iter().all(|m| m.actor == remote("alice")));
    // The turn's activity replays through `TurnActivity`.
    let bob_answer = bob_root_messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .unwrap();
    assert_eq!(
        bob.activity(&bob_answer.id).await,
        alice.activity(&answer.id).await
    );
    bob_ipc
        .send(ClientMessage::UnsubscribeThread {
            thread_id: bob_root.clone(),
        })
        .await;
    let mut replay = bob.ipc().await;
    replay.subscribe(&bob_root).await;
    replay
        .expect("activity replayed before the answer", |m| match m {
            DaemonMessage::TurnActivity {
                message_id: Some(id),
                items,
                ..
            } if *id == bob_answer.id => Some(assert_eq!(tool_names(items), ["Edit", "Bash"])),
            _ => None,
        })
        .await;
    // The subagent row links to Bob's own copy of the child.
    let spawned = bob_root_messages
        .iter()
        .find_map(|m| match &m.annotation {
            Some(MessageAnnotation::SubagentSpawned {
                child_thread_id, ..
            }) => Some(child_thread_id.clone()),
            _ => None,
        })
        .expect("subagent spawn row");
    assert!(
        bob_child_ids.contains(&spawned),
        "{spawned} in {bob_child_ids:?}"
    );

    // Comments with replies and resolution, on Bob's copies of the passages.
    let comments = bob.sessions.list_comments(&bob_root).await.unwrap();
    assert_eq!(comments.len(), 3, "{comments:#?}");
    let bob_prompt = bob_root_messages
        .iter()
        .find(|m| m.text() == "first prompt")
        .unwrap();
    let root_comment = comments.iter().find(|c| c.body == "why first?").unwrap();
    assert!(matches!(
        root_comment.state,
        trama_protocol::CommentState::Resolved { .. }
    ));
    assert_eq!(
        root_comment.target,
        CommentTarget::MessageRange {
            message_id: bob_prompt.id.clone(),
            start_byte: 0,
            end_byte: 5,
        }
    );
    let reply = comments
        .iter()
        .find(|c| c.body == "answering myself")
        .unwrap();
    assert_eq!(reply.parent_comment_id.as_ref(), Some(&root_comment.id));
    let open = comments.iter().find(|c| c.body == "nice answer").unwrap();
    assert!(matches!(open.state, trama_protocol::CommentState::Pending));
    assert!(comments.iter().all(|c| c.author == remote("alice")));

    // The family: kinds, statuses, verdict, and each child's transcript.
    let by_kind = |kind: ThreadKind| {
        bob_children
            .iter()
            .filter(|c| c.kind == kind)
            .cloned()
            .collect::<Vec<_>>()
    };
    let bob_review = by_kind(ThreadKind::Review).remove(0);
    assert_eq!(bob_review.review_verdict, Some(ReviewVerdict::Approve));
    let bob_land = by_kind(ThreadKind::Land).remove(0);
    let bob_worker = bob_children
        .iter()
        .find(|c| c.subagent.is_some())
        .unwrap()
        .clone();
    let subagent = bob_worker.subagent.clone().unwrap();
    assert_eq!(subagent.spawned_by.profile, SubagentProfile::Worker);
    assert_eq!(subagent.status, SubagentStatus::Completed);
    for (mine, theirs) in [
        (&review, &bob_review.id),
        (&land, &bob_land.id),
        (&worker, &bob_worker.id),
    ] {
        let a = alice.messages(mine).await;
        let b = bob.messages(theirs).await;
        assert!(!a.is_empty());
        assert_eq!(shape(&b), shape(&a), "child {mine}");
    }
    // The changes snapshot: Alice's diff, not Bob's checkout.
    let snapshots = bob.collab.remote_changes(&bob_root).await.unwrap();
    let snapshot = snapshots.first().expect("a changes snapshot");
    assert_eq!(snapshot.author, "alice");
    assert!(
        snapshot
            .repos
            .iter()
            .any(|r| r.diff.contains("written by alice")),
        "{snapshot:#?}"
    );
    assert!(bob.prompts().is_empty(), "joining runs nothing");

    // --- A subagent spawned after sharing appears live --------------------
    let scout = alice
        .sessions
        .spawn_subagent(
            &root,
            "Scout".into(),
            "look around".into(),
            SubagentProfile::Scout,
            None,
        )
        .await
        .unwrap();
    replay
        .expect("live child announced", |m| match m {
            DaemonMessage::SubthreadCreated(snapshot) if snapshot.title == "Scout" => Some(()),
            _ => None,
        })
        .await;
    alice.settle(&[scout.clone(), root.clone()]).await;
    eventually("the scout's status and transcript arrive", || async {
        let children = bob.children(&bob_root).await;
        let Some(child) = children.iter().find(|c| c.title == "Scout") else {
            return false;
        };
        child.subagent.as_ref().map(|s| s.status) == Some(SubagentStatus::Completed)
            && shape(&bob.messages(&child.id).await) == shape(&alice.messages(&scout).await)
    })
    .await;

    // --- Bob works in the worker child, on his own machine ----------------
    bob_ipc.subscribe(&bob_worker.id).await;
    bob_ipc.send_turn(&bob_worker.id, "bob in the child").await;
    assert_eq!(bob.prompts(), vec!["bob in the child".to_string()]);
    let bob_raw = bob.raw_prompts().remove(0);
    assert!(
        bob_raw.contains("[alice] asked: ") || bob_raw.contains("[alice's agent] answered:"),
        "bob's agent gets the digest of alice's work: {bob_raw}"
    );
    assert!(alice.prompts().iter().all(|p| p != "bob in the child"));
    eventually("alice sees bob's turn with its activity", || async {
        let messages = alice.messages(&worker).await;
        let Some(answer) = messages
            .iter()
            .rev()
            .find(|m| m.actor == remote("bob") && m.role == Role::Assistant)
        else {
            return false;
        };
        messages
            .iter()
            .any(|m| m.actor == remote("bob") && m.text() == "bob in the child")
            && answer.text() == AGENT_TEXT
            && tool_names(&alice.activity(&answer.id).await) == ["Edit", "Bash"]
    })
    .await;
    // Alice can see Bob's diff of that child.
    eventually("bob's snapshot of the child", || async {
        alice
            .collab
            .remote_changes(&worker)
            .await
            .unwrap()
            .iter()
            .any(|s| s.author == "bob" && s.repos.iter().any(|r| r.diff.contains("written by bob")))
    })
    .await;

    // --- Revoking the root revokes the whole family ----------------------
    alice_ipc
        .request(
            ClientMessage::RemoveSharedThreadMember {
                thread_id: root.clone(),
                username: "bob".into(),
            },
            "member removed",
            |m| match m {
                DaemonMessage::SharedThread(shared) => Some(shared.clone()),
                _ => None,
            },
        )
        .await;
    bob_ipc
        .expect("child revoked", |m| match m {
            DaemonMessage::CollabAccessRevoked { thread_id } if *thread_id == bob_worker.id => {
                Some(())
            }
            _ => None,
        })
        .await;
    let session = server.login("bob").await;
    let worker_collab = bob
        .sessions
        .db()
        .collab_link(&bob_worker.id)
        .await
        .unwrap()
        .unwrap()
        .collab_thread_id;
    let denied = server
        .endpoint
        .call::<ThreadDetail>(
            Method::GET,
            &format!("/threads/{worker_collab}"),
            Some(&session),
            None::<&()>,
        )
        .await;
    assert_eq!(denied, Err(http::CollabError::NotFound));
}

/// A loopback TCP forwarder that can drop every connection and refuse new
/// ones, to take the service "down" without losing its database.
struct Outage {
    down: Arc<AtomicBool>,
    connections: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Outage {
    async fn start(target: SocketAddr) -> (Self, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let down = Arc::new(AtomicBool::new(false));
        let connections: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();
        let (flag, list) = (down.clone(), connections.clone());
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                if flag.load(Ordering::SeqCst) {
                    continue;
                }
                let task = tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                list.lock().unwrap().push(task);
            }
        });
        (Self { down, connections }, format!("http://{addr}"))
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
        if down {
            for task in self.connections.lock().unwrap().drain(..) {
                task.abort();
            }
        }
    }
}

/// While the service is unreachable nothing is lost: the outbox keeps what
/// to publish across a daemon restart and delivers it, once, when the
/// service is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_outbox_delivers_after_downtime_and_restart_without_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let auth = AuthService::open(dir.path().join("collab.sqlite"), AuthConfig::default()).unwrap();
    for user in ["alice", "bob"] {
        auth.create_user(user, &super::tests::password(user))
            .await
            .unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    let app = router(auth.clone());
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let (outage, url) = Outage::start(target).await;
    let server = Server {
        endpoint: Endpoint::parse(&url).unwrap(),
        url,
        auth,
        _dir: dir,
    };
    let mut alice = Node::start(editing_adapter("alice")).await;
    let mut bob = Node::start(editing_adapter("bob")).await;
    let mut alice_ipc = alice.ipc().await;
    let mut bob_ipc = bob.ipc().await;
    alice_ipc.connect(&server, "alice", None).await;
    bob_ipc.connect(&server, "bob", None).await;
    alice.wait_online().await;
    bob.wait_online().await;
    let root = alice_ipc
        .create_thread(&alice.workspace_id, "Offline")
        .await;
    let shared = alice_ipc.share(&root, vec!["bob".into()]).await;
    let bob_root = bob_ipc.join(&bob, &shared.collab_thread_id).await;
    alice_ipc.subscribe(&root).await;
    alice_ipc.send_turn(&root, "online turn").await;
    alice.settle(std::slice::from_ref(&root)).await;
    eventually("published", || alice.outbox_drained()).await;
    // The runner's `Finish` must be acknowledged before the outage, or the
    // service marks the claimed turn ambiguous (seen on slow Windows CI).
    eventually("online turn finished", || async {
        alice
            .sessions
            .db()
            .collab_turns_in_states(&[turn_state::SUBMITTED, turn_state::CLAIMED])
            .await
            .unwrap()
            .is_empty()
    })
    .await;

    // The service goes away; Alice keeps working locally.
    outage.set_down(true);
    let child = alice_ipc
        .create_subthread(
            &root,
            SubthreadKind::Plain,
            "Offline work",
            Some("work while offline"),
        )
        .await;
    alice.settle(std::slice::from_ref(&child)).await;
    let answer = alice
        .messages(&root)
        .await
        .into_iter()
        .find(|m| m.role == Role::Assistant)
        .unwrap();
    let comment = alice_ipc
        .comment(
            &root,
            CommentTarget::MessageRange {
                message_id: answer.id.clone(),
                start_byte: 0,
                end_byte: 5,
            },
            "offline remark",
            None,
        )
        .await;
    alice_ipc
        .request(
            ClientMessage::ResolveComment {
                comment_id: comment.clone(),
            },
            "resolved",
            |m| match m {
                DaemonMessage::CommentUpdated(c) if c.id == comment => Some(()),
                _ => None,
            },
        )
        .await;
    eventually("queued while offline", || async {
        let pending = alice.sessions.db().collab_outbox_pending(64).await.unwrap();
        pending.iter().any(|row| row.attempts > 0)
            && pending.iter().any(|row| row.job.job == "child")
            && pending.iter().any(|row| row.job.job == "resolve")
    })
    .await;

    // Alice's daemon restarts while offline; the outbox is on disk.
    drop(alice_ipc);
    alice.restart_collab().await;
    let pending = alice.sessions.db().collab_outbox_pending(64).await.unwrap();
    assert!(!pending.is_empty());

    // Back online: everything arrives once.
    outage.set_down(false);
    eventually("delivered after the outage", || alice.outbox_drained()).await;
    caught_up(&server, "bob", &bob, std::slice::from_ref(&bob_root)).await;
    eventually("bob mirrored the offline child", || async {
        bob.children(&bob_root).await.len() == 1
    })
    .await;
    let bob_child = bob.children(&bob_root).await.remove(0);
    caught_up(&server, "bob", &bob, std::slice::from_ref(&bob_child.id)).await;
    assert_eq!(
        shape(&bob.messages(&bob_child.id).await),
        shape(&alice.messages(&child).await)
    );
    assert_eq!(
        shape(&bob.messages(&bob_root).await),
        shape(&alice.messages(&root).await)
    );
    let comments = bob.sessions.list_comments(&bob_root).await.unwrap();
    assert_eq!(comments.len(), 1);
    assert!(matches!(
        comments[0].state,
        trama_protocol::CommentState::Resolved { .. }
    ));

    // No duplicates in the log: every history message origin appears once.
    let session = server.login("alice").await;
    let events = server.events(&session, &shared.collab_thread_id).await;
    let mut origins: Vec<&str> = events
        .iter()
        .filter(|e| e.kind == EventKind::HistoryMessage)
        .map(|e| e.payload["origin"].as_str().unwrap())
        .collect();
    let total = origins.len();
    origins.sort();
    origins.dedup();
    assert_eq!(origins.len(), total, "{events:#?}");
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::CommentCreated)
            .count(),
        1
    );

    // Bob restarts too and catches up from his cursor without duplicates.
    bob.restart_collab().await;
    caught_up(&server, "bob", &bob, std::slice::from_ref(&bob_root)).await;
    assert_eq!(
        shape(&bob.messages(&bob_root).await),
        shape(&alice.messages(&root).await)
    );
    // Sharing again publishes nothing twice.
    let mut again = alice.ipc().await;
    let reshared = again.share(&root, vec![]).await;
    assert_eq!(reshared.collab_thread_id, shared.collab_thread_id);
    eventually("re-share drained", || alice.outbox_drained()).await;
    let after = server.events(&session, &shared.collab_thread_id).await;
    assert_eq!(
        after
            .iter()
            .filter(|e| e.kind == EventKind::HistoryMessage)
            .count(),
        total
    );
}
