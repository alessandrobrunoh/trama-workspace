//! Shared threads (protocol v8, ADR 0008 and 0009).
//!
//! [`CollabStore`] keeps what the daemon reports about the collaboration
//! service: the account and runner status, which local threads are shared,
//! members and live presence, other people's drafts, turns queued for this
//! machine's runner, and live output of other people's turns. Views read it;
//! only the store sends collab commands. The pure pieces (join strings,
//! actor names, the draft debounce, live remote turns) are plain functions
//! and structs so they can be tested without a window.
//!
//! The execution rule is the daemon's, not the UI's: a turn always runs on
//! the machine of the person who pressed Send, with that person's own
//! `claude` login. Other participants only see its output.

pub(crate) mod account;
pub(crate) mod presence;
pub(crate) mod share_dialog;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use gpui_kit::*;
use trama_domain::agent::AgentEvent;
use trama_domain::ids::{ThreadId, WorkspaceId};
use trama_domain::message::{Actor, Role};
use trama_protocol::{
    ClientMessage, CollabEventKind, CollabPresenceState, CollabRunnerState, CollabStatusSnapshot,
    DaemonMessage, SharedThreadListing, SharedThreadSnapshot,
};

use crate::net::{DaemonClient, SendStatus};
use crate::share_dialog::ShareState;

/// Where `trama-collab serve` listens by default.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8787";
/// Prefix of the join strings the Share dialog copies.
pub const JOIN_PREFIX: &str = "trama-join:";
/// Pause in typing before the draft is relayed.
pub const DRAFT_DEBOUNCE: Duration = Duration::from_millis(350);
/// Actor id prefix of mirrored participants (`collab:bob`).
const REMOTE_ACTOR_PREFIX: &str = "collab:";
/// Longest live output kept per remote turn, in bytes.
const MAX_LIVE_TEXT: usize = 4_000;
/// Tool lines kept per remote turn.
const MAX_LIVE_TOOLS: usize = 6;

// -- join strings -------------------------------------------------------------

/// What a join string names: the service, the collab thread and, for link
/// access, the link token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinTarget {
    /// `None` when only an id (and token) was pasted: use the signed-in server.
    pub server_url: Option<String>,
    pub thread_id: String,
    pub link_token: Option<String>,
}

/// `trama-join:http://127.0.0.1:8787/<thread id>#<link token>`. Without a
/// token the string still names the thread, for people who were invited.
pub fn encode_join(server_url: &str, thread_id: &str, link_token: Option<&str>) -> String {
    let server = server_url.trim().trim_end_matches('/');
    match link_token {
        Some(token) => format!("{JOIN_PREFIX}{server}/{thread_id}#{token}"),
        None => format!("{JOIN_PREFIX}{server}/{thread_id}"),
    }
}

/// Accepts a join string from [`encode_join`], or a bare thread id followed
/// by an optional link token (separated by a space or `#`).
pub fn parse_join(input: &str) -> Result<JoinTarget, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Paste a join link, or a thread id and link token.".into());
    }
    if let Some(rest) = input.strip_prefix(JOIN_PREFIX) {
        let (location, token) = match rest.split_once('#') {
            Some((location, token)) => (location, Some(token)),
            None => (rest, None),
        };
        let (server, thread_id) = location
            .trim_end_matches('/')
            .rsplit_once('/')
            .ok_or_else(|| "The join link has no thread id.".to_string())?;
        let server_url = parse_server(server)?;
        return Ok(JoinTarget {
            server_url: Some(server_url),
            thread_id: parse_thread_id(thread_id)?,
            link_token: token.map(parse_token).transpose()?,
        });
    }
    let parts: Vec<&str> = input
        .split(|c: char| c.is_whitespace() || c == '#')
        .filter(|part| !part.is_empty())
        .collect();
    match parts.as_slice() {
        [thread_id] => Ok(JoinTarget {
            server_url: None,
            thread_id: parse_thread_id(thread_id)?,
            link_token: None,
        }),
        [thread_id, token] => Ok(JoinTarget {
            server_url: None,
            thread_id: parse_thread_id(thread_id)?,
            link_token: Some(parse_token(token)?),
        }),
        _ => Err("Paste the whole join link as copied from the Share dialog.".into()),
    }
}

fn parse_server(raw: &str) -> Result<String, String> {
    let rest = raw
        .strip_prefix("http://")
        .or_else(|| raw.strip_prefix("https://"))
        .ok_or_else(|| {
            "The join link must name an https:// (or local http://) server.".to_string()
        })?;
    let ok = !rest.is_empty()
        && !rest.contains(['/', '@', '?', '#'])
        && !rest.chars().any(char::is_whitespace);
    if ok {
        Ok(raw.to_string())
    } else {
        Err("The server in the join link is not a host and port.".into())
    }
}

