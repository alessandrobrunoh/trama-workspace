//! Unix-domain client for the local trama-daemon.
//!
//! All socket I/O runs on a process-wide multi-thread Tokio runtime. GPUI's
//! foreground executor never blocks on the socket: the UI sends [`NetCommand`]s
//! and polls [`NetEvent`]s over `async_channel`, the same pattern as FakeAgent.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use trama_domain::ids::{ThreadId, WorkspaceId};
use trama_protocol::{
    ClientMessage, DaemonMessage, PROTOCOL_VERSION, decode_json_line, encode_json_line,
};

const CLIENT_NAME: &str = "trama-desktop";
const FIRST_CONNECT_TIMEOUT: Duration = Duration::from_millis(1_500);
const RECONNECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const BACKOFF_SECS: [u64; 3] = [2, 5, 10];

/// Commands from the UI onto the Tokio client task.
#[derive(Debug, Clone)]
pub enum NetCommand {
    Send(ClientMessage),
    SelectWorkspace(WorkspaceId),
    SelectThread(ThreadId),
    /// The thread the side pane follows (a subagent), or none.
    SelectPaneThread(Option<ThreadId>),
}

/// Whether [`DaemonClient::send`] wrote the message onto the live socket queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendStatus {
    /// Queued for the connected daemon.
    Enqueued,
    /// The daemon socket is down. The message was not written.
    Rejected,
}

/// Events the UI polls on GPUI's foreground via `cx.spawn`.
#[derive(Debug)]
pub enum NetEvent {
    Connected {
        #[allow(dead_code)]
        daemon_version: String,
    },
    /// The socket is gone and the client will keep trying.
    Disconnected,
    /// Welcome was refused. The reconnect loop has stopped.
    ProtocolMismatch {
        daemon_protocol: u32,
    },
    /// Enqueued earlier, then the socket could not take it. Restore any draft.
    OutboundRejected(ClientMessage),
    Message(DaemonMessage),
}

/// Cloneable handle used by the workspace, sidebar, and composer.
#[derive(Clone)]
pub struct DaemonClient {
    commands: Sender<NetCommand>,
    connected: Arc<AtomicBool>,
}

/// Unique receiver for daemon events. Held by [`crate::workspace::Workspace`].
pub struct DaemonEvents {
    events: Receiver<NetEvent>,
}

impl DaemonEvents {
    pub fn into_receiver(self) -> Receiver<NetEvent> {
        self.events
    }
}

impl DaemonClient {
    /// Returns [`SendStatus::Rejected`] immediately when the daemon is offline
    /// so the caller can keep the user's draft. A disconnect that happens after
    /// [`SendStatus::Enqueued`] is reported as [`NetEvent::OutboundRejected`].
    pub fn send(&self, message: ClientMessage) -> SendStatus {
        if !self.connected.load(Ordering::Acquire) {
            return SendStatus::Rejected;
        }
        match self.commands.try_send(NetCommand::Send(message)) {
            Ok(()) => SendStatus::Enqueued,
            Err(_) => SendStatus::Rejected,
        }
    }

    pub fn select_workspace(&self, workspace_id: WorkspaceId) {
        let _ = self
            .commands
            .try_send(NetCommand::SelectWorkspace(workspace_id));
    }

    pub fn select_thread(&self, thread_id: ThreadId) {
        let _ = self.commands.try_send(NetCommand::SelectThread(thread_id));
    }

    /// Follow `thread_id` in the side pane (`None` stops).
    pub fn select_pane_thread(&self, thread_id: Option<ThreadId>) {
        let _ = self
            .commands
            .try_send(NetCommand::SelectPaneThread(thread_id));
    }
}

/// `<app dir>/daemon.sock`; see [`trama_protocol::default_app_dir`].
pub fn default_socket_path() -> Option<PathBuf> {
    Some(trama_protocol::default_app_dir()?.join("daemon.sock"))
}

fn runtime() -> Option<&'static tokio::runtime::Runtime> {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    if let Some(rt) = RT.get() {
        return Some(rt);
    }
    let built = tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .thread_name("trama-desktop-net")
        .build()
        .ok()?;
    Some(RT.get_or_init(|| built))
}

