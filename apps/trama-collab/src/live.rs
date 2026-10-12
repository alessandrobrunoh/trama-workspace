//! In-process live fan-out. Nothing here is durable: the event log in SQLite
//! is the source of truth, drafts and presence are ephemeral by design.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

use tokio::sync::broadcast;
use trama_collab_protocol::{EventEnvelope, PendingTurn, PresenceState, ServerFrame};

use crate::AuthError;

/// Per-thread live channel capacity. A subscriber that falls further behind
/// receives `Lagged` and re-reads the gap from the durable log.
const THREAD_CHANNEL_CAPACITY: usize = 1024;
const RUNNER_CHANNEL_CAPACITY: usize = 256;
/// Open WebSocket connections (client + runner) allowed per user.
/// A shared family keeps one client socket per thread (root, subthreads,
/// subagents), so this is well above a single thread's needs.
pub(crate) const MAX_SOCKETS_PER_USER: usize = 128;
/// HTTP mutations allowed per user per window.
const MAX_MUTATIONS_PER_WINDOW: usize = 240;
const MUTATION_WINDOW_SECS: i64 = 60;
const MAX_LIMITER_KEYS: usize = 10_000;

pub(crate) enum Live {
    /// A committed durable event.
    Event(EventEnvelope),
    /// An ephemeral frame (draft, presence).
    Frame(ServerFrame),
    /// Membership or access changed; subscribers re-check before continuing.
    AccessChanged,
}

/// Side effects of a committed write, published while the write lock is
/// still held so channel order equals commit order.
#[derive(Default)]
pub(crate) struct Outbox {
    pub events: Vec<EventEnvelope>,
    pub access_changed: Vec<String>,
    /// (thread_id, username) whose drafts/presence must be dropped.
    pub removed: Vec<(String, String)>,
    /// (author user id, turn) newly available to that author's runners.
    pub turns: Vec<(i64, PendingTurn)>,
}

/// thread id -> username -> (draft text, updated_at).
type Drafts = HashMap<String, HashMap<String, (String, i64)>>;

#[derive(Default)]
struct Presence {
    connections: usize,
    state: Option<PresenceState>,
}