fn parse_thread_id(raw: &str) -> Result<String, String> {
    if valid_name(raw) {
        Ok(raw.to_string())
    } else {
        Err("The thread id is malformed.".into())
    }
}

fn parse_token(raw: &str) -> Result<String, String> {
    let ok = (16..=256).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_alphanumeric());
    if ok {
        Ok(raw.to_string())
    } else {
        Err("The link token is malformed; copy the link again.".into())
    }
}

/// Usernames and thread ids: 1-64 ASCII letters, digits, `.`, `_`, `-`, `@`
/// (the service's rule).
pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'))
}

/// Same server, ignoring case and a trailing slash.
pub fn same_server(a: &str, b: &str) -> bool {
    a.trim()
        .trim_end_matches('/')
        .eq_ignore_ascii_case(b.trim().trim_end_matches('/'))
}

// -- names --------------------------------------------------------------------

/// Who wrote a transcript message, as the transcript labels it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Speaker {
    /// This machine's user.
    Me,
    /// Another participant of a shared thread.
    Remote(String),
    Agent,
    System,
}

impl Speaker {
    pub fn of(actor: &Actor) -> Self {
        match actor {
            Actor::Human { id } => match id.strip_prefix(REMOTE_ACTOR_PREFIX) {
                Some(name) if !name.is_empty() => Self::Remote(name.to_string()),
                _ => Self::Me,
            },
            Actor::Agent { .. } => Self::Agent,
            Actor::System => Self::System,
        }
    }

    /// The label above a message of this role.
    pub fn label(&self, role: Role) -> String {
        match (self, role) {
            (Self::Me, _) => "You".into(),
            (Self::Remote(name), Role::Assistant) => format!("{name}'s Claude"),
            (Self::Remote(name), _) => name.clone(),
            (Self::Agent, _) => "Claude".into(),
            (Self::System, _) => "Trama".into(),
        }
    }
}

/// The participant behind a mirrored message, if any.
pub fn remote_username(actor: &Actor) -> Option<&str> {
    match actor {
        Actor::Human { id } => id
            .strip_prefix(REMOTE_ACTOR_PREFIX)
            .filter(|name| !name.is_empty()),
        _ => None,
    }
}

/// One or two letters for an avatar: `alice.smith` → `AS`, `bob` → `B`.
pub fn initials(name: &str) -> String {
    let local = name.split('@').next().unwrap_or(name);
    let mut parts = local
        .split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty());
    let first = parts.next().and_then(|part| part.chars().next());
    let second = parts.next().and_then(|part| part.chars().next());
    match (first, second) {
        (Some(a), Some(b)) => format!("{a}{b}").to_uppercase(),
        (Some(a), None) => a.to_uppercase().to_string(),
        _ => "?".into(),
    }
}

/// A stable hue in `0.0..1.0` for a username's avatar.
pub fn avatar_hue(name: &str) -> f32 {
    let hash = name
        .to_lowercase()
        .bytes()
        .fold(2166136261u32, |hash, byte| {
            (hash ^ byte as u32).wrapping_mul(16777619)
        });
    (hash % 360) as f32 / 360.
}

// -- draft debounce -------------------------------------------------------------

/// Relays the composer draft of the selected shared thread after a pause.
///
/// [`Self::edit`] returns a generation to fire after [`DRAFT_DEBOUNCE`];
/// only the newest generation sends, and only when the text changed since
/// the last relay.
#[derive(Debug, Default)]
pub struct DraftDebounce {
    thread: Option<ThreadId>,
    pending: Option<String>,
    last_sent: String,
    generation: u64,
}

impl DraftDebounce {
    /// Follow the selected thread (`None` when it is not shared). Returns
    /// the message clearing the draft left in the previous thread.
    pub fn retarget(&mut self, thread: Option<ThreadId>) -> Option<(ThreadId, String)> {
        if self.thread == thread {
            return None;
        }
        let clear = self
            .thread
            .take()
            .filter(|_| !self.last_sent.is_empty())
            .map(|previous| (previous, String::new()));
        self.thread = thread;
        self.pending = None;
        self.last_sent.clear();
        self.generation += 1;
        clear
    }

    pub fn thread(&self) -> Option<&ThreadId> {
        self.thread.as_ref()
    }

    /// The composer changed. `None` when there is nothing to schedule.
    pub fn edit(&mut self, text: &str) -> Option<u64> {
        self.thread.as_ref()?;
        let text = text.trim_end();
        if self.pending.is_none() && text == self.last_sent {
            return None;
        }
        self.pending = Some(text.to_string());
        self.generation += 1;
        Some(self.generation)
    }

    /// The pause after `generation` elapsed: the draft to relay, if any.
    pub fn fire(&mut self, generation: u64) -> Option<(ThreadId, String)> {
        if generation != self.generation {
            return None;
        }
        let text = self.pending.take()?;
        if text == self.last_sent {
            return None;
        }
        self.last_sent = text.clone();
        Some((self.thread.clone()?, text))
    }
}