/// Start the background client. Connection is attempted immediately (1.5s
/// timeout); failures surface as [`NetEvent::Disconnected`] and FakeAgent
/// remains available in the UI.
pub fn start() -> (DaemonClient, DaemonEvents) {
    let (command_tx, command_rx) = async_channel::unbounded();
    let (event_tx, event_rx) = async_channel::unbounded();
    let connected = Arc::new(AtomicBool::new(false));

    match runtime() {
        Some(rt) => {
            rt.spawn(net_loop(command_rx, event_tx, Arc::clone(&connected)));
        }
        None => {
            let _ = event_tx.try_send(NetEvent::Disconnected);
        }
    }

    (
        DaemonClient {
            commands: command_tx,
            connected,
        },
        DaemonEvents { events: event_rx },
    )
}

/// Messages sent once a socket is up again. Subscriptions died with the
/// previous socket, so history is resubscribed without an unsubscribe.
pub(crate) fn replay_messages(
    workspace: Option<WorkspaceId>,
    thread: Option<ThreadId>,
    pane: Option<ThreadId>,
) -> Vec<ClientMessage> {
    let mut messages = vec![ClientMessage::ListWorkspaces];
    if let Some(workspace_id) = workspace {
        messages.push(ClientMessage::ListThreads {
            workspace_id: workspace_id.clone(),
        });
        messages.push(ClientMessage::ListRepositories { workspace_id });
    }
    if let Some(thread_id) = thread {
        messages.push(ClientMessage::SubscribeThread {
            thread_id: thread_id.clone(),
        });
        messages.push(ClientMessage::GetThreadCheckouts {
            thread_id: thread_id.clone(),
        });
        // Subscribing does not replay comments.
        messages.push(ClientMessage::ListComments {
            thread_id: thread_id.clone(),
        });
        if pane.as_ref() == Some(&thread_id) {
            return messages;
        }
    }
    if let Some(pane) = pane {
        messages.extend(follow_messages(pane));
    }
    messages
}

/// Subscribe to a thread with its checkouts and comments.
fn follow_messages(thread_id: ThreadId) -> Vec<ClientMessage> {
    vec![
        ClientMessage::SubscribeThread {
            thread_id: thread_id.clone(),
        },
        ClientMessage::GetThreadCheckouts {
            thread_id: thread_id.clone(),
        },
        ClientMessage::ListComments { thread_id },
    ]
}

/// Move the side pane's subscription from `previous` to `next`, leaving
/// the selected thread's (`main`) alone: one connection holds one
/// subscription per thread.
pub(crate) fn pane_thread_messages(
    previous: Option<ThreadId>,
    next: Option<ThreadId>,
    main: Option<&ThreadId>,
) -> Vec<ClientMessage> {
    if previous == next {
        return Vec::new();
    }
    let mut messages = Vec::new();
    if let Some(previous) = previous.filter(|previous| Some(previous) != main) {
        messages.push(ClientMessage::UnsubscribeThread {
            thread_id: previous,
        });
    }
    if let Some(next) = next.filter(|next| Some(next) != main) {
        messages.extend(follow_messages(next));
    }
    messages
}

/// Resubscribe even when the id did not change, so a migrated thread reloads
/// history instead of keeping a stale daemon subscription.
pub(crate) fn select_thread_messages(
    previous: Option<ThreadId>,
    thread_id: ThreadId,
) -> Vec<ClientMessage> {
    let mut messages = Vec::new();
    if let Some(previous) = previous {
        messages.push(ClientMessage::UnsubscribeThread {
            thread_id: previous,
        });
    }
    messages.push(ClientMessage::SubscribeThread {
        thread_id: thread_id.clone(),
    });
    messages.push(ClientMessage::GetThreadCheckouts {
        thread_id: thread_id.clone(),
    });
    messages.push(ClientMessage::ListComments { thread_id });
    messages
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WelcomeOutcome {
    Ready,
    Mismatch { daemon_protocol: u32 },
}

pub(crate) fn classify_welcome(accepted: bool, protocol_version: u32) -> WelcomeOutcome {
    if accepted && protocol_version == PROTOCOL_VERSION {
        WelcomeOutcome::Ready
    } else {
        WelcomeOutcome::Mismatch {
            daemon_protocol: protocol_version,
        }
    }
}

struct SessionState {
    workspace: Option<WorkspaceId>,
    thread: Option<ThreadId>,
    pane: Option<ThreadId>,
    write_tx: Option<mpsc::UnboundedSender<ClientMessage>>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            workspace: None,
            thread: None,
            pane: None,
            write_tx: None,
        }
    }
}

