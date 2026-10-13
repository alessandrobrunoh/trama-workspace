//! Two daemons (separate data dirs, fake adapters) against the real
//! `trama-collab` service in process, over the IPC protocol.

use super::http::Endpoint;
use super::*;
use crate::ipc::handle_connection;
use crate::testing::{FakeAdapter, scripted_events};
use ::http::Method;
use std::net::SocketAddr;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::watch;
use trama_collab::{AuthConfig, AuthService, router};
use trama_collab_protocol::{CommandInfo, CommandState};
use trama_domain::agent::AgentEvent;
use trama_domain::message::{Actor, Message, Role};
use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership};
use trama_protocol::{
    ClientMessage, DaemonMessage, PROTOCOL_VERSION, decode_json_line, encode_json_line,
};

pub(super) const WAIT: Duration = Duration::from_secs(20);
pub(super) const AGENT_TEXT: &str = "Hello from the mock agent.";

pub(super) struct Server {
    pub(super) url: String,
    pub(super) endpoint: Endpoint,
    pub(super) auth: AuthService,
    pub(super) _dir: TempDir,
}

pub(super) fn password(user: &str) -> String {
    format!("{user}-secret-123")
}

impl Server {
    pub(super) async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let auth =
            AuthService::open(dir.path().join("collab.sqlite"), AuthConfig::default()).unwrap();
        for user in ["alice", "bob", "carol"] {
            auth.create_user(user, &password(user)).await.unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(auth.clone());
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let url = format!("http://{addr}");
        Self {
            endpoint: Endpoint::parse(&url).unwrap(),
            url,
            auth,
            _dir: dir,
        }
    }

    pub(super) async fn login(&self, user: &str) -> String {
        #[derive(serde::Deserialize)]
        struct Login {
            token: String,
        }
        let login: Login = self
            .endpoint
            .call(
                Method::POST,
                "/auth/login",
                None,
                Some(&serde_json::json!({"username": user, "password": password(user)})),
            )
            .await
            .unwrap();
        login.token
    }

    pub(super) async fn commands(&self, session: &str, thread: &str) -> Vec<CommandInfo> {
        self.endpoint
            .call(
                Method::GET,
                &format!("/threads/{thread}/commands"),
                Some(session),
                None::<&()>,
            )
            .await
            .unwrap()
    }

    pub(super) async fn events(
        &self,
        session: &str,
        thread: &str,
    ) -> Vec<trama_collab_protocol::EventEnvelope> {
        let page: trama_collab_protocol::EventsPage = self
            .endpoint
            .call(
                Method::GET,
                &format!("/threads/{thread}/events?after=0&limit=500"),
                Some(session),
                None::<&()>,
            )
            .await
            .unwrap();
        page.events
    }

    pub(super) async fn last_seq(&self, session: &str, thread: &str) -> i64 {
        let detail: ThreadDetail = self
            .endpoint
            .call(
                Method::GET,
                &format!("/threads/{thread}"),
                Some(session),
                None::<&()>,
            )
            .await
            .unwrap();
        detail.summary.last_seq
    }
}