pub(crate) struct Hub {
    /// Serializes event-log writes with their publication.
    pub write_lock: Mutex<()>,
    threads: Mutex<HashMap<String, broadcast::Sender<Arc<Live>>>>,
    runners: Mutex<HashMap<i64, broadcast::Sender<PendingTurn>>>,
    drafts: Mutex<Drafts>,
    presence: Mutex<HashMap<String, HashMap<String, Presence>>>,
    credentials: broadcast::Sender<()>,
    sockets: Mutex<HashMap<i64, usize>>,
    mutations: Mutex<HashMap<i64, Vec<i64>>>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            write_lock: Mutex::new(()),
            threads: Mutex::default(),
            runners: Mutex::default(),
            drafts: Mutex::default(),
            presence: Mutex::default(),
            credentials: broadcast::channel(16).0,
            sockets: Mutex::default(),
            mutations: Mutex::default(),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Hub {
    pub fn subscribe(&self, thread_id: &str) -> broadcast::Receiver<Arc<Live>> {
        lock(&self.threads)
            .entry(thread_id.to_owned())
            .or_insert_with(|| broadcast::channel(THREAD_CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Drops the thread channel once its last subscriber is gone.
    pub fn release(&self, thread_id: &str) {
        let mut threads = lock(&self.threads);
        if threads
            .get(thread_id)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            threads.remove(thread_id);
        }
    }

    fn send_thread(&self, thread_id: &str, item: Live) {
        if let Some(sender) = lock(&self.threads).get(thread_id) {
            let _ = sender.send(Arc::new(item));
        }
    }

    pub fn subscribe_turns(&self, user_id: i64) -> broadcast::Receiver<PendingTurn> {
        lock(&self.runners)
            .entry(user_id)
            .or_insert_with(|| broadcast::channel(RUNNER_CHANNEL_CAPACITY).0)
            .subscribe()
    }

    pub fn release_turns(&self, user_id: i64) {
        let mut runners = lock(&self.runners);
        if runners
            .get(&user_id)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            runners.remove(&user_id);
        }
    }

    /// Publishes a committed outbox. Callers hold `write_lock`.
    pub fn publish(&self, outbox: Outbox) {
        for event in outbox.events {
            let thread_id = event.thread_id.clone();
            self.send_thread(&thread_id, Live::Event(event));
        }
        for (thread_id, username) in outbox.removed {
            if let Some(drafts) = lock(&self.drafts).get_mut(&thread_id) {
                drafts.remove(&username);
            }
            if let Some(presence) = lock(&self.presence).get_mut(&thread_id) {
                presence.remove(&username);
            }
        }
        for thread_id in outbox.access_changed {
            self.send_thread(&thread_id, Live::AccessChanged);
        }
        for (user_id, turn) in outbox.turns {
            if let Some(sender) = lock(&self.runners).get(&user_id) {
                let _ = sender.send(turn);
            }
        }
    }

    pub fn credentials(&self) -> broadcast::Receiver<()> {
        self.credentials.subscribe()
    }

    /// A session or runner credential was revoked: open sockets re-validate.
    pub fn credentials_changed(&self) {
        let _ = self.credentials.send(());
    }

    pub fn draft(&self, thread_id: &str, username: &str) -> Option<String> {
        lock(&self.drafts)
            .get(thread_id)
            .and_then(|drafts| drafts.get(username))
            .map(|(text, _)| text.clone())
    }

    /// Last write wins. An empty text clears the draft. The map update and
    /// the broadcast happen under one lock so subscribers see writes in the
    /// order they were applied.
    pub fn set_draft(&self, thread_id: &str, username: &str, text: String, now: i64) {
        let mut drafts = lock(&self.drafts);
        let thread = drafts.entry(thread_id.to_owned()).or_default();
        if text.is_empty() {
            thread.remove(username);
        } else {
            thread.insert(username.to_owned(), (text.clone(), now));
        }
        if thread.is_empty() {
            drafts.remove(thread_id);
        }
        self.send_thread(
            thread_id,
            Live::Frame(ServerFrame::Draft {
                username: username.to_owned(),
                text,
                updated_at: now,
            }),
        );
    }

    /// Clears the draft only if it still equals what was sent, so a newer
    /// draft typed after Send is kept.
    pub fn clear_sent_draft(&self, thread_id: &str, username: &str, sent: &str, now: i64) {
        if self.draft(thread_id, username).as_deref() == Some(sent) {
            self.set_draft(thread_id, username, String::new(), now);
        }
    }

    pub fn draft_snapshot(&self, thread_id: &str) -> Vec<ServerFrame> {
        lock(&self.drafts)
            .get(thread_id)
            .map(|drafts| {
                drafts
                    .iter()
                    .map(|(username, (text, updated_at))| ServerFrame::Draft {
                        username: username.clone(),
                        text: text.clone(),
                        updated_at: *updated_at,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn presence_snapshot(&self, thread_id: &str) -> Vec<ServerFrame> {
        lock(&self.presence)
            .get(thread_id)
            .map(|users| {
                users
                    .iter()
                    .map(|(username, presence)| ServerFrame::Presence {
                        username: username.clone(),
                        state: presence.state.unwrap_or(PresenceState::Online),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn presence_join(&self, thread_id: &str, username: &str) {
        let mut presence = lock(&self.presence);
        let entry = presence
            .entry(thread_id.to_owned())
            .or_default()
            .entry(username.to_owned())
            .or_default();
        entry.connections += 1;
        if entry.connections == 1 {
            entry.state = Some(PresenceState::Online);
            self.send_presence(thread_id, username, PresenceState::Online);
        }
    }

    pub fn presence_set(&self, thread_id: &str, username: &str, state: PresenceState) {
        let mut presence = lock(&self.presence);
        if let Some(entry) = presence
            .get_mut(thread_id)
            .and_then(|users| users.get_mut(username))
        {
            entry.state = Some(state);
            self.send_presence(thread_id, username, state);
        }
    }

    pub fn presence_leave(&self, thread_id: &str, username: &str) {
        let mut presence = lock(&self.presence);
        let Some(users) = presence.get_mut(thread_id) else {
            return;
        };
        let gone = match users.get_mut(username) {
            Some(entry) => {
                entry.connections = entry.connections.saturating_sub(1);
                entry.connections == 0
            }
            // Already dropped by a revocation.
            None => false,
        };
        if gone {
            users.remove(username);
            self.send_presence(thread_id, username, PresenceState::Offline);
        }
        if users.is_empty() {
            presence.remove(thread_id);
        }
    }

    fn send_presence(&self, thread_id: &str, username: &str, state: PresenceState) {
        self.send_thread(
            thread_id,
            Live::Frame(ServerFrame::Presence {
                username: username.to_owned(),
                state,
            }),
        );
    }

    /// Reserves one socket slot for `user_id`; the guard releases it.
    pub fn open_socket(self: &Arc<Self>, user_id: i64) -> Result<SocketSlot, AuthError> {
        let mut sockets = lock(&self.sockets);
        let count = sockets.entry(user_id).or_default();
        if *count >= MAX_SOCKETS_PER_USER {
            return Err(AuthError::RateLimited);
        }
        *count += 1;
        Ok(SocketSlot {
            hub: self.clone(),
            user_id,
        })
    }

    /// Sliding-window limit on HTTP mutations per user. In-memory and reset
    /// on restart, like the login throttle.
    pub fn allow_mutation(&self, user_id: i64, now: i64) -> Result<(), AuthError> {
        let mut mutations = lock(&self.mutations);
        let expired = |at: &i64| now.saturating_sub(*at) >= MUTATION_WINDOW_SECS;
        if mutations.len() >= MAX_LIMITER_KEYS && !mutations.contains_key(&user_id) {
            mutations.retain(|_, attempts| attempts.iter().any(|at| !expired(at)));
            if mutations.len() >= MAX_LIMITER_KEYS {
                return Err(AuthError::RateLimited);
            }
        }
        let attempts = mutations.entry(user_id).or_default();
        attempts.retain(|at| !expired(at));
        if attempts.len() >= MAX_MUTATIONS_PER_WINDOW {
            return Err(AuthError::RateLimited);
        }
        attempts.push(now);
        Ok(())
    }
}

pub(crate) struct SocketSlot {
    hub: Arc<Hub>,
    user_id: i64,
}

impl Drop for SocketSlot {
    fn drop(&mut self) {
        let mut sockets = lock(&self.hub.sockets);
        if let Some(count) = sockets.get_mut(&self.user_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                sockets.remove(&self.user_id);
            }
        }
    }
}