fn lock_state(state: &Mutex<SessionState>) -> std::sync::MutexGuard<'_, SessionState> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

async fn net_loop(
    commands: Receiver<NetCommand>,
    events: Sender<NetEvent>,
    connected: Arc<AtomicBool>,
) {
    let state = Arc::new(Mutex::new(SessionState::new()));
    let command_state = Arc::clone(&state);
    let command_events = events.clone();

    tokio::spawn(async move {
        dispatch_commands(commands, command_state, command_events).await;
    });

    let mut first = true;
    let mut backoff_step: usize = 0;
    let mut announce_offline = true;

    loop {
        let timeout = if first {
            FIRST_CONNECT_TIMEOUT
        } else {
            RECONNECT_CONNECT_TIMEOUT
        };
        first = false;

        match connect_and_handshake(timeout).await {
            Ok((read, write, daemon_version)) => {
                backoff_step = 0;

                let (write_tx, write_rx) = mpsc::unbounded_channel();
                let replay = {
                    let mut st = lock_state(&state);
                    st.write_tx = Some(write_tx.clone());
                    connected.store(true, Ordering::Release);
                    (st.workspace.clone(), st.thread.clone(), st.pane.clone())
                };

                for message in replay_messages(replay.0, replay.1, replay.2) {
                    let _ = write_tx.send(message);
                }

                let _ = events.send(NetEvent::Connected { daemon_version }).await;

                let mut writer = tokio::spawn(writer_loop(write, write_rx));
                let mut reader = tokio::spawn(reader_loop(read, events.clone()));

                tokio::select! {
                    _ = &mut writer => { reader.abort(); }
                    _ = &mut reader => { writer.abort(); }
                }

                connected.store(false, Ordering::Release);
                {
                    let mut st = lock_state(&state);
                    st.write_tx = None;
                }
                let _ = events.send(NetEvent::Disconnected).await;
                announce_offline = false;
            }
            Err(ConnectFailure::ProtocolMismatch { daemon_protocol }) => {
                connected.store(false, Ordering::Release);
                {
                    let mut st = lock_state(&state);
                    st.write_tx = None;
                }
                let _ = events
                    .send(NetEvent::ProtocolMismatch { daemon_protocol })
                    .await;
                break;
            }
            Err(ConnectFailure::Unavailable(reason)) => {
                if announce_offline {
                    eprintln!("trama daemon unavailable: {reason}");
                    announce_offline = false;
                    let _ = events.send(NetEvent::Disconnected).await;
                }
            }
        }

        let delay = Duration::from_secs(BACKOFF_SECS[backoff_step.min(BACKOFF_SECS.len() - 1)]);
        if backoff_step + 1 < BACKOFF_SECS.len() {
            backoff_step += 1;
        }
        tokio::time::sleep(delay).await;
    }
}

