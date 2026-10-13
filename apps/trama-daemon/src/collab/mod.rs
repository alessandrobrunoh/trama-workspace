//! Shared threads through a `trama-collab` service (ADR 0008, ADR 0009).
//!
//! # Execution rule
//!
//! A turn runs only on the machine of the person who pressed Send, with that
//! person's own `claude` login. This daemon is a runner for its owner only:
//!
//! - `SendPrompt` on a linked thread submits the turn to the service with a
//!   command id carrying this installation's prefix and records it locally.
//! - The runner socket (runner token) only claims a `command_available` turn
//!   when the service's hello names the signed-in user, the command id is one
//!   this daemon submitted (same thread and text), and the durable
//!   `message_appended` event for it is authored by the signed-in user. The
//!   service enforces author == runner owner as well; this is the client-side
//!   half, so a misconfigured or hostile service still cannot make this
//!   daemon run someone else's prompt.
//! - A claimed turn runs through the normal `SendPrompt` path (isolated
//!   checkout, approvals, billing guard). A claim this daemon cannot finish
//!   (socket loss, crash, restart) is surfaced as ambiguous and never retried.
//!
//! # Mirroring
//!
//! Each linked thread keeps a client socket that resumes from the cursor
//! stored in TramaDB. Events from other participants (and other clients of
//! this user) are persisted as transcript messages with the actor
//! `collab:<username>` in the same transaction that advances the cursor, so a
//! reconnect never duplicates or skips one. Live agent output, drafts and
//! presence are relayed to subscribers and never stored.
//!
//! # History and family (ADR 0013)
//!
//! Sharing a thread publishes its whole history and its whole family: every
//! local message with its turn's activity, child threads at any depth
//! (subthreads, forks, subagents, each its own collab thread under the
//! root's membership), comments and their resolution, review verdicts,
//! subagent states and changes snapshots. Everything goes through a durable
//! outbox (`outbox`) fed by rescans of the family (`family`); the mirror
//! rebuilds it on every other participant's machine. A shared turn's agent
//! gets a digest of what the others did since its last turn (`digest`).

mod credentials;
mod digest;
mod discovery;
pub(crate) mod family;
mod history;
mod http;
mod mirror;
mod outbox;
mod runner;
mod share_events;
#[cfg(test)]
mod tests;
#[cfg(test)]
#[path = "tests/history.rs"]
mod tests_history;
#[cfg(test)]
#[path = "tests/tls.rs"]
mod tests_tls;
mod tls;

use crate::error::{DaemonError, Result};
use crate::session::SessionManager;
use ::http::Method;
use chrono::Utc;
use credentials::Credentials;
use http::{CollabError, Endpoint, validate_path_segment};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use trama_collab_protocol::{
    ClientFrame, CommandReceipt, CreateRunnerRequest, CreateThreadRequest, InviteRequest,
    JoinRequest, RunnerClientFrame, RunnerCreated, SendRequest, SetAccessRequest,
    SetAccessResponse, ThreadDetail, ThreadSummary,
};
use trama_db::{CollabLink, CollabTurn, TramaDb};
use trama_domain::ids::{CommentId, MessageId, ThreadId, WorkspaceId};
use trama_domain::thread::ThreadKind;
use trama_protocol::{
    CollabAccess, CollabPresenceState, CollabRunnerState, CollabStatusSnapshot, CommentSnapshot,
    SharedThreadListing, SharedThreadSnapshot, ThreadSnapshot,
};

/// Name of runners this daemon provisions for its user.
const RUNNER_NAME: &str = "trama-daemon";
const INSTALL_KEY: &str = "install_id";
/// Actor id prefix of mirrored participants (`collab:bob`).
pub(crate) const REMOTE_ACTOR_PREFIX: &str = "collab:";
const MAX_TURN_TEXT_BYTES: usize = 32 * 1024;
const MAX_COMMENT_BYTES: usize = 16 * 1024;
const MAX_ANCHOR_BYTES: usize = 4 * 1024;

/// Local turn states (see migration 0005).
pub(crate) mod turn_state {
    pub const SUBMITTED: &str = "submitted";
    pub const SEND_FAILED: &str = "send_failed";
    pub const CLAIMING: &str = "claiming";
    pub const CLAIMED: &str = "claimed";
    pub const COMPLETED: &str = "completed";
    pub const FAILED: &str = "failed";
    pub const REJECTED: &str = "rejected";
    pub const AMBIGUOUS: &str = "ambiguous";
}

pub(crate) const AMBIGUOUS_NOTICE: &str = "This shared turn's outcome is unknown: Trama lost the \
     collaboration service connection (or restarted) after claiming it, so its result was not \
     confirmed to the other participants. It is not retried automatically; check the thread's \
     changes and send again if needed.";

/// Shared threads and the local runner. Cheap to clone.
#[derive(Clone)]
pub struct CollabService {
    inner: Arc<Inner>,
}