pub(super) fn git(cwd: &Path, args: &[&str]) {
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

/// One daemon: its own data dir (database, credentials, checkouts), its own
/// repository and a fake adapter recording every prompt it runs.
pub(super) struct Node {
    pub(super) dir: TempDir,
    pub(super) sessions: SessionManager,
    pub(super) collab: CollabService,
    pub(super) fake: FakeAdapter,
    pub(super) workspace_id: WorkspaceId,
}

impl Node {
    pub(super) async fn start(fake: FakeAdapter) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("repo");
        std::fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        std::fs::write(source.join("same.txt"), "source\n").unwrap();
        git(&source, &["add", "same.txt"]);
        git(&source, &["commit", "-m", "initial"]);
        let db = TramaDb::open(&dir.path().join("app/trama.db")).unwrap();
        let workspace = Workspace::new("Shared");
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
        let sessions =
            SessionManager::with_checkout_root(db, Arc::new(fake.clone()), dir.path().join("app"));
        let collab = CollabService::new(sessions.clone(), &dir.path().join("app"));
        collab.start().await.unwrap();
        Self {
            dir,
            sessions,
            collab,
            fake,
            workspace_id: workspace.id,
        }
    }

    pub(super) fn app_dir(&self) -> PathBuf {
        self.dir.path().join("app")
    }

    /// Simulate a daemon restart: drop every connection, then start a new
    /// service from what is on disk.
    pub(super) async fn restart_collab(&mut self) {
        self.collab.shutdown();
        self.collab = CollabService::new(self.sessions.clone(), &self.app_dir());
        self.collab.start().await.unwrap();
    }

    /// Prompts the fake adapter ran, without the shared-thread digest.
    pub(super) fn prompts(&self) -> Vec<String> {
        self.raw_prompts()
            .iter()
            .map(|p| bare(p).to_owned())
            .collect()
    }

    /// Prompts exactly as the agent received them.
    pub(super) fn raw_prompts(&self) -> Vec<String> {
        self.fake
            .prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(super) async fn ipc(&self) -> Ipc {
        let (client, server) = tokio::io::duplex(1024 * 1024);
        let sessions = self.sessions.clone();
        let collab = self.collab.clone();
        let (shutdown, _) = watch::channel(false);
        tokio::spawn(async move {
            let _ = handle_connection(server, sessions, collab, shutdown).await;
        });
        let (read, write) = tokio::io::split(client);
        let mut ipc = Ipc {
            reader: BufReader::new(read),
            writer: write,
            backlog: Vec::new(),
        };
        ipc.send(ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "collab-test".into(),
        })
        .await;
        let accepted = ipc
            .expect("welcome", |m| match m {
                DaemonMessage::Welcome { accepted, .. } => Some(*accepted),
                _ => None,
            })
            .await;
        assert!(accepted);
        ipc
    }

    pub(super) async fn wait_online(&self) {
        eventually("runner online", || async {
            self.collab.status().await.unwrap().runner == CollabRunnerState::Online
        })
        .await;
    }

    pub(super) async fn messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.sessions
            .db()
            .list_messages_by_thread(thread_id)
            .await
            .unwrap()
    }
}

pub(super) struct Ipc {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    pub(super) backlog: Vec<DaemonMessage>,
}

impl Ipc {
    pub(super) async fn send(&mut self, message: ClientMessage) {
        let bytes = encode_json_line(&message).unwrap();
        self.writer.write_all(&bytes).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    /// First message (backlog included) for which `pick` returns `Some`.
    pub(super) async fn expect<T>(
        &mut self,
        what: &str,
        mut pick: impl FnMut(&DaemonMessage) -> Option<T>,
    ) -> T {
        if let Some(index) = self.backlog.iter().position(|m| pick(m).is_some()) {
            let message = self.backlog.remove(index);
            return pick(&message).unwrap();
        }
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout_at(deadline, self.reader.read_line(&mut line)).await;
            let Ok(Ok(n)) = read else {
                panic!("timed out waiting for {what}; saw {:#?}", self.backlog);
            };
            assert!(n > 0, "connection closed waiting for {what}");
            let message: DaemonMessage = decode_json_line(&line).unwrap();
            if let Some(value) = pick(&message) {
                return value;
            }
            self.backlog.push(message);
        }
    }

    pub(super) async fn request<T>(
        &mut self,
        message: ClientMessage,
        what: &str,
        pick: impl FnMut(&DaemonMessage) -> Option<T>,
    ) -> T {
        self.send(message).await;
        self.expect(what, pick).await
    }

    pub(super) async fn connect(
        &mut self,
        server: &Server,
        user: &str,
        runner_token: Option<String>,
    ) {
        let status = self
            .request(
                ClientMessage::ConnectCollab {
                    server_url: server.url.clone(),
                    username: user.into(),
                    password: password(user),
                    runner_token,
                },
                "collab status",
                |m| match m {
                    DaemonMessage::CollabStatus(status) => Some(status.clone()),
                    DaemonMessage::Error { message, .. } => panic!("connect failed: {message}"),
                    _ => None,
                },
            )
            .await;
        assert!(status.signed_in);
        assert_eq!(status.username.as_deref(), Some(user));
    }