async fn dispatch_commands(
    commands: Receiver<NetCommand>,
    state: Arc<Mutex<SessionState>>,
    events: Sender<NetEvent>,
) {
    while let Ok(command) = commands.recv().await {
        let rejected = {
            let mut st = lock_state(&state);
            match command {
                NetCommand::Send(message) => {
                    let sent = st
                        .write_tx
                        .as_ref()
                        .is_some_and(|tx| tx.send(message.clone()).is_ok());
                    if sent { None } else { Some(message) }
                }
                NetCommand::SelectWorkspace(workspace_id) => {
                    st.workspace = Some(workspace_id.clone());
                    if let Some(tx) = st.write_tx.as_ref() {
                        let _ = tx.send(ClientMessage::ListThreads {
                            workspace_id: workspace_id.clone(),
                        });
                        let _ = tx.send(ClientMessage::ListRepositories { workspace_id });
                    }
                    None
                }
                NetCommand::SelectThread(thread_id) => {
                    let previous = st.thread.clone();
                    st.thread = Some(thread_id.clone());
                    // The pane cannot show the selected thread: stop
                    // following it there first, so subscribing replays.
                    let mut messages = Vec::new();
                    if st.pane.as_ref() == Some(&thread_id) {
                        st.pane = None;
                        messages.push(ClientMessage::UnsubscribeThread {
                            thread_id: thread_id.clone(),
                        });
                    }
                    let keep = st.pane.clone();
                    messages.extend(
                        select_thread_messages(previous, thread_id).into_iter().filter(
                            |message| !matches!(message, ClientMessage::UnsubscribeThread { thread_id } if Some(thread_id) == keep.as_ref()),
                        ),
                    );
                    if let Some(tx) = st.write_tx.as_ref() {
                        for message in messages {
                            let _ = tx.send(message);
                        }
                    }
                    None
                }
                NetCommand::SelectPaneThread(next) => {
                    let previous = std::mem::replace(&mut st.pane, next.clone());
                    let messages = pane_thread_messages(previous, next, st.thread.as_ref());
                    if let Some(tx) = st.write_tx.as_ref() {
                        for message in messages {
                            let _ = tx.send(message);
                        }
                    }
                    None
                }
            }
        };
        if let Some(message) = rejected {
            let _ = events.send(NetEvent::OutboundRejected(message)).await;
        }
    }
}

enum ConnectFailure {
    Unavailable(String),
    ProtocolMismatch { daemon_protocol: u32 },
}

type DaemonRead = tokio::io::Lines<BufReader<tokio::io::ReadHalf<trama_ipc::IpcStream>>>;
type DaemonWrite = tokio::io::WriteHalf<trama_ipc::IpcStream>;

async fn connect_and_handshake(
    timeout: Duration,
) -> Result<(DaemonRead, DaemonWrite, String), ConnectFailure> {
    let path = default_socket_path()
        .ok_or_else(|| ConnectFailure::Unavailable("HOME is not set".to_string()))?;
    match tokio::time::timeout(timeout, handshake_on(&path)).await {
        Ok(result) => result,
        Err(_) => Err(ConnectFailure::Unavailable("connect timeout".to_string())),
    }
}

async fn handshake_on(path: &Path) -> Result<(DaemonRead, DaemonWrite, String), ConnectFailure> {
    let stream = trama_ipc::connect(path)
        .await
        .map_err(|err| ConnectFailure::Unavailable(err.to_string()))?;
    let (read, mut write) = tokio::io::split(stream);

    let hello = encode_json_line(&ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: CLIENT_NAME.to_string(),
    })
    .map_err(|err| ConnectFailure::Unavailable(err.to_string()))?;
    write
        .write_all(&hello)
        .await
        .map_err(|err| ConnectFailure::Unavailable(err.to_string()))?;
    write
        .flush()
        .await
        .map_err(|err| ConnectFailure::Unavailable(err.to_string()))?;

    let mut lines = BufReader::new(read).lines();
    let line = loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => break line,
            Ok(None) => {
                return Err(ConnectFailure::Unavailable(
                    "daemon closed during handshake".into(),
                ));
            }
            Err(err) => return Err(ConnectFailure::Unavailable(err.to_string())),
        }
    };

    match decode_json_line::<DaemonMessage>(&line) {
        Ok(DaemonMessage::Welcome {
            accepted,
            protocol_version,
            daemon_version,
        }) => match classify_welcome(accepted, protocol_version) {
            WelcomeOutcome::Ready => Ok((lines, write, daemon_version)),
            WelcomeOutcome::Mismatch { daemon_protocol } => {
                Err(ConnectFailure::ProtocolMismatch { daemon_protocol })
            }
        },
        Ok(_) => Err(ConnectFailure::Unavailable(
            "first daemon message was not welcome".into(),
        )),
        Err(err) => Err(ConnectFailure::Unavailable(err.to_string())),
    }
}

async fn writer_loop(mut write: DaemonWrite, mut rx: mpsc::UnboundedReceiver<ClientMessage>) {
    while let Some(message) = rx.recv().await {
        let bytes = match encode_json_line(&message) {
            Ok(bytes) => bytes,
            Err(_) => break,
        };
        if write.write_all(&bytes).await.is_err() {
            break;
        }
        if write.flush().await.is_err() {
            break;
        }
    }
}