// -- live remote turns ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePhase {
    /// Sent; waiting for the author's own runner to claim it.
    Waiting,
    /// Running on the author's machine.
    Running,
}

/// One tool call of a remote turn, as a one-line summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTool {
    pub tool_use_id: String,
    pub name: String,
    pub summary: String,
    pub finished: bool,
    pub is_error: bool,
}

/// Another participant's turn, streamed live from their runner. Not stored:
/// the final answer arrives as a mirrored message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTurn {
    pub author: String,
    pub command_id: String,
    pub phase: RemotePhase,
    pub text: String,
    pub tools: Vec<RemoteTool>,
}

impl RemoteTurn {
    pub fn new(author: &str, command_id: &str, phase: RemotePhase) -> Self {
        Self {
            author: author.to_string(),
            command_id: command_id.to_string(),
            phase,
            text: String::new(),
            tools: Vec::new(),
        }
    }

    /// Apply one live event. Returns `false` once the turn is over.
    pub fn apply(&mut self, event: &AgentEvent) -> bool {
        self.phase = RemotePhase::Running;
        match event {
            AgentEvent::TextDelta { text } => {
                self.text.push_str(text);
                if self.text.len() > MAX_LIVE_TEXT {
                    let mut cut = self.text.len() - MAX_LIVE_TEXT;
                    while !self.text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    self.text.drain(..cut);
                }
            }
            AgentEvent::ToolCallStarted {
                tool_use_id,
                name,
                input_json,
            } => {
                self.tools.push(RemoteTool {
                    tool_use_id: tool_use_id.clone(),
                    name: name.clone(),
                    summary: crate::chat::tool_summary(name, input_json),
                    finished: false,
                    is_error: false,
                });
                if self.tools.len() > MAX_LIVE_TOOLS {
                    self.tools.remove(0);
                }
            }
            AgentEvent::ToolCallFinished {
                tool_use_id,
                is_error,
                ..
            } => {
                if let Some(tool) = self
                    .tools
                    .iter_mut()
                    .find(|tool| &tool.tool_use_id == tool_use_id)
                {
                    tool.finished = true;
                    tool.is_error = *is_error;
                }
            }
            AgentEvent::TurnCompleted { .. } | AgentEvent::Failed { .. } => return false,
            _ => {}
        }
        true
    }
}

/// Track other people's turns of one thread from structured events.
pub fn apply_remote_event(
    turns: &mut Vec<RemoteTurn>,
    kind: CollabEventKind,
    author: Option<&str>,
    command_id: Option<&str>,
) {
    let (Some(author), Some(command_id)) = (author, command_id) else {
        return;
    };
    let position = turns.iter().position(|turn| turn.command_id == command_id);
    match kind {
        CollabEventKind::MessageAppended if position.is_none() => {
            turns.push(RemoteTurn::new(author, command_id, RemotePhase::Waiting));
        }
        CollabEventKind::CommandClaimed => match position {
            Some(index) => turns[index].phase = RemotePhase::Running,
            None => turns.push(RemoteTurn::new(author, command_id, RemotePhase::Running)),
        },
        CollabEventKind::CommandFinished | CollabEventKind::CommandAmbiguous => {
            if let Some(index) = position {
                turns.remove(index);
            }
        }
        _ => {}
    }
}

// -- the store ------------------------------------------------------------------

/// What a pending collab command is for, so an uncorrelated daemon `Error`
/// lands next to the form that caused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollabOp {
    Connect,
    Disconnect,
    Share,
    Join,
    List,
}

/// The sidebar's view of shared threads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SidebarCollab {
    pub signed_in: bool,
    /// Local threads linked to a shared thread.
    pub shared: HashSet<ThreadId>,
    /// Shared threads someone else owns.
    pub shared_with_me: Vec<SharedThreadListing>,
}

/// What the transcript of one thread shows about sharing.
#[derive(Debug, Clone, Default)]
pub struct ThreadCollab {
    pub linked: bool,
    pub revoked: bool,
    /// My turns submitted and not started yet.
    pub queued: bool,
    pub runner_online: bool,
    /// Others' non-empty drafts, by username.
    pub drafts: Vec<(String, String)>,
    pub remote_turns: Vec<RemoteTurn>,
}