struct Inner {
    sessions: SessionManager,
    /// `None` disables collaboration (no app dir, e.g. unit tests).
    credentials_path: Option<PathBuf>,
    state: Mutex<State>,
    /// Serializes family rescans with the mirror creating children, so a
    /// thread is never both announced and mirrored.
    sync_lock: tokio::sync::Mutex<()>,
    /// Wakes the outbox worker.
    outbox_notify: tokio::sync::Notify,
    /// vNext share event replay state.
    #[allow(dead_code)] // fed by the M4 transport
    share_events: share_events::ShareEventRouter,
}

struct State {
    credentials: Option<Credentials>,
    install_id: String,
    /// Bumped whenever connections are torn down; tasks of an older
    /// generation stop instead of acting on stale credentials.
    generation: u64,
    signed_in: bool,
    runner: CollabRunnerState,
    last_error: Option<String>,
    runner_task: Option<JoinHandle<()>>,
    outbox_task: Option<JoinHandle<()>>,
    runner_tx: Option<mpsc::UnboundedSender<RunnerClientFrame>>,
    claims: HashMap<String, oneshot::Sender<std::result::Result<(), String>>>,
    finishes: HashMap<String, oneshot::Sender<()>>,
    active_turns: HashSet<String>,
    mirrors: HashMap<ThreadId, MirrorHandle>,
    thread_locks: HashMap<ThreadId, Arc<tokio::sync::Mutex<()>>>,
}

struct MirrorHandle {
    generation: u64,
    task: JoinHandle<()>,
    frames: mpsc::UnboundedSender<ClientFrame>,
}

/// What a task needs to talk to the service, captured under the lock.
#[derive(Clone)]
pub(crate) struct Connection {
    pub endpoint: Endpoint,
    pub username: String,
    pub session_token: Option<String>,
    pub runner_token: Option<String>,
    pub generation: u64,
    /// `trd.<installation id>.`: every command id this daemon issues.
    pub own_prefix: String,
}

impl Connection {
    fn new_command_id(&self) -> String {
        format!("{}{}", self.own_prefix, MessageId::generate())
    }

    pub fn session(&self) -> Result<&str> {
        self.session_token.as_deref().ok_or_else(|| {
            DaemonError::Collab("not signed in to the collab service; connect again".into())
        })
    }

    pub fn is_own_command(&self, command_id: Option<&str>) -> bool {
        command_id.is_some_and(|id| id.starts_with(&self.own_prefix))
    }

    pub fn is_me(&self, username: Option<&str>) -> bool {
        username.is_some_and(|name| name.eq_ignore_ascii_case(&self.username))
    }
}

fn collab_error(error: CollabError) -> DaemonError {
    match error {
        CollabError::NotFound => DaemonError::NotFound(error.to_string()),
        CollabError::Rejected(..) | CollabError::Forbidden => {
            DaemonError::Invalid(error.to_string())
        }
        other => DaemonError::Collab(other.to_string()),
    }
}

/// Normalize line endings and refuse text the service would reject.
pub(crate) fn sanitize_text(text: &str, max: usize, what: &str) -> Result<String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if text.trim().is_empty() {
        return Err(DaemonError::Invalid(format!("{what} must not be empty")));
    }
    if text.len() > max {
        return Err(DaemonError::Invalid(format!(
            "{what} is longer than the shared-thread limit of {} KiB",
            max / 1024
        )));
    }
    if text.chars().any(|c| {
        (c.is_control() && !matches!(c, '\n' | '\t')) || matches!(c, '\u{2028}' | '\u{2029}')
    }) {
        return Err(DaemonError::Invalid(format!(
            "{what} contains control characters a shared thread cannot carry"
        )));
    }
    Ok(text)
}

/// Cut `text` to at most `max` bytes on a character boundary.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn access_label(access: CollabAccess) -> &'static str {
    match access {
        CollabAccess::Invited => "invited",
        CollabAccess::Link => "link",
    }
}

impl CollabService {
    /// Collaboration turned off: every command answers with an error.
    pub fn disabled(sessions: SessionManager) -> Self {
        Self::build(sessions, None)
    }

    /// Credentials live in `<app_dir>/collab-credentials.json`.
    pub fn new(sessions: SessionManager, app_dir: &Path) -> Self {
        Self::build(sessions, Some(app_dir.join(credentials::CREDENTIALS_FILE)))
    }