    pub(super) async fn create_thread(
        &mut self,
        workspace_id: &WorkspaceId,
        title: &str,
    ) -> ThreadId {
        self.request(
            ClientMessage::CreateThread {
                workspace_id: workspace_id.clone(),
                title: title.into(),
            },
            "thread created",
            |m| match m {
                DaemonMessage::ThreadCreated(snapshot) => Some(snapshot.id.clone()),
                _ => None,
            },
        )
        .await
    }

    pub(super) async fn share(
        &mut self,
        thread_id: &ThreadId,
        invite: Vec<String>,
    ) -> SharedThreadSnapshot {
        self.request(
            ClientMessage::ShareThread {
                thread_id: thread_id.clone(),
                access: None,
                invite,
            },
            "shared thread",
            |m| match m {
                DaemonMessage::SharedThread(shared) => Some(shared.clone()),
                DaemonMessage::Error { message, .. } => panic!("share failed: {message}"),
                _ => None,
            },
        )
        .await
    }

    pub(super) async fn subscribe(&mut self, thread_id: &ThreadId) {
        self.send(ClientMessage::SubscribeThread {
            thread_id: thread_id.clone(),
        })
        .await;
    }

    pub(super) async fn queue_turn(&mut self, thread_id: &ThreadId, text: &str) {
        self.request(
            ClientMessage::SendPrompt {
                thread_id: thread_id.clone(),
                text: text.into(),
                mentions: Vec::new(),
                terminals: Vec::new(),
            },
            "turn queued",
            |m| match m {
                DaemonMessage::CollabTurnQueued { thread_id: id, .. } if id == thread_id => {
                    Some(())
                }
                DaemonMessage::Error { message, .. } => panic!("send failed: {message}"),
                _ => None,
            },
        )
        .await;
    }

    /// Send in a shared thread and wait until this daemon finished the turn.
    pub(super) async fn send_turn(&mut self, thread_id: &ThreadId, text: &str) {
        self.queue_turn(thread_id, text).await;
        self.expect("local turn completed", |m| match m {
            DaemonMessage::AgentEventEnvelope {
                thread_id: id,
                event: AgentEvent::TurnCompleted { .. },
            } if id == thread_id => Some(()),
            _ => None,
        })
        .await;
    }

    /// A mirrored message from `user` with `role` and `text`.
    pub(super) async fn expect_remote(
        &mut self,
        thread_id: &ThreadId,
        user: &str,
        role: Role,
        text: &str,
    ) {
        let actor = Actor::Human {
            id: format!("collab:{user}"),
        };
        self.expect(&format!("{role:?} from {user}: {text}"), |m| match m {
            DaemonMessage::MessageAppended {
                thread_id: id,
                message,
            } if id == thread_id
                && message.actor == actor
                && message.role == role
                && message.text() == text =>
            {
                Some(())
            }
            _ => None,
        })
        .await;
    }
}

/// A prompt without its leading shared-thread digest.
pub(super) fn bare(prompt: &str) -> &str {
    match prompt.split_once(&format!("{}\n\n", super::digest::CLOSE)) {
        Some((_, rest)) if prompt.starts_with(super::digest::OPEN) => rest,
        _ => prompt,
    }
}

pub(super) async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + WAIT;
    while tokio::time::Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Alice shares a thread and invites Bob; both daemons are online and
/// subscribed. Alice's runner token comes from `trama-collab create-runner`;
/// Bob's daemon provisions its own.
struct Pair {
    server: Server,
    alice: Node,
    bob: Node,
    alice_ipc: Ipc,
    bob_ipc: Ipc,
    alice_thread: ThreadId,
    bob_thread: ThreadId,
    collab_thread: String,
}