async fn reader_loop(mut lines: DaemonRead, events: Sender<NetEvent>) {
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match decode_json_line::<DaemonMessage>(&line) {
                    Ok(message) => {
                        if events.send(NetEvent::Message(message)).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_uses_application_support() {
        let path = default_socket_path().expect("HOME");
        let app_dir = trama_protocol::default_app_dir().expect("HOME");
        assert_eq!(path, app_dir.join("daemon.sock"));
    }

    #[test]
    fn backoff_caps_at_ten_seconds() {
        assert_eq!(BACKOFF_SECS, [2, 5, 10]);
    }

    #[test]
    fn rejected_welcome_is_a_mismatch_and_stops_being_a_silent_retry() {
        assert_eq!(
            classify_welcome(true, PROTOCOL_VERSION),
            WelcomeOutcome::Ready
        );
        assert_eq!(
            classify_welcome(false, PROTOCOL_VERSION),
            WelcomeOutcome::Mismatch {
                daemon_protocol: PROTOCOL_VERSION,
            }
        );
        assert_eq!(
            classify_welcome(false, PROTOCOL_VERSION.saturating_sub(1)),
            WelcomeOutcome::Mismatch {
                daemon_protocol: PROTOCOL_VERSION.saturating_sub(1),
            }
        );
    }

    #[test]
    fn replay_lists_repositories_and_resubscribes_history_and_comments() {
        let workspace_id = WorkspaceId::from("ws");
        let thread_id = ThreadId::from("th");
        assert_eq!(
            replay_messages(Some(workspace_id.clone()), Some(thread_id.clone()), None),
            vec![
                ClientMessage::ListWorkspaces,
                ClientMessage::ListThreads {
                    workspace_id: workspace_id.clone(),
                },
                ClientMessage::ListRepositories { workspace_id },
                ClientMessage::SubscribeThread {
                    thread_id: thread_id.clone()
                },
                ClientMessage::GetThreadCheckouts {
                    thread_id: thread_id.clone()
                },
                ClientMessage::ListComments { thread_id },
            ]
        );
    }

    #[test]
    fn the_side_pane_follows_its_own_thread() {
        let main = ThreadId::from("main");
        let a = ThreadId::from("a");
        let b = ThreadId::from("b");
        let follow = |thread: &ThreadId| {
            vec![
                ClientMessage::SubscribeThread {
                    thread_id: thread.clone(),
                },
                ClientMessage::GetThreadCheckouts {
                    thread_id: thread.clone(),
                },
                ClientMessage::ListComments {
                    thread_id: thread.clone(),
                },
            ]
        };
        assert_eq!(
            pane_thread_messages(None, Some(a.clone()), Some(&main)),
            follow(&a)
        );
        let mut moved = vec![ClientMessage::UnsubscribeThread {
            thread_id: a.clone(),
        }];
        moved.extend(follow(&b));
        assert_eq!(
            pane_thread_messages(Some(a.clone()), Some(b.clone()), Some(&main)),
            moved
        );
        assert!(pane_thread_messages(Some(a.clone()), Some(a.clone()), Some(&main)).is_empty());
        // The selected thread's subscription is never touched.
        assert!(pane_thread_messages(Some(main.clone()), None, Some(&main)).is_empty());
        assert!(pane_thread_messages(None, Some(main.clone()), Some(&main)).is_empty());
        // A reconnect follows both.
        let replay = replay_messages(None, Some(main.clone()), Some(a.clone()));
        assert_eq!(replay[replay.len() - 3..], follow(&a)[..]);
        let same = replay_messages(None, Some(main.clone()), Some(main.clone()));
        assert_eq!(same.len(), 4, "one subscription for one thread");
    }

    #[test]
    fn selecting_the_same_thread_still_resubscribes() {
        let thread_id = ThreadId::from("th");
        assert_eq!(
            select_thread_messages(Some(thread_id.clone()), thread_id.clone()),
            vec![
                ClientMessage::UnsubscribeThread {
                    thread_id: thread_id.clone(),
                },
                ClientMessage::SubscribeThread {
                    thread_id: thread_id.clone()
                },
                ClientMessage::GetThreadCheckouts {
                    thread_id: thread_id.clone()
                },
                ClientMessage::ListComments { thread_id },
            ]
        );
    }
}