pub struct CollabStore {
    client: DaemonClient,
    pub status: Option<CollabStatusSnapshot>,
    shared: HashMap<ThreadId, SharedThreadSnapshot>,
    listings: Vec<SharedThreadListing>,
    linked: HashSet<ThreadId>,
    presence: HashMap<ThreadId, HashMap<String, CollabPresenceState>>,
    drafts: HashMap<ThreadId, BTreeMap<String, String>>,
    queued: HashMap<ThreadId, Vec<String>>,
    remote_turns: HashMap<ThreadId, Vec<RemoteTurn>>,
    revoked: HashSet<ThreadId>,
    pending: Option<CollabOp>,
    /// Shown in the account dialog.
    pub account_error: Option<String>,
    /// Shown in the join dialog.
    pub join_error: Option<String>,
    pub share: ShareState,
    selected: Option<ThreadId>,
    window_active: bool,
    presence_sent: Option<(ThreadId, CollabPresenceState)>,
    debounce: DraftDebounce,
    _debounce_task: Option<Task<()>>,
}

impl CollabStore {
    pub fn new(client: DaemonClient) -> Self {
        Self {
            client,
            status: None,
            shared: HashMap::new(),
            listings: Vec::new(),
            linked: HashSet::new(),
            presence: HashMap::new(),
            drafts: HashMap::new(),
            queued: HashMap::new(),
            remote_turns: HashMap::new(),
            revoked: HashSet::new(),
            pending: None,
            account_error: None,
            join_error: None,
            share: ShareState::default(),
            selected: None,
            window_active: true,
            presence_sent: None,
            debounce: DraftDebounce::default(),
            _debounce_task: None,
        }
    }

    fn send(&self, message: ClientMessage) -> bool {
        self.client.send(message) == SendStatus::Enqueued
    }

    pub fn signed_in(&self) -> bool {
        self.status.as_ref().is_some_and(|status| status.signed_in)
    }