    fn build(sessions: SessionManager, credentials_path: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions,
                credentials_path,
                state: Mutex::new(State {
                    credentials: None,
                    install_id: String::new(),
                    generation: 0,
                    signed_in: false,
                    runner: CollabRunnerState::Off,
                    last_error: None,
                    runner_task: None,
                    outbox_task: None,
                    runner_tx: None,
                    claims: HashMap::new(),
                    finishes: HashMap::new(),
                    active_turns: HashSet::new(),
                    mirrors: HashMap::new(),
                    thread_locks: HashMap::new(),
                }),
                sync_lock: tokio::sync::Mutex::new(()),
                outbox_notify: tokio::sync::Notify::new(),
                share_events: Default::default(),
            }),
        }
    }

    /// `trd.<installation id>.`: the prefix of every command id this daemon
    /// issues.
    pub(crate) fn own_prefix(&self) -> String {
        format!("trd.{}.", self.lock().install_id)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn sessions(&self) -> &SessionManager {
        &self.inner.sessions
    }

    #[allow(dead_code)] // fed by the M4 transport
    pub(crate) fn share_events(&self) -> &share_events::ShareEventRouter {
        &self.inner.share_events
    }

    pub(crate) fn db(&self) -> &TramaDb {
        self.inner.sessions.db()
    }

    fn credentials_path(&self) -> Result<&Path> {
        self.inner.credentials_path.as_deref().ok_or_else(|| {
            DaemonError::Collab("collaboration is not available in this daemon".into())
        })
    }

    /// Load the installation id and stored credentials, settle turns a
    /// previous run left claimed, and reconnect when credentials are enabled.
    pub async fn start(&self) -> Result<()> {
        let Some(path) = self.inner.credentials_path.clone() else {
            return Ok(());
        };
        let install = self
            .db()
            .collab_setting_or_insert(INSTALL_KEY, &MessageId::generate().to_string())
            .await?;
        self.lock().install_id = install;
        self.settle_interrupted_turns().await;
        let (signals, rx) = mpsc::unbounded_channel();
        self.inner.sessions.set_collab_observer(Some(signals));
        tokio::spawn(self.clone().run_signals(rx));
        match credentials::load(&path) {
            Ok(Some(credentials)) if credentials.enabled => self.launch(credentials),
            Ok(Some(credentials)) => self.lock().credentials = Some(credentials),
            Ok(None) => {}
            Err(err) => {
                crate::log::warn(format!("collab credentials not loaded: {err}"));
                self.lock().last_error = Some(err.to_string());
            }
        }
        Ok(())
    }

    /// Turns a previous process claimed but never confirmed: their outcome
    /// is unknown. Mark them ambiguous once, with a notice; never rerun.
    async fn settle_interrupted_turns(&self) {
        let stale = match self
            .db()
            .collab_turns_in_states(&[turn_state::CLAIMING, turn_state::CLAIMED])
            .await
        {
            Ok(stale) => stale,
            Err(err) => {
                crate::log::warn(format!("could not read interrupted shared turns: {err}"));
                return;
            }
        };
        for turn in stale {
            self.mark_ambiguous(&turn.command_id, &turn.thread_id).await;
        }
    }

    /// Move a local turn to ambiguous and persist one notice when it changed.
    pub(crate) async fn mark_ambiguous(&self, command_id: &str, thread_id: &ThreadId) -> bool {
        let changed = self
            .db()
            .transition_collab_turn(
                command_id,
                &[
                    turn_state::SUBMITTED,
                    turn_state::CLAIMING,
                    turn_state::CLAIMED,
                ],
                turn_state::AMBIGUOUS,
            )
            .await
            .unwrap_or(false);
        if changed {
            self.sessions()
                .append_system_notice(thread_id, AMBIGUOUS_NOTICE.into())
                .await;
        }
        changed
    }

    /// Replace live connections with ones for `credentials`.
    fn launch(&self, credentials: Credentials) {
        let generation = {
            let mut state = self.lock();
            Self::stop_locked(&mut state);
            state.signed_in = credentials.session_token.is_some();
            state.runner = if credentials.runner_token.is_some() {
                CollabRunnerState::Connecting
            } else {
                CollabRunnerState::Off
            };
            state.last_error = None;
            let has_runner = credentials.runner_token.is_some();
            state.credentials = Some(credentials);
            let generation = state.generation;
            if has_runner {
                state.runner_task = Some(tokio::spawn(runner::run(self.clone(), generation)));
            }
            state.outbox_task = Some(tokio::spawn(self.clone().run_outbox(generation)));
            generation
        };
        let service = self.clone();
        tokio::spawn(async move { service.start_mirrors(generation).await });
    }

    /// Drop every connection without touching stored state, as if the
    /// process stopped. Claimed turns become ambiguous.
    #[cfg(test)]
    pub(crate) fn shutdown(&self) {
        let mut state = self.lock();
        Self::stop_locked(&mut state);
        state.credentials = None;
    }

    fn stop_locked(state: &mut State) {
        state.generation += 1;
        if let Some(task) = state.runner_task.take() {
            task.abort();
        }
        if let Some(task) = state.outbox_task.take() {
            task.abort();
        }
        state.runner_tx = None;
        state.claims.clear();
        state.finishes.clear();
        for (_, mirror) in state.mirrors.drain() {
            mirror.task.abort();
        }
        state.runner = CollabRunnerState::Off;
    }

    /// Current connection parameters, if enabled and still `generation`
    /// (any generation when `None`).
    pub(crate) fn connection(&self, generation: Option<u64>) -> Option<Connection> {
        let state = self.lock();
        if generation.is_some_and(|g| g != state.generation) {
            return None;
        }
        let credentials = state.credentials.as_ref().filter(|c| c.enabled)?;
        let endpoint = Endpoint::parse(&credentials.server_url).ok()?;
        Some(Connection {
            endpoint,
            username: credentials.username.clone(),
            session_token: credentials
                .session_token
                .clone()
                .filter(|_| state.signed_in),
            runner_token: credentials.runner_token.clone(),
            generation: state.generation,
            own_prefix: format!("trd.{}.", state.install_id),
        })
    }

    fn require_connection(&self) -> Result<Connection> {
        self.credentials_path()?;
        self.connection(None).ok_or_else(|| {
            DaemonError::Collab("not connected to a collab service; connect first".into())
        })
    }

    pub(crate) fn set_runner_state(
        &self,
        generation: u64,
        runner: CollabRunnerState,
        error: Option<String>,
    ) {
        let mut state = self.lock();
        if state.generation != generation {
            return;
        }
        state.runner = runner;
        if error.is_some() || runner == CollabRunnerState::Online {
            state.last_error = error;
        }
    }

    /// The service rejected the session token: stop using it until the user
    /// connects again. The runner keeps its own credential.
    pub(crate) fn session_rejected(&self, generation: u64) {
        let mut state = self.lock();
        if state.generation != generation {
            return;
        }
        state.signed_in = false;
        state.last_error = Some("the collab session expired; connect again to sign in".into());
        for (_, mirror) in state.mirrors.drain() {
            mirror.task.abort();
        }
    }

    pub(crate) fn note_error(&self, generation: u64, error: String) {
        let mut state = self.lock();
        if state.generation == generation {
            state.last_error = Some(error);
        }
    }

    // -----------------------------------------------------------------------
    // Runner bookkeeping (used by `runner`)
    // -----------------------------------------------------------------------

    pub(crate) fn set_runner_tx(
        &self,
        generation: u64,
        tx: Option<mpsc::UnboundedSender<RunnerClientFrame>>,
    ) -> bool {
        let mut state = self.lock();
        if state.generation != generation {
            return false;
        }
        if tx.is_none() {
            // Waiters observe the dropped senders as a lost connection.
            state.claims.clear();
            state.finishes.clear();
        }
        state.runner_tx = tx;
        true
    }

    pub(crate) fn send_runner(&self, frame: RunnerClientFrame) -> bool {
        let state = self.lock();
        state
            .runner_tx
            .as_ref()
            .is_some_and(|tx| tx.send(frame).is_ok())
    }

    pub(crate) fn expect_claim(
        &self,
        command_id: &str,
    ) -> oneshot::Receiver<std::result::Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        self.lock().claims.insert(command_id.to_owned(), tx);
        rx
    }

    pub(crate) fn resolve_claim(&self, command_id: &str, result: std::result::Result<(), String>) {
        if let Some(tx) = self.lock().claims.remove(command_id) {
            let _ = tx.send(result);
        }
    }

    pub(crate) fn expect_finish(&self, command_id: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.lock().finishes.insert(command_id.to_owned(), tx);
        rx
    }

    pub(crate) fn resolve_finish(&self, command_id: &str) {
        if let Some(tx) = self.lock().finishes.remove(command_id) {
            let _ = tx.send(());
        }
    }

    /// `false` when the command is already being handled.
    pub(crate) fn begin_turn(&self, command_id: &str) -> bool {
        self.lock().active_turns.insert(command_id.to_owned())
    }

    pub(crate) fn end_turn(&self, command_id: &str) {
        self.lock().active_turns.remove(command_id);
    }

    /// Serializes shared turns per local thread.
    pub(crate) fn thread_lock(&self, thread_id: &ThreadId) -> Arc<tokio::sync::Mutex<()>> {
        self.lock()
            .thread_locks
            .entry(thread_id.clone())
            .or_default()
            .clone()
    }

    // -----------------------------------------------------------------------
    // Mirrors
    // -----------------------------------------------------------------------

    async fn start_mirrors(&self, generation: u64) {
        let Some(connection) = self.connection(Some(generation)) else {
            return;
        };
        if connection.session_token.is_none() {
            return;
        }
        match self.db().list_collab_links(connection.endpoint.url()).await {
            Ok(links) => {
                for link in links {
                    self.ensure_mirror(&link, generation);
                }
            }
            Err(err) => crate::log::warn(format!("could not list shared threads: {err}")),
        }
    }

    fn ensure_mirror(&self, link: &CollabLink, generation: u64) {
        let mut state = self.lock();
        if state.generation != generation || !state.signed_in {
            return;
        }
        if state
            .mirrors
            .get(&link.thread_id)
            .is_some_and(|mirror| mirror.generation == generation && !mirror.task.is_finished())
        {
            return;
        }
        let (frames, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(mirror::run(
            self.clone(),
            generation,
            link.thread_id.clone(),
            link.collab_thread_id.clone(),
            rx,
        ));
        if let Some(old) = state.mirrors.insert(
            link.thread_id.clone(),
            MirrorHandle {
                generation,
                task,
                frames,
            },
        ) {
            old.task.abort();
        }
    }

    pub(crate) fn mirror_ended(&self, thread_id: &ThreadId, generation: u64) {
        let mut state = self.lock();
        if state
            .mirrors
            .get(thread_id)
            .is_some_and(|mirror| mirror.generation == generation)
        {
            state.mirrors.remove(thread_id);
        }
    }

    fn mirror_frame(&self, thread_id: &ThreadId, frame: ClientFrame) -> Result<()> {
        let state = self.lock();
        let mirror = state.mirrors.get(thread_id).ok_or_else(|| {
            DaemonError::Collab("this thread is not shared or not connected".into())
        })?;
        mirror
            .frames
            .send(frame)
            .map_err(|_| DaemonError::Collab("the shared-thread connection closed".into()))
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    pub async fn status(&self) -> Result<CollabStatusSnapshot> {
        let (server_url, username, signed_in, runner, last_error) = {
            let state = self.lock();
            let credentials = state.credentials.as_ref();
            (
                credentials.map(|c| c.server_url.clone()),
                credentials.map(|c| c.username.clone()),
                state.signed_in && credentials.is_some_and(|c| c.enabled),
                state.runner,
                state.last_error.clone(),
            )
        };
        let linked_threads = match &server_url {
            Some(url) => self.db().list_collab_links(url).await?.len() as u32,
            None => 0,
        };
        Ok(CollabStatusSnapshot {
            server_url,
            username,
            signed_in,
            runner,
            linked_threads,
            last_error,
        })
    }

    /// Sign in, provision (or reuse) the runner token, store both owner-only,
    /// and (re)start the runner and shared-thread sockets.
    pub async fn connect(
        &self,
        server_url: String,
        username: String,
        password: String,
        runner_token: Option<String>,
    ) -> Result<CollabStatusSnapshot> {
        let path = self.credentials_path()?.to_path_buf();
        let endpoint = Endpoint::parse(&server_url).map_err(DaemonError::Invalid)?;
        let username = username.trim().to_owned();
        validate_path_segment(&username, "username").map_err(DaemonError::Invalid)?;
        if password.is_empty() {
            return Err(DaemonError::Invalid("password must not be empty".into()));
        }
        #[derive(Deserialize)]
        struct Login {
            token: String,
            username: String,
        }
        let login: Login = endpoint
            .call(
                Method::POST,
                "/auth/login",
                None,
                Some(&serde_json::json!({ "username": username, "password": password })),
            )
            .await
            .map_err(|err| match err {
                CollabError::Unauthorized => {
                    DaemonError::Invalid("wrong username or password".into())
                }
                other => collab_error(other),
            })?;
        let runner_token = match runner_token.map(|t| t.trim().to_owned()) {
            Some(token) => {
                if !token.starts_with("trr_")
                    || token.len() > 256
                    || !token.bytes().all(|b| b.is_ascii_graphic())
                {
                    return Err(DaemonError::Invalid(
                        "that is not a runner token (they start with `trr_`)".into(),
                    ));
                }
                token
            }
            None => {
                let previous = credentials::load(&path).ok().flatten().filter(|c| {
                    c.server_url == endpoint.url()
                        && c.username.eq_ignore_ascii_case(&login.username)
                });
                match previous.and_then(|c| c.runner_token) {
                    Some(token) => token,
                    None => {
                        let created: RunnerCreated = endpoint
                            .call(
                                Method::POST,
                                "/runners",
                                Some(&login.token),
                                Some(&CreateRunnerRequest {
                                    name: RUNNER_NAME.into(),
                                }),
                            )
                            .await
                            .map_err(collab_error)?;
                        created.token
                    }
                }
            }
        };
        let credentials = Credentials {
            server_url: endpoint.url().to_owned(),
            username: login.username,
            session_token: Some(login.token),
            runner_token: Some(runner_token),
            enabled: true,
        };
        credentials::store(&path, &credentials)?;
        crate::log::info(format!(
            "connected to collab service {} as {}",
            credentials.server_url, credentials.username
        ));
        self.launch(credentials);
        self.status().await
    }

    /// Stop every collab connection. `forget` logs out and deletes the stored
    /// credentials; otherwise they are kept but disabled.
    pub async fn disconnect(&self, forget: bool) -> Result<CollabStatusSnapshot> {
        let path = self.credentials_path()?.to_path_buf();
        let credentials = {
            let mut state = self.lock();
            Self::stop_locked(&mut state);
            state.signed_in = false;
            state.last_error = None;
            if forget {
                state.credentials.take()
            } else {
                if let Some(credentials) = state.credentials.as_mut() {
                    credentials.enabled = false;
                }
                state.credentials.clone()
            }
        };
        if forget {
            if let Some(credentials) = &credentials {
                if let (Ok(endpoint), Some(token)) = (
                    Endpoint::parse(&credentials.server_url),
                    credentials.session_token.as_deref(),
                ) {
                    let _ = endpoint
                        .call_empty(Method::POST, "/auth/logout", Some(token), None::<&()>)
                        .await;
                }
            }
            credentials::remove(&path)?;
        } else if let Some(credentials) = &credentials {
            credentials::store(&path, credentials)?;
        }
        self.status().await
    }

    pub async fn link(&self, thread_id: &ThreadId) -> Result<Option<CollabLink>> {
        if self.inner.credentials_path.is_none() {
            return Ok(None);
        }
        Ok(self.db().collab_link(thread_id).await?)
    }

    async fn snapshot(
        &self,
        connection: &Connection,
        thread_id: &ThreadId,
        collab_thread_id: &str,
        link_token: Option<String>,
    ) -> Result<SharedThreadSnapshot> {
        let detail: ThreadDetail = connection
            .endpoint
            .call(
                Method::GET,
                &format!("/threads/{collab_thread_id}"),
                Some(connection.session()?),
                None::<&()>,
            )
            .await
            .map_err(|err| self.request_failed(connection, err))?;
        let mut family: Vec<ThreadId> = vec![thread_id.clone()];
        family.extend(
            self.db()
                .collab_family_of(thread_id)
                .await?
                .into_iter()
                .map(|row| row.thread_id),
        );
        let pending_publish = self.db().collab_outbox_pending_for(&family).await? as u32;
        Ok(SharedThreadSnapshot {
            thread_id: thread_id.clone(),
            collab_thread_id: collab_thread_id.to_owned(),
            title: detail.summary.title,
            owner: detail.summary.owner,
            access: detail.summary.access,
            members: detail.members,
            link_token,
            pending_publish,
        })
    }

    fn request_failed(&self, connection: &Connection, error: CollabError) -> DaemonError {
        if error == CollabError::Unauthorized {
            self.session_rejected(connection.generation);
        }
        collab_error(error)
    }

    /// The link of `thread_id` on the connected server.
    async fn linked(&self, connection: &Connection, thread_id: &ThreadId) -> Result<CollabLink> {
        let link = self
            .db()
            .collab_link(thread_id)
            .await?
            .ok_or_else(|| DaemonError::Invalid("this thread is not shared".into()))?;
        if link.server_url != connection.endpoint.url() {
            return Err(DaemonError::Invalid(format!(
                "this thread is shared on {}, not on the connected collab service",
                link.server_url
            )));
        }
        Ok(link)
    }

    pub async fn share_thread(
        &self,
        thread_id: &ThreadId,
        access: Option<CollabAccess>,
        invite: Vec<String>,
    ) -> Result<SharedThreadSnapshot> {
        let connection = self.require_connection()?;
        let session = connection.session()?.to_owned();
        for username in &invite {
            validate_path_segment(username, "username").map_err(DaemonError::Invalid)?;
        }
        let thread = self
            .db()
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        if thread.kind != ThreadKind::Top {
            return Err(DaemonError::Invalid(
                "share the top-level thread: its subthreads and subagents are shared with it"
                    .into(),
            ));
        }
        let link = match self.db().collab_link(thread_id).await? {
            Some(link) if link.server_url == connection.endpoint.url() => link,
            Some(link) => {
                return Err(DaemonError::Invalid(format!(
                    "this thread is already shared on {}",
                    link.server_url
                )));
            }
            None => {
                let title = thread
                    .title
                    .clone()
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| "Shared thread".into());
                let summary: ThreadSummary = connection
                    .endpoint
                    .call(
                        Method::POST,
                        "/threads",
                        Some(&session),
                        Some(&CreateThreadRequest {
                            title: truncate(&title, 190),
                        }),
                    )
                    .await
                    .map_err(|err| self.request_failed(&connection, err))?;
                validate_path_segment(&summary.thread_id, "shared thread id")
                    .map_err(DaemonError::Collab)?;
                let link = self
                    .db()
                    .link_collab_thread(thread_id, connection.endpoint.url(), &summary.thread_id)
                    .await?;
                // Start mirroring the log from here on; the history and the
                // family are published below.
                self.ensure_mirror(&link, connection.generation);
                link
            }
        };
        // Publish (or resume publishing) the whole history and family.
        // Idempotent: what was already queued is not queued again.
        self.sync_family(thread_id, &HashSet::new(), true).await?;
        let cid = link.collab_thread_id.clone();
        let mut link_token = None;
        if let Some(access) = access {
            let response: SetAccessResponse = connection
                .endpoint
                .call(
                    Method::PUT,
                    &format!("/threads/{cid}/access"),
                    Some(&session),
                    Some(&SetAccessRequest { access }),
                )
                .await
                .map_err(|err| self.request_failed(&connection, err))?;
            link_token = response.link_token;
            crate::log::info(format!(
                "shared thread {thread_id} access set to {}",
                access_label(access)
            ));
        }
        for username in invite {
            connection
                .endpoint
                .call_empty(
                    Method::POST,
                    &format!("/threads/{cid}/invites"),
                    Some(&session),
                    Some(&InviteRequest {
                        username: username.clone(),
                    }),
                )
                .await
                .map_err(|err| match err {
                    CollabError::NotFound => {
                        DaemonError::NotFound(format!("collab user `{username}`"))
                    }
                    other => self.request_failed(&connection, other),
                })?;
        }
        self.snapshot(&connection, thread_id, &cid, link_token)
            .await
    }

    pub async fn shared_thread(&self, thread_id: &ThreadId) -> Result<SharedThreadSnapshot> {
        let connection = self.require_connection()?;
        let link = self.linked(&connection, thread_id).await?;
        self.snapshot(&connection, thread_id, &link.collab_thread_id, None)
            .await
    }

    pub async fn remove_member(
        &self,
        thread_id: &ThreadId,
        username: &str,
    ) -> Result<SharedThreadSnapshot> {
        let connection = self.require_connection()?;
        validate_path_segment(username, "username").map_err(DaemonError::Invalid)?;
        let link = self.linked(&connection, thread_id).await?;
        connection
            .endpoint
            .call_empty(
                Method::DELETE,
                &format!("/threads/{}/members/{username}", link.collab_thread_id),
                Some(connection.session()?),
                None::<&()>,
            )
            .await
            .map_err(|err| self.request_failed(&connection, err))?;
        self.snapshot(&connection, thread_id, &link.collab_thread_id, None)
            .await
    }

    /// Join `collab_thread_id` into a new local thread of `workspace_id`.
    pub async fn join(
        &self,
        workspace_id: WorkspaceId,
        collab_thread_id: String,
        link_token: Option<String>,
    ) -> Result<(ThreadSnapshot, SharedThreadSnapshot)> {
        let connection = self.require_connection()?;
        let session = connection.session()?.to_owned();
        validate_path_segment(&collab_thread_id, "shared thread id")
            .map_err(DaemonError::Invalid)?;
        if let Some(link) = self
            .db()
            .collab_link_by_remote(connection.endpoint.url(), &collab_thread_id)
            .await?
        {
            return Err(DaemonError::Invalid(format!(
                "this shared thread is already open as thread {}",
                link.thread_id
            )));
        }
        let path = format!("/threads/{collab_thread_id}");
        let summary: ThreadSummary = match link_token {
            Some(token) => connection
                .endpoint
                .call(
                    Method::POST,
                    &format!("{path}/join"),
                    Some(&session),
                    Some(&JoinRequest {
                        link_token: token.trim().to_owned(),
                    }),
                )
                .await
                .map_err(|err| self.request_failed(&connection, err))?,
            None => {
                let detail: ThreadDetail = connection
                    .endpoint
                    .call(Method::GET, &path, Some(&session), None::<&()>)
                    .await
                    .map_err(|err| self.request_failed(&connection, err))?;
                detail.summary
            }
        };
        let thread = self
            .sessions()
            .create_thread(workspace_id, truncate(&summary.title, 110))
            .await?;
        let link = self
            .db()
            .link_collab_thread(&thread.id, connection.endpoint.url(), &collab_thread_id)
            .await?;
        self.ensure_mirror(&link, connection.generation);
        let shared = self
            .snapshot(&connection, &thread.id, &collab_thread_id, None)
            .await?;
        Ok((thread, shared))
    }

    pub async fn list_shared(&self) -> Result<Vec<SharedThreadListing>> {
        let connection = self.require_connection()?;
        let threads: Vec<ThreadSummary> = connection
            .endpoint
            .call(
                Method::GET,
                "/threads",
                Some(connection.session()?),
                None::<&()>,
            )
            .await
            .map_err(|err| self.request_failed(&connection, err))?;
        let links: HashMap<String, ThreadId> = self
            .db()
            .list_collab_links(connection.endpoint.url())
            .await?
            .into_iter()
            .map(|link| (link.collab_thread_id, link.thread_id))
            .collect();
        Ok(threads
            .into_iter()
            .map(|summary| SharedThreadListing {
                local_thread_id: links.get(&summary.thread_id).cloned(),
                collab_thread_id: summary.thread_id,
                title: summary.title,
                owner: summary.owner,
                access: summary.access,
                via: summary.via,
                last_seq: summary.last_seq,
            })
            .collect())
    }

    /// Stop mirroring a shared thread and its family here. Local history
    /// stays; sharing it again publishes everything to a new shared thread.
    pub async fn unlink(&self, thread_id: &ThreadId) -> Result<()> {
        self.credentials_path()?;
        if self.db().collab_link(thread_id).await?.is_none() {
            return Err(DaemonError::Invalid("this thread is not shared".into()));
        }
        let _serialized = self.inner.sync_lock.lock().await;
        let mut family: Vec<ThreadId> = vec![thread_id.clone()];
        family.extend(
            self.db()
                .collab_family_of(thread_id)
                .await?
                .into_iter()
                .map(|row| row.thread_id),
        );
        self.db().forget_collab_family(&family).await?;
        let mut state = self.lock();
        for id in &family {
            if let Some(mirror) = state.mirrors.remove(id) {
                mirror.task.abort();
            }
        }
        Ok(())
    }

    /// Other participants' changes snapshots of `thread_id`, newest first.
    pub async fn remote_changes(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<trama_protocol::RemoteChangesSnapshot>> {
        if self.inner.credentials_path.is_none() {
            return Ok(Vec::new());
        }
        let rows = self.db().collab_remote_changes(thread_id).await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let payload: serde_json::Value = serde_json::from_str(&row.payload_json).ok()?;
                Some(trama_protocol::RemoteChangesSnapshot {
                    author: row.author,
                    captured_at: payload
                        .get("captured_at")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or(row.captured_at),
                    base: payload
                        .get("base")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or_default(),
                    repos: payload
                        .get("repos")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or_default(),
                })
            })
            .collect())
    }

    /// The service answered again: retry pending items now instead of
    /// waiting out their backoff.
    pub(crate) async fn reset_outbox_backoff(&self) {
        if let Ok(rows) = self.db().collab_outbox_pending(1).await {
            if let Some(head) = rows.first() {
                if head.next_attempt_at > Utc::now() {
                    let _ = self
                        .db()
                        .collab_outbox_retry_at(head.id, Utc::now(), "reconnected")
                        .await;
                }
            }
        }
        self.kick_outbox();
    }

    pub fn set_draft(&self, thread_id: &ThreadId, text: String) -> Result<()> {
        let text = text.replace("\r\n", "\n");
        self.mirror_frame(
            thread_id,
            ClientFrame::Draft {
                text: truncate(&text, MAX_TURN_TEXT_BYTES - 4),
            },
        )
    }

    pub fn set_presence(&self, thread_id: &ThreadId, state: CollabPresenceState) -> Result<()> {
        if !matches!(
            state,
            CollabPresenceState::Active | CollabPresenceState::Idle
        ) {
            return Err(DaemonError::Invalid(
                "presence can only be set to active or idle".into(),
            ));
        }
        self.mirror_frame(thread_id, ClientFrame::Presence { state })
    }

    /// `SendPrompt` on a linked thread: record the turn as submitted by this
    /// daemon, then hand it to the service. This daemon's runner runs it.
    pub async fn submit_turn(&self, link: &CollabLink, text: String) -> Result<String> {
        let connection = self.require_connection().map_err(|_| {
            DaemonError::Collab(
                "this thread is shared: connect to the collab service to send. \
                 Turns of shared threads run through it so everyone sees them"
                    .into(),
            )
        })?;
        let session = connection.session()?.to_owned();
        if link.server_url != connection.endpoint.url() {
            return Err(DaemonError::Invalid(format!(
                "this thread is shared on {}, not on the connected collab service",
                link.server_url
            )));
        }
        let text = sanitize_text(&text, MAX_TURN_TEXT_BYTES, "the message")?;
        // The turn lands after the history it follows.
        self.flush_thread(&link.thread_id).await;
        let command_id = connection.new_command_id();
        self.db()
            .insert_collab_turn(&CollabTurn {
                command_id: command_id.clone(),
                thread_id: link.thread_id.clone(),
                collab_thread_id: link.collab_thread_id.clone(),
                text: text.clone(),
                state: turn_state::SUBMITTED.into(),
                created_at: Utc::now(),
            })
            .await?;
        let sent: std::result::Result<CommandReceipt, CollabError> = connection
            .endpoint
            .call(
                Method::POST,
                &format!("/threads/{}/send", link.collab_thread_id),
                Some(&session),
                Some(&SendRequest {
                    command_id: command_id.clone(),
                    text: Some(text),
                }),
            )
            .await;
        if let Err(err) = sent {
            // Never claimed later: the user sees the error and decides.
            let _ = self
                .db()
                .transition_collab_turn(
                    &command_id,
                    &[turn_state::SUBMITTED],
                    turn_state::SEND_FAILED,
                )
                .await;
            return Err(self.request_failed(&connection, err));
        }
        Ok(command_id)
    }

    /// A local comment was created: the family rescan publishes it (after
    /// the messages it may anchor to).
    pub fn publish_comment(&self, snapshot: CommentSnapshot) {
        self.signal(snapshot.thread_id);
    }

    pub fn publish_resolve(&self, comment: CommentSnapshot) {
        if let trama_protocol::CommentState::Resolved { resolved_at, .. } = comment.state {
            self.comment_changed(
                comment.thread_id,
                family::CommentChange::Resolved(comment.id, resolved_at),
            );
        }
    }

    pub fn publish_reopen(&self, comment: CommentSnapshot) {
        self.comment_changed(
            comment.thread_id,
            family::CommentChange::Reopened(comment.id),
        );
    }

    /// Deleted local comments: the root (replies go with it).
    pub fn publish_delete(&self, thread_id: ThreadId, comment_ids: Vec<CommentId>) {
        if let Some(root) = comment_ids.last().cloned() {
            self.comment_changed(thread_id, family::CommentChange::Deleted(root));
        }
    }

    /// A review subthread's verdict: published to the review thread by the
    /// family rescan (its parent's row is published as history).
    pub fn publish_verdict(&self, review_thread_id: ThreadId) {
        self.signal(review_thread_id);
    }

    pub(crate) fn signal(&self, thread_id: ThreadId) {
        let service = self.clone();
        tokio::spawn(async move {
            if let Some(root) = service.family_root(&thread_id).await {
                if let Err(err) = service.sync_family(&root, &HashSet::new(), false).await {
                    crate::log::warn(format!("shared thread not synced: {err}"));
                }
            }
        });
    }
}
