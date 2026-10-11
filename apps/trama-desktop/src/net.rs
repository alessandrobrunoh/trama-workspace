//! Unix-domain client for the local trama-daemon.
//!
//! All socket I/O runs on a process-wide multi-thread Tokio runtime. GPUI's
//! foreground executor never blocks on the socket: the UI sends [`NetCommand`]s
//! and polls [`NetEvent`]s over `async_channel`, the same pattern as FakeAgent.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
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
}

/// Events the UI polls on GPUI's foreground via `cx.spawn`.
#[derive(Debug)]
pub enum NetEvent {
    Connected {
        #[allow(dead_code)]
        daemon_version: String,
    },
    Disconnected,
    Message(DaemonMessage),
}

/// Cloneable handle used by the workspace, sidebar, and composer.
#[derive(Clone)]
pub struct DaemonClient {
    commands: Sender<NetCommand>,
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
    pub fn send(&self, message: ClientMessage) {
        let _ = self.commands.try_send(NetCommand::Send(message));
    }

    pub fn select_workspace(&self, workspace_id: WorkspaceId) {
        let _ = self
            .commands
            .try_send(NetCommand::SelectWorkspace(workspace_id));
    }

    pub fn select_thread(&self, thread_id: ThreadId) {
        let _ = self.commands.try_send(NetCommand::SelectThread(thread_id));
    }
}

/// `$HOME/Library/Application Support/TramaWorkspace/daemon.sock`
pub fn default_socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/TramaWorkspace/daemon.sock"))
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

    match runtime() {
        Some(rt) => {
            rt.spawn(net_loop(command_rx, event_tx));
        }
        None => {
            let _ = event_tx.try_send(NetEvent::Disconnected);
        }
    }

    (
        DaemonClient {
            commands: command_tx,
        },
        DaemonEvents { events: event_rx },
    )
}

struct SessionState {
    workspace: Option<WorkspaceId>,
    thread: Option<ThreadId>,
    write_tx: Option<mpsc::UnboundedSender<ClientMessage>>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            workspace: None,
            thread: None,
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

async fn net_loop(commands: Receiver<NetCommand>, events: Sender<NetEvent>) {
    let state = std::sync::Arc::new(Mutex::new(SessionState::new()));
    let command_state = std::sync::Arc::clone(&state);

    tokio::spawn(async move {
        dispatch_commands(commands, command_state).await;
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
                    (st.workspace.clone(), st.thread.clone())
                };

                let _ = write_tx.send(ClientMessage::ListWorkspaces);
                if let Some(workspace_id) = replay.0 {
                    let _ = write_tx.send(ClientMessage::ListThreads { workspace_id });
                }
                if let Some(thread_id) = replay.1 {
                    let _ = write_tx.send(ClientMessage::SubscribeThread { thread_id });
                }

                let _ = events.send(NetEvent::Connected { daemon_version }).await;

                let mut writer = tokio::spawn(writer_loop(write, write_rx));
                let mut reader = tokio::spawn(reader_loop(read, events.clone()));

                tokio::select! {
                    _ = &mut writer => { reader.abort(); }
                    _ = &mut reader => { writer.abort(); }
                }

                {
                    let mut st = lock_state(&state);
                    st.write_tx = None;
                }
                let _ = events.send(NetEvent::Disconnected).await;
                announce_offline = false;
            }
            Err(_) => {
                if announce_offline {
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
    state: std::sync::Arc<Mutex<SessionState>>,
) {
    while let Ok(command) = commands.recv().await {
        let mut st = lock_state(&state);
        match command {
            NetCommand::Send(message) => {
                if let Some(tx) = st.write_tx.as_ref() {
                    let _ = tx.send(message);
                }
            }
            NetCommand::SelectWorkspace(workspace_id) => {
                st.workspace = Some(workspace_id.clone());
                if let Some(tx) = st.write_tx.as_ref() {
                    let _ = tx.send(ClientMessage::ListThreads { workspace_id });
                }
            }
            NetCommand::SelectThread(thread_id) => {
                if let Some(previous) = st.thread.clone() {
                    if previous != thread_id {
                        if let Some(tx) = st.write_tx.as_ref() {
                            let _ = tx.send(ClientMessage::UnsubscribeThread {
                                thread_id: previous,
                            });
                        }
                    }
                }
                st.thread = Some(thread_id.clone());
                if let Some(tx) = st.write_tx.as_ref() {
                    let _ = tx.send(ClientMessage::SubscribeThread { thread_id });
                }
            }
        }
    }
}

type DaemonRead = tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>;
type DaemonWrite = tokio::net::unix::OwnedWriteHalf;

async fn connect_and_handshake(
    timeout: Duration,
) -> Result<(DaemonRead, DaemonWrite, String), String> {
    let path = default_socket_path().ok_or_else(|| "HOME is not set".to_string())?;
    tokio::time::timeout(timeout, handshake_on(&path))
        .await
        .map_err(|_| "connect timeout".to_string())?
}

async fn handshake_on(path: &Path) -> Result<(DaemonRead, DaemonWrite, String), String> {
    let stream = UnixStream::connect(path)
        .await
        .map_err(|err| err.to_string())?;
    let (read, mut write) = stream.into_split();

    let hello = encode_json_line(&ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: CLIENT_NAME.to_string(),
    })
    .map_err(|err| err.to_string())?;
    write
        .write_all(&hello)
        .await
        .map_err(|err| err.to_string())?;
    write.flush().await.map_err(|err| err.to_string())?;

    let mut lines = BufReader::new(read).lines();
    let line = loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => break line,
            Ok(None) => return Err("daemon closed during handshake".into()),
            Err(err) => return Err(err.to_string()),
        }
    };

    match decode_json_line::<DaemonMessage>(&line) {
        Ok(DaemonMessage::Welcome {
            accepted,
            protocol_version,
            daemon_version,
        }) if accepted && protocol_version == PROTOCOL_VERSION => {
            Ok((lines, write, daemon_version))
        }
        Ok(DaemonMessage::Welcome {
            accepted,
            protocol_version,
            ..
        }) => Err(format!(
            "welcome rejected (accepted={accepted}, protocol_version={protocol_version})"
        )),
        Ok(_) => Err("first daemon message was not welcome".into()),
        Err(err) => Err(err.to_string()),
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
        assert!(path.ends_with("Library/Application Support/TramaWorkspace/daemon.sock"));
    }

    #[test]
    fn backoff_caps_at_ten_seconds() {
        assert_eq!(BACKOFF_SECS, [2, 5, 10]);
    }
}