    pub fn username(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|status| status.signed_in)
            .and_then(|status| status.username.as_deref())
    }

    pub fn server_url(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|status| status.server_url.as_deref())
    }

    pub fn runner(&self) -> CollabRunnerState {
        self.status
            .as_ref()
            .map_or(CollabRunnerState::Off, |status| status.runner)
    }

    pub fn is_pending(&self, op: CollabOp) -> bool {
        self.pending == Some(op)
    }

    pub fn is_linked(&self, thread: &ThreadId) -> bool {
        self.linked.contains(thread)
    }

    pub fn snapshot(&self, thread: &ThreadId) -> Option<&SharedThreadSnapshot> {
        self.shared.get(thread)
    }

    pub fn presence(&self, thread: &ThreadId) -> Option<&HashMap<String, CollabPresenceState>> {
        self.presence.get(thread)
    }

    pub fn typing(&self, thread: &ThreadId) -> HashSet<String> {
        self.drafts
            .get(thread)
            .map(|drafts| {
                drafts
                    .iter()
                    .filter(|(_, text)| !text.trim().is_empty())
                    .map(|(name, _)| name.to_lowercase())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn sidebar(&self) -> SidebarCollab {
        let me = self.username().map(str::to_lowercase);
        SidebarCollab {
            signed_in: self.signed_in(),
            shared: self.linked.clone(),
            shared_with_me: self
                .listings
                .iter()
                .filter(|listing| Some(listing.owner.to_lowercase()) != me)
                .cloned()
                .collect(),
        }
    }

    pub fn thread_view(&self, thread: &ThreadId) -> ThreadCollab {
        ThreadCollab {
            linked: self.linked.contains(thread),
            revoked: self.revoked.contains(thread),
            queued: self.queued.get(thread).is_some_and(|ids| !ids.is_empty()),
            runner_online: self.runner() == CollabRunnerState::Online,
            drafts: self
                .drafts
                .get(thread)
                .map(|drafts| {
                    drafts
                        .iter()
                        .filter(|(_, text)| !text.trim().is_empty())
                        .map(|(name, text)| (name.clone(), text.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            remote_turns: self.remote_turns.get(thread).cloned().unwrap_or_default(),
        }
    }

    // -- commands -----------------------------------------------------------

    /// After every (re)connect to the daemon.
    pub fn on_connected(&mut self, cx: &mut Context<Self>) {
        self.presence_sent = None;
        self.send(ClientMessage::GetCollabStatus);
        cx.notify();
    }

    pub fn refresh_shared(&mut self, cx: &mut Context<Self>) {
        if self.signed_in() && self.pending.is_none() && self.send(ClientMessage::ListSharedThreads)
        {
            self.pending = Some(CollabOp::List);
            cx.notify();
        }
    }

    pub fn connect(
        &mut self,
        server_url: String,
        username: String,
        password: String,
        runner_token: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.account_error = None;
        let sent = self.send(ClientMessage::ConnectCollab {
            server_url,
            username,
            password,
            runner_token,
        });
        if sent {
            self.pending = Some(CollabOp::Connect);
        } else {
            self.account_error = Some("The daemon is not connected.".into());
        }
        cx.notify();
    }

    pub fn disconnect(&mut self, forget: bool, cx: &mut Context<Self>) {
        self.account_error = None;
        if self.send(ClientMessage::DisconnectCollab { forget }) {
            self.pending = Some(CollabOp::Disconnect);
        }
        cx.notify();
    }

    /// Open the Share dialog's state for `thread`.
    pub fn open_share(&mut self, thread: ThreadId, title: String, cx: &mut Context<Self>) {
        let linked = self.linked.contains(&thread);
        let message = self.share.open(thread, title, linked, self.signed_in());
        self.dispatch_share(message, cx);
    }

    /// Send what a Share dialog action asked for.
    pub fn dispatch_share(&mut self, message: Option<ClientMessage>, cx: &mut Context<Self>) {
        if let Some(message) = message {
            if self.send(message) {
                self.pending = Some(CollabOp::Share);
            } else {
                self.share
                    .apply_error("The daemon is not connected.".into());
            }
        }
        cx.notify();
    }

    pub fn join(
        &mut self,
        workspace: Option<WorkspaceId>,
        target: &JoinTarget,
        cx: &mut Context<Self>,
    ) -> bool {
        self.join_error = join_problem(
            self.signed_in(),
            self.server_url(),
            workspace.is_some(),
            target,
        );
        let Some(workspace_id) = workspace.filter(|_| self.join_error.is_none()) else {
            cx.notify();
            return false;
        };
        let sent = self.send(ClientMessage::JoinSharedThread {
            workspace_id,
            collab_thread_id: target.thread_id.clone(),
            link_token: target.link_token.clone(),
        });
        if sent {
            self.pending = Some(CollabOp::Join);
        } else {
            self.join_error = Some("The daemon is not connected.".into());
        }
        cx.notify();
        sent
    }

    /// The selected thread changed.
    pub fn set_selected(&mut self, thread: Option<ThreadId>, cx: &mut Context<Self>) {
        if self.selected == thread {
            return;
        }
        // Live state only reaches the selected thread's subscription, so
        // what was shown for the previous one may go stale: drop it.
        if let Some(previous) = self.selected.take() {
            self.drafts.remove(&previous);
            self.presence.remove(&previous);
            self.remote_turns.remove(&previous);
        }
        self.selected = thread.clone();
        let shared = thread.filter(|id| self.linked.contains(id));
        if let Some((previous, text)) = self.debounce.retarget(shared.clone()) {
            self.send(ClientMessage::SetCollabDraft {
                thread_id: previous,
                text,
            });
        }
        if let Some(id) = &shared {
            self.send(ClientMessage::GetSharedThread {
                thread_id: id.clone(),
            });
        }
        self.sync_presence();
        cx.notify();
    }

    pub fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.window_active != active {
            self.window_active = active;
            self.sync_presence();
            if active {
                self.refresh_shared(cx);
            }
        }
    }

    /// `active` in the selected shared thread while the window is focused;
    /// `idle` when leaving it or when the window loses focus.
    fn sync_presence(&mut self) {
        let wanted = self
            .selected
            .clone()
            .filter(|id| self.linked.contains(id) && !self.revoked.contains(id))
            .map(|id| {
                let state = if self.window_active {
                    CollabPresenceState::Active
                } else {
                    CollabPresenceState::Idle
                };
                (id, state)
            });
        if wanted == self.presence_sent {
            return;
        }
        if let Some((previous, _)) = &self.presence_sent
            && wanted.as_ref().map(|(id, _)| id) != Some(previous)
            && self.linked.contains(previous)
        {
            self.send(ClientMessage::SetCollabPresence {
                thread_id: previous.clone(),
                state: CollabPresenceState::Idle,
            });
        }
        if let Some((id, state)) = &wanted {
            self.send(ClientMessage::SetCollabPresence {
                thread_id: id.clone(),
                state: *state,
            });
        }
        self.presence_sent = wanted;
    }

    /// The composer text changed; relay it after a pause.
    pub fn note_draft(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(generation) = self.debounce.edit(text) else {
            return;
        };
        self._debounce_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DRAFT_DEBOUNCE).await;
            let _ = this.update(cx, |this, _| {
                if let Some((thread_id, text)) = this.debounce.fire(generation) {
                    this.send(ClientMessage::SetCollabDraft { thread_id, text });
                }
            });
        }));
    }

    // -- daemon messages ----------------------------------------------------

    /// Apply a daemon message. Returns `true` when it is fully handled here;
    /// transcript messages and errors of other requests pass through.
    pub fn apply(&mut self, message: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        let handled = match message {
            DaemonMessage::CollabStatus(status) => {
                let was_signed_in = self.signed_in();
                if matches!(self.pending, Some(CollabOp::Connect | CollabOp::Disconnect)) {
                    self.pending = None;
                }
                self.account_error = None;
                self.status = Some(status.clone());
                if status.signed_in {
                    if !was_signed_in || self.listings.is_empty() {
                        self.refresh_shared(cx);
                    }
                } else {
                    self.listings.clear();
                }
                true
            }
            DaemonMessage::SharedThread(snapshot) => {
                if self.pending == Some(CollabOp::Share) {
                    self.pending = None;
                }
                self.note_snapshot(snapshot.clone());
                let server = self.server_url().map(str::to_string);
                self.share.apply_snapshot(snapshot, server.as_deref());
                true
            }
            DaemonMessage::SharedThreadJoined { shared, .. } => {
                if self.pending == Some(CollabOp::Join) {
                    self.pending = None;
                }
                self.join_error = None;
                self.note_snapshot(shared.clone());
                self.refresh_shared(cx);
                // The workspace adds and opens the new thread.
                false
            }
            DaemonMessage::SharedThreads(listings) => {
                if self.pending == Some(CollabOp::List) {
                    self.pending = None;
                }
                self.listings = listings.clone();
                for listing in listings {
                    if let Some(id) = &listing.local_thread_id {
                        self.linked.insert(id.clone());
                    }
                }
                self.sync_selected_link();
                true
            }
            DaemonMessage::SharedThreadUnlinked { thread_id } => {
                self.forget_thread(thread_id);
                true
            }
            DaemonMessage::CollabTurnQueued {
                thread_id,
                command_id,
            } => {
                self.queued
                    .entry(thread_id.clone())
                    .or_default()
                    .push(command_id.clone());
                true
            }
            DaemonMessage::CollabEvent { thread_id, event } => {
                let turns = self.remote_turns.entry(thread_id.clone()).or_default();
                apply_remote_event(
                    turns,
                    event.kind,
                    event.author.as_deref(),
                    event.command_id.as_deref(),
                );
                if matches!(
                    event.kind,
                    CollabEventKind::MemberJoined
                        | CollabEventKind::MemberRemoved
                        | CollabEventKind::AccessChanged
                ) {
                    self.send(ClientMessage::GetSharedThread {
                        thread_id: thread_id.clone(),
                    });
                }
                true
            }
            DaemonMessage::CollabAgentEvent {
                thread_id,
                author,
                command_id,
                event,
            } => {
                let turns = self.remote_turns.entry(thread_id.clone()).or_default();
                let index = match turns.iter().position(|turn| &turn.command_id == command_id) {
                    Some(index) => index,
                    None => {
                        turns.push(RemoteTurn::new(author, command_id, RemotePhase::Running));
                        turns.len() - 1
                    }
                };
                if !turns[index].apply(event) {
                    turns.remove(index);
                }
                true
            }
            DaemonMessage::CollabDraft {
                thread_id,
                username,
                text,
            } => {
                let drafts = self.drafts.entry(thread_id.clone()).or_default();
                if text.trim().is_empty() {
                    drafts.remove(username);
                } else {
                    drafts.insert(username.clone(), text.clone());
                }
                true
            }
            DaemonMessage::CollabPresence {
                thread_id,
                username,
                state,
            } => {
                self.presence
                    .entry(thread_id.clone())
                    .or_default()
                    .insert(username.to_lowercase(), *state);
                if *state == CollabPresenceState::Offline
                    && let Some(drafts) = self.drafts.get_mut(thread_id)
                {
                    drafts.remove(username);
                }
                true
            }
            DaemonMessage::CollabAccessRevoked { thread_id } => {
                // Mirroring stopped: stop relaying drafts and presence too.
                self.forget_thread(thread_id);
                self.revoked.insert(thread_id.clone());
                if self.share.thread() == Some(thread_id) {
                    self.share
                        .apply_error("You no longer have access to this shared thread.".into());
                }
                self.refresh_shared(cx);
                true
            }
            DaemonMessage::MessageAppended { thread_id, message } => {
                if let Some(author) = remote_username(&message.actor) {
                    if message.role == Role::Assistant
                        && let Some(turns) = self.remote_turns.get_mut(thread_id)
                    {
                        turns.retain(|turn| !turn.author.eq_ignore_ascii_case(author));
                    }
                } else if message.role == Role::User {
                    self.queued.remove(thread_id);
                }
                false
            }
            DaemonMessage::AgentEventEnvelope { thread_id, .. } => {
                self.queued.remove(thread_id);
                false
            }
            DaemonMessage::Error { message, .. } => match self.pending.take() {
                Some(CollabOp::Connect | CollabOp::Disconnect) => {
                    self.account_error = Some(message.clone());
                    true
                }
                Some(CollabOp::Share) => {
                    self.share.apply_error(message.clone());
                    true
                }
                Some(CollabOp::Join) => {
                    self.join_error = Some(message.clone());
                    true
                }
                Some(CollabOp::List) => true,
                None => false,
            },
            _ => false,
        };
        if handled
            || matches!(
                message,
                DaemonMessage::MessageAppended { .. } | DaemonMessage::SharedThreadJoined { .. }
            )
        {
            cx.notify();
        }
        handled
    }

    fn note_snapshot(&mut self, snapshot: SharedThreadSnapshot) {
        self.linked.insert(snapshot.thread_id.clone());
        self.revoked.remove(&snapshot.thread_id);
        self.shared.insert(snapshot.thread_id.clone(), snapshot);
        self.sync_selected_link();
    }

    /// A thread became shared while selected: start relaying.
    fn sync_selected_link(&mut self) {
        let shared = self.selected.clone().filter(|id| self.linked.contains(id));
        if self.debounce.thread() != shared.as_ref() {
            if let Some((previous, text)) = self.debounce.retarget(shared.clone()) {
                self.send(ClientMessage::SetCollabDraft {
                    thread_id: previous,
                    text,
                });
            }
            // Became shared while selected: load members for the avatars.
            if let Some(id) = shared
                && !self.shared.contains_key(&id)
            {
                self.send(ClientMessage::GetSharedThread { thread_id: id });
            }
        }
        self.sync_presence();
    }

    fn forget_thread(&mut self, thread: &ThreadId) {
        self.linked.remove(thread);
        self.shared.remove(thread);
        self.drafts.remove(thread);
        self.presence.remove(thread);
        self.remote_turns.remove(thread);
        self.queued.remove(thread);
        self.sync_selected_link();
    }
}

/// Why a join cannot be sent, or `None`.
pub fn join_problem(
    signed_in: bool,
    server_url: Option<&str>,
    has_workspace: bool,
    target: &JoinTarget,
) -> Option<String> {
    if !signed_in {
        return Some("Sign in to the collaboration server first.".into());
    }
    if let (Some(link), Some(mine)) = (target.server_url.as_deref(), server_url)
        && !same_server(link, mine)
    {
        return Some(format!(
            "This link is for {link}, but you are signed in to {mine}. Sign in to that server first."
        ));
    }
    if !has_workspace {
        return Some("Select a project first; the shared thread joins it.".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        Actor, AgentEvent, CollabEventKind, DraftDebounce, JoinTarget, RemotePhase, Role, Speaker,
        ThreadId, apply_remote_event, avatar_hue, encode_join, initials, join_problem, parse_join,
        remote_username,
    };
    use trama_domain::ids::AgentSessionId;

    #[test]
    fn join_strings_round_trip() {
        let token = "a".repeat(64);
        let link = encode_join("http://127.0.0.1:8787/", "0123abcd", Some(&token));
        assert_eq!(
            link,
            format!("trama-join:http://127.0.0.1:8787/0123abcd#{token}")
        );
        assert_eq!(
            parse_join(&format!("  {link}\n")),
            Ok(JoinTarget {
                server_url: Some("http://127.0.0.1:8787".into()),
                thread_id: "0123abcd".into(),
                link_token: Some(token.clone()),
            })
        );
        let invite = encode_join("http://localhost:9000", "t1", None);
        assert_eq!(
            parse_join(&invite),
            Ok(JoinTarget {
                server_url: Some("http://localhost:9000".into()),
                thread_id: "t1".into(),
                link_token: None,
            })
        );
    }

    #[test]
    fn bare_ids_and_tokens_are_accepted() {
        let token = "b".repeat(32);
        for input in [format!("t1 {token}"), format!("t1#{token}")] {
            assert_eq!(
                parse_join(&input),
                Ok(JoinTarget {
                    server_url: None,
                    thread_id: "t1".into(),
                    link_token: Some(token.clone()),
                })
            );
        }
        assert_eq!(parse_join("t1").unwrap().link_token, None);
    }

    #[test]
    fn malformed_join_strings_are_rejected() {
        let token = "c".repeat(64);
        for input in [
            String::new(),
            "   ".into(),
            format!("trama-join:ftp://host/t1#{token}"),
            format!("trama-join:http:///t1#{token}"),
            format!("trama-join:http://user@host:1/t1#{token}"),
            format!("trama-join:http://host:1/a/t1#{token}"),
            format!("trama-join:http://host:1/#{token}"),
            "trama-join:http://host:1/t1#short".into(),
            format!("trama-join:http://host:1/t1#{token}!"),
            "trama-join:t1".into(),
            "bad/id".into(),
            format!("t1 {token} extra"),
            "x".repeat(65),
        ] {
            assert!(parse_join(&input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn join_checks_sign_in_server_and_project() {
        let target = parse_join(&encode_join("http://127.0.0.1:8787", "t1", None)).unwrap();
        assert!(join_problem(false, None, true, &target).is_some());
        assert!(join_problem(true, Some("http://127.0.0.1:9999"), true, &target).is_some());
        assert!(join_problem(true, Some("HTTP://127.0.0.1:8787/"), false, &target).is_some());
        assert_eq!(
            join_problem(true, Some("http://127.0.0.1:8787"), true, &target),
            None
        );
    }

    #[test]
    fn actors_map_to_display_names() {
        let me = Actor::Human { id: "local".into() };
        let bob = Actor::Human {
            id: "collab:bob".into(),
        };
        let empty = Actor::Human {
            id: "collab:".into(),
        };
        let agent = Actor::Agent {
            session: AgentSessionId::from("s"),
        };
        assert_eq!(Speaker::of(&me), Speaker::Me);
        assert_eq!(Speaker::of(&bob), Speaker::Remote("bob".into()));
        assert_eq!(Speaker::of(&empty), Speaker::Me);
        assert_eq!(Speaker::of(&agent), Speaker::Agent);
        assert_eq!(Speaker::of(&Actor::System), Speaker::System);
        assert_eq!(Speaker::of(&me).label(Role::User), "You");
        assert_eq!(Speaker::of(&bob).label(Role::User), "bob");
        assert_eq!(Speaker::of(&bob).label(Role::Assistant), "bob's Claude");
        assert_eq!(Speaker::of(&agent).label(Role::Assistant), "Claude");
        assert_eq!(remote_username(&bob), Some("bob"));
        assert_eq!(remote_username(&me), None);
        assert_eq!(remote_username(&empty), None);
    }

    #[test]
    fn initials_and_hues_are_stable() {
        assert_eq!(initials("alice.smith"), "AS");
        assert_eq!(initials("bob"), "B");
        assert_eq!(initials("carol_ann@example.com"), "CA");
        assert_eq!(initials("--"), "?");
        assert_eq!(avatar_hue("Bob"), avatar_hue("bob"));
        assert!((0.0..1.0).contains(&avatar_hue("dave")));
    }

    #[test]
    fn draft_debounce_sends_only_the_latest_change() {
        let thread = ThreadId::from("t");
        let mut debounce = DraftDebounce::default();
        assert_eq!(debounce.edit("ignored, no shared thread"), None);
        assert_eq!(debounce.retarget(Some(thread.clone())), None);

        let first = debounce.edit("h").unwrap();
        let second = debounce.edit("hello").unwrap();
        assert_eq!(debounce.fire(first), None, "superseded");
        assert_eq!(
            debounce.fire(second),
            Some((thread.clone(), "hello".into()))
        );
        assert_eq!(debounce.fire(second), None, "fires once");
        assert_eq!(debounce.edit("hello  "), None, "no change after trimming");

        // Typing then reverting to the sent text sends nothing.
        let again = debounce.edit("hello!").unwrap();
        let back = debounce.edit("hello").unwrap();
        assert_eq!(debounce.fire(again), None);
        assert_eq!(debounce.fire(back), None);

        // Clearing is relayed, and leaving a thread with a live draft clears it.
        let typed = debounce.edit("draft").unwrap();
        assert!(debounce.fire(typed).is_some());
        let other = ThreadId::from("u");
        assert_eq!(
            debounce.retarget(Some(other.clone())),
            Some((thread.clone(), String::new()))
        );
        assert_eq!(debounce.retarget(None), None, "nothing sent in `u`");
        let stale = debounce.edit("x");
        assert_eq!(stale, None, "no thread, nothing scheduled");
    }

    #[test]
    fn remote_turns_follow_events_and_live_output() {
        let mut turns = Vec::new();
        apply_remote_event(
            &mut turns,
            CollabEventKind::MessageAppended,
            Some("bob"),
            Some("c1"),
        );
        assert_eq!(turns[0].phase, RemotePhase::Waiting);
        apply_remote_event(
            &mut turns,
            CollabEventKind::CommandClaimed,
            Some("bob"),
            Some("c1"),
        );
        assert_eq!(turns[0].phase, RemotePhase::Running);
        assert!(turns[0].apply(&AgentEvent::TextDelta { text: "Hi".into() }));
        assert!(turns[0].apply(&AgentEvent::ToolCallStarted {
            tool_use_id: "u1".into(),
            name: "Read".into(),
            input_json: serde_json::json!({"file_path": "src/main.rs"}),
        }));
        assert!(turns[0].apply(&AgentEvent::ToolCallFinished {
            tool_use_id: "u1".into(),
            output_json: serde_json::json!("ok"),
            is_error: false,
        }));
        assert_eq!(turns[0].text, "Hi");
        assert!(turns[0].tools[0].finished);
        assert!(!turns[0].apply(&AgentEvent::TurnCompleted {
            num_turns: 1,
            cost_usd_micros: None,
        }));
        apply_remote_event(
            &mut turns,
            CollabEventKind::CommandFinished,
            Some("bob"),
            Some("c1"),
        );
        assert!(turns.is_empty());
        apply_remote_event(
            &mut turns,
            CollabEventKind::MessageAppended,
            None,
            Some("c2"),
        );
        assert!(turns.is_empty(), "events without an author are ignored");
    }
}