async fn pair() -> Pair {
    let server = Server::start().await;
    let alice = Node::start(FakeAdapter::scripted()).await;
    let bob = Node::start(FakeAdapter::scripted()).await;
    let mut alice_ipc = alice.ipc().await;
    let mut bob_ipc = bob.ipc().await;
    let token = server
        .auth
        .create_runner("alice", "alice-laptop")
        .await
        .unwrap();
    alice_ipc.connect(&server, "alice", Some(token)).await;
    bob_ipc.connect(&server, "bob", None).await;
    alice.wait_online().await;
    bob.wait_online().await;

    let alice_thread = alice_ipc
        .create_thread(&alice.workspace_id, "Pairing")
        .await;
    let shared = alice_ipc.share(&alice_thread, vec!["bob".into()]).await;
    assert_eq!(shared.owner, "alice");
    assert_eq!(shared.access, CollabAccess::Invited);
    assert!(shared.members.iter().any(|m| m.username == "bob"));

    let (bob_thread, joined) = bob_ipc
        .request(
            ClientMessage::JoinSharedThread {
                workspace_id: bob.workspace_id.clone(),
                collab_thread_id: shared.collab_thread_id.clone(),
                link_token: None,
            },
            "joined",
            |m| match m {
                DaemonMessage::SharedThreadJoined { thread, shared } => {
                    Some((thread.id.clone(), shared.clone()))
                }
                DaemonMessage::Error { message, .. } => panic!("join failed: {message}"),
                _ => None,
            },
        )
        .await;
    assert_eq!(joined.collab_thread_id, shared.collab_thread_id);
    assert_eq!(joined.title, "Pairing");
    alice_ipc.subscribe(&alice_thread).await;
    bob_ipc.subscribe(&bob_thread).await;
    Pair {
        server,
        alice,
        bob,
        alice_ipc,
        bob_ipc,
        alice_thread,
        bob_thread,
        collab_thread: shared.collab_thread_id,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_turn_runs_only_on_its_senders_daemon_and_is_mirrored() {
    let Pair {
        server,
        alice,
        bob,
        mut alice_ipc,
        mut bob_ipc,
        alice_thread,
        bob_thread,
        collab_thread,
    } = pair().await;

    // Bob sends: only Bob's daemon and adapter run it.
    bob_ipc.send_turn(&bob_thread, "from bob").await;
    alice_ipc
        .expect_remote(&alice_thread, "bob", Role::User, "from bob")
        .await;
    alice_ipc
        .expect_remote(&alice_thread, "bob", Role::Assistant, AGENT_TEXT)
        .await;
    assert_eq!(bob.prompts(), vec!["from bob".to_string()]);
    assert!(
        alice.prompts().is_empty(),
        "alice's agent never sees bob's prompt"
    );
    // Bob's live output was relayed to Alice (not stored).
    assert!(alice_ipc.backlog.iter().any(|m| matches!(
        m,
        DaemonMessage::CollabAgentEvent { author, event: AgentEvent::TextDelta { .. }, .. }
            if author == "bob"
    )));

    // Alice sends: only Alice's daemon runs it; Bob sees it mirrored.
    alice_ipc.send_turn(&alice_thread, "from alice").await;
    bob_ipc
        .expect_remote(&bob_thread, "alice", Role::User, "from alice")
        .await;
    bob_ipc
        .expect_remote(&bob_thread, "alice", Role::Assistant, AGENT_TEXT)
        .await;
    assert_eq!(alice.prompts(), vec!["from alice".to_string()]);
    assert_eq!(bob.prompts(), vec!["from bob".to_string()]);
    // Alice's agent was told what Bob did since its last turn.
    let raw = alice.raw_prompts().remove(0);
    assert!(raw.starts_with(super::digest::OPEN), "{raw}");
    assert!(raw.contains("[bob] asked: from bob"), "{raw}");
    assert!(
        raw.contains(&format!("[bob's agent] answered: {AGENT_TEXT}")),
        "{raw}"
    );

    // Each transcript holds its own turn once (local actor) and the other
    // participant's turn once (remote actor).
    let alice_messages = alice.messages(&alice_thread).await;
    let texts: Vec<(String, String)> = alice_messages
        .iter()
        .map(|m| (format!("{:?}", m.actor), m.text()))
        .collect();
    for text in ["from bob", "from alice"] {
        assert_eq!(
            alice_messages.iter().filter(|m| m.text() == text).count(),
            1,
            "{texts:#?}"
        );
    }
    assert!(
        alice_messages
            .iter()
            .any(|m| m.text() == "from alice" && m.actor == Actor::Human { id: "user".into() })
    );

    // The service agrees: each turn was claimed and completed by its
    // author's runner (the finish may still be in flight).
    let session = server.login("alice").await;
    eventually("both turns finished", || async {
        server
            .commands(&session, &collab_thread)
            .await
            .iter()
            .all(|c| c.state != CommandState::Claimed)
    })
    .await;
    let commands = server.commands(&session, &collab_thread).await;
    let turns: Vec<_> = commands
        .iter()
        .filter(|c| c.kind == trama_collab_protocol::CommandKind::Turn)
        .collect();
    assert_eq!(turns.len(), 2, "{commands:#?}");
    for turn in turns {
        assert_eq!(turn.state, CommandState::Completed, "{turn:#?}");
        let runner = turn.runner.as_deref().unwrap();
        match turn.author.as_str() {
            "alice" => assert_eq!(runner, "alice-laptop"),
            "bob" => assert_eq!(runner, RUNNER_NAME),
            other => panic!("unexpected author {other}"),
        }
    }

    // Credentials: owner-only file with both tokens, never the password.
    let path = alice.app_dir().join(credentials::CREDENTIALS_FILE);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains(&password("alice")));
    assert!(raw.contains("trr_"));
    let status = alice.collab.status().await.unwrap();
    let status_json = serde_json::to_string(&status).unwrap();
    assert!(!status_json.contains("trr_"), "{status_json}");
    assert_eq!(status.linked_threads, 1);

    // Drafts are relayed, never stored; comments are published.
    bob_ipc
        .send(ClientMessage::SetCollabDraft {
            thread_id: bob_thread.clone(),
            text: "half a thought".into(),
        })
        .await;
    alice_ipc
        .expect("bob's draft", |m| match m {
            DaemonMessage::CollabDraft { username, text, .. }
                if username == "bob" && text == "half a thought" =>
            {
                Some(())
            }
            _ => None,
        })
        .await;
    let target = trama_domain::comment::CommentTarget::MessageRange {
        message_id: alice_messages
            .iter()
            .find(|m| m.text() == "from alice")
            .unwrap()
            .id
            .clone(),
        start_byte: 0,
        end_byte: 4,
    };
    alice_ipc
        .send(ClientMessage::CreateComment {
            thread_id: alice_thread.clone(),
            target,
            body: "please add tests".into(),
            parent_comment_id: None,
        })
        .await;
    // Bob gets a real comment by Alice, anchored to his copy of the passage.
    let bob_from_alice = bob
        .messages(&bob_thread)
        .await
        .into_iter()
        .find(|m| m.text() == "from alice")
        .unwrap()
        .id;
    bob_ipc
        .expect("alice's comment", |m| match m {
            DaemonMessage::CommentCreated(comment)
                if comment.body == "please add tests"
                    && comment.author
                        == Actor::Human {
                            id: "collab:alice".into(),
                        } =>
            {
                Some(assert_eq!(
                    comment.target,
                    trama_domain::comment::CommentTarget::MessageRange {
                        message_id: bob_from_alice.clone(),
                        start_byte: 0,
                        end_byte: 4,
                    }
                ))
            }
            _ => None,
        })
        .await;
    assert!(
        !bob.messages(&bob_thread)
            .await
            .iter()
            .any(|m| m.text().contains("half a thought"))
    );
    // A comments-only turn is refused in a shared thread instead of reaching
    // the agent outside the shared log.
    bob_ipc
        .send(ClientMessage::SendComments {
            thread_id: bob_thread.clone(),
        })
        .await;
    bob_ipc
        .expect("send_comments refused", |m| match m {
            DaemonMessage::Error { code, message } if code == "invalid" => {
                Some(assert!(message.contains("shared thread"), "{message}"))
            }
            _ => None,
        })
        .await;
    assert_eq!(bob.prompts().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_catches_up_from_the_stored_cursor_without_duplicates() {
    let Pair {
        server,
        alice,
        mut bob,
        mut alice_ipc,
        bob_ipc,
        alice_thread,
        bob_thread,
        collab_thread,
    } = pair().await;
    drop(bob_ipc);
    bob.collab.shutdown();

    alice_ipc
        .send_turn(&alice_thread, "while bob was away 1")
        .await;
    alice_ipc
        .send_turn(&alice_thread, "while bob was away 2")
        .await;

    let session = server.login("bob").await;
    // `TurnCompleted` precedes the stored answer (the turn checkpoint is
    // recorded first), so wait until the runner reported and finished both
    // turns, and Alice's outbox published the answers, before taking the
    // log head.
    eventually("alice published both answers", || async {
        alice
            .sessions
            .db()
            .collab_outbox_pending(1)
            .await
            .unwrap()
            .is_empty()
            && server
                .events(&session, &collab_thread)
                .await
                .iter()
                .filter(|e| e.kind == trama_collab_protocol::EventKind::HistoryMessage)
                .count()
                == 2
    })
    .await;
    eventually("alice's runner finished both turns", || async {
        let turns: Vec<_> = server
            .commands(&session, &collab_thread)
            .await
            .into_iter()
            .filter(|c| c.kind == trama_collab_protocol::CommandKind::Turn)
            .collect();
        turns.len() == 2
            && turns
                .iter()
                .all(|c| matches!(c.state, CommandState::Completed | CommandState::Failed))
    })
    .await;
    let remote_count = |messages: &[Message]| {
        messages
            .iter()
            .filter(|m| {
                m.actor
                    == Actor::Human {
                        id: "collab:alice".into(),
                    }
            })
            .count()
    };
    for _ in 0..2 {
        // Credentials and cursor come from disk, like a daemon restart.
        bob.restart_collab().await;
        let head = server.last_seq(&session, &collab_thread).await;
        eventually("bob caught up to the log head", || async {
            bob.sessions
                .db()
                .collab_link(&bob_thread)
                .await
                .unwrap()
                .is_some_and(|link| link.cursor == head)
        })
        .await;
        let messages = bob.messages(&bob_thread).await;
        assert_eq!(remote_count(&messages), 4, "two prompts and two answers");
        for text in ["while bob was away 1", "while bob was away 2"] {
            assert_eq!(messages.iter().filter(|m| m.text() == text).count(), 1);
        }
    }
    assert!(bob.prompts().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_member_stops_receiving_and_cannot_send() {
    let Pair {
        server: _server,
        alice: _alice,
        bob,
        mut alice_ipc,
        mut bob_ipc,
        alice_thread,
        bob_thread,
        ..
    } = pair().await;
    let shared = alice_ipc
        .request(
            ClientMessage::RemoveSharedThreadMember {
                thread_id: alice_thread.clone(),
                username: "bob".into(),
            },
            "member removed",
            |m| match m {
                DaemonMessage::SharedThread(shared) => Some(shared.clone()),
                _ => None,
            },
        )
        .await;
    assert!(!shared.members.iter().any(|m| m.username == "bob"));
    bob_ipc
        .expect("revoked", |m| match m {
            DaemonMessage::CollabAccessRevoked { thread_id } if *thread_id == bob_thread => {
                Some(())
            }
            _ => None,
        })
        .await;

    alice_ipc.send_turn(&alice_thread, "after bob left").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !bob.messages(&bob_thread)
            .await
            .iter()
            .any(|m| m.text().contains("after bob left"))
    );

    bob_ipc
        .send(ClientMessage::SendPrompt {
            thread_id: bob_thread.clone(),
            text: "still here?".into(),
            mentions: Vec::new(),
            terminals: Vec::new(),
        })
        .await;
    bob_ipc
        .expect("send refused", |m| match m {
            DaemonMessage::Error { code, .. } if code == "not_found" => Some(()),
            _ => None,
        })
        .await;
    assert!(bob.prompts().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_runner_never_claims_a_turn_it_did_not_send() {
    let server = Server::start().await;
    let alice = Node::start(FakeAdapter::scripted()).await;
    let mut alice_ipc = alice.ipc().await;
    alice_ipc.connect(&server, "alice", None).await;
    alice.wait_online().await;
    let thread = alice_ipc.create_thread(&alice.workspace_id, "Mine").await;
    let cid = alice_ipc.share(&thread, vec![]).await.collab_thread_id;
    let alice_session = server.login("alice").await;

    // Alice's own turn, sent from another client (not this daemon): the
    // runner is offered it but must not run it on this machine.
    server
        .endpoint
        .call::<CommandReceipt>(
            Method::POST,
            &format!("/threads/{cid}/send"),
            Some(&alice_session),
            Some(&SendRequest {
                command_id: "web-1".into(),
                text: Some("sent from the web".into()),
            }),
        )
        .await
        .unwrap();

    // Bob's daemon holding Alice's runner token: the hello names alice, so
    // it refuses to act at all.
    let bob = Node::start(FakeAdapter::scripted()).await;
    let mut bob_ipc = bob.ipc().await;
    let alice_token = server.auth.create_runner("alice", "stolen").await.unwrap();
    bob_ipc.connect(&server, "bob", Some(alice_token)).await;
    eventually("bob's runner refused alice's token", || async {
        bob.collab.status().await.unwrap().runner == CollabRunnerState::Failed
    })
    .await;
    let status = bob.collab.status().await.unwrap();
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("belongs to `alice`")),
        "{status:?}"
    );

    tokio::time::sleep(Duration::from_millis(800)).await;
    let commands = server.commands(&alice_session, &cid).await;
    let web = commands.iter().find(|c| c.command_id == "web-1").unwrap();
    assert_eq!(web.state, CommandState::Pending, "{commands:#?}");
    assert!(alice.prompts().is_empty());
    assert!(bob.prompts().is_empty());

    // This daemon's own send still runs here, and only that one.
    alice_ipc.subscribe(&thread).await;
    alice_ipc.send_turn(&thread, "sent from this daemon").await;
    assert_eq!(alice.prompts(), vec!["sent from this daemon".to_string()]);
    let commands = server.commands(&alice_session, &cid).await;
    assert_eq!(
        commands
            .iter()
            .find(|c| c.command_id == "web-1")
            .unwrap()
            .state,
        CommandState::Pending
    );
    // The web turn is mirrored as Alice's message from another client.
    eventually("web turn mirrored", || async {
        alice
            .messages(&thread)
            .await
            .iter()
            .any(|m| m.text() == "sent from the web")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_lost_mid_turn_is_ambiguous_and_never_rerun() {
    let server = Server::start().await;
    let mut alice = Node::start(FakeAdapter::new(
        scripted_events(),
        Duration::from_millis(150),
    ))
    .await;
    let mut ipc = alice.ipc().await;
    ipc.connect(&server, "alice", None).await;
    alice.wait_online().await;
    let thread = ipc.create_thread(&alice.workspace_id, "Flaky").await;
    let cid = ipc.share(&thread, vec![]).await.collab_thread_id;
    ipc.subscribe(&thread).await;
    ipc.queue_turn(&thread, "long running").await;
    eventually("turn started", || async { !alice.prompts().is_empty() }).await;
    // The daemon loses the service while the claimed turn runs.
    alice.collab.shutdown();
    ipc.expect("local turn ends", |m| match m {
        DaemonMessage::AgentEventEnvelope {
            event: AgentEvent::TurnCompleted { .. },
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    let notices = |messages: &[Message]| {
        messages
            .iter()
            .filter(|m| m.role == Role::System && m.text() == AMBIGUOUS_NOTICE)
            .count()
    };
    eventually("ambiguous notice", || async {
        notices(&alice.messages(&thread).await) == 1
    })
    .await;

    // Restart: nothing is rerun, the service reports it ambiguous, and the
    // notice is not repeated.
    alice.restart_collab().await;
    alice.wait_online().await;
    let session = server.login("alice").await;
    eventually("server marks the turn ambiguous", || async {
        server
            .commands(&session, &cid)
            .await
            .iter()
            .all(|c| c.state == CommandState::Ambiguous)
    })
    .await;
    let head = server.last_seq(&session, &cid).await;
    eventually("mirror caught up", || async {
        alice
            .sessions
            .db()
            .collab_link(&thread)
            .await
            .unwrap()
            .is_some_and(|link| link.cursor == head)
    })
    .await;
    assert_eq!(alice.prompts(), vec!["long running".to_string()]);
    assert_eq!(notices(&alice.messages(&thread).await), 1);
}
