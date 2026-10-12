//! What a shared thread's family publishes (ADR 0013): every local message
//! (with its turn's activity), child threads at any depth, local comments
//! and their resolution, review verdicts, subagent states and changes
//! snapshots. Each item becomes one durable outbox job, keyed so a rescan,
//! a retry or sharing again never publishes it twice.

use super::{CollabService, outbox, truncate};
use crate::error::Result;
use crate::session::CollabSignal;
use chrono::Utc;
use serde_json::json;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use tokio::sync::mpsc;
use trama_db::{FamilyRow, OutboxJob};
use trama_domain::ids::{CommentId, MessageId, ThreadId};
use trama_domain::message::{Actor, Message, Role};
use trama_domain::thread::{Thread, ThreadKind};
use trama_protocol::ChangeBase;

/// Signals are coalesced over this window before a family is rescanned.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(80);
/// Bounds of a published changes snapshot.
const MAX_SNAPSHOT_DIFF_BYTES: usize = 150 * 1024;
const MAX_SNAPSHOT_FILES: usize = 300;

/// Job kinds (`collab_outbox.job`).
pub(crate) mod job {
    pub const MESSAGE: &str = "message";
    pub const CHILD: &str = "child";
    pub const COMMENT: &str = "comment";
    pub const RESOLVE: &str = "resolve";
    pub const REOPEN: &str = "reopen";
    pub const DELETE_COMMENT: &str = "delete_comment";
    pub const CHANGES: &str = "changes";
    pub const STATUS: &str = "status";
    pub const VERDICT: &str = "verdict";
}

/// The local person's comments (others' are mirrored, never republished).
fn is_local_comment(author: &Actor) -> bool {
    matches!(author, Actor::Human { id } if id == "user")
}

/// The millisecond-time prefix of a ULID, for ordering children among
/// messages: a child is announced before anything later than its creation
/// (its spawn row), after anything earlier (the message a fork continues).
fn ulid_time(id: &str) -> &str {
    &id[..id.len().min(10)]
}

enum Item {
    Message(Message),
    Child(Thread),
}

impl CollabService {
    /// Receive session signals and rescan the families they touch.
    pub(super) async fn run_signals(self, mut rx: mpsc::UnboundedReceiver<CollabSignal>) {
        while let Some(first) = rx.recv().await {
            tokio::time::sleep(DEBOUNCE).await;
            let mut signals = vec![first];
            while let Ok(signal) = rx.try_recv() {
                signals.push(signal);
            }
            let mut roots = BTreeSet::new();
            let mut statuses = HashSet::new();
            for signal in signals {
                let thread = match signal {
                    CollabSignal::Changed(thread) => thread,
                    CollabSignal::SubagentStatus(thread) => {
                        statuses.insert(thread.clone());
                        thread
                    }
                };
                if let Some(root) = self.family_root(&thread).await {
                    roots.insert(root);
                }
            }
            for root in roots {
                if let Err(err) = self.sync_family(&root, &statuses, false).await {
                    crate::log::warn(format!("shared thread not synced: {err}"));
                }
            }
        }
    }

    /// The shared root of `thread_id`'s family: its top-level ancestor (or
    /// itself), when that thread is linked.
    pub(crate) async fn family_root(&self, thread_id: &ThreadId) -> Option<ThreadId> {
        let ancestors = self.db().thread_ancestors(thread_id).await.ok()?;
        let root = ancestors
            .last()
            .cloned()
            .unwrap_or_else(|| thread_id.clone());
        self.db()
            .collab_link(&root)
            .await
            .ok()
            .flatten()
            .map(|_| root)
    }

    /// The root and every descendant, parents before children, children in
    /// creation order.
    pub(crate) async fn family_threads(&self, root: &ThreadId) -> Result<Vec<Thread>> {
        let mut out = Vec::new();
        let Some(thread) = self.db().get_thread(root).await? else {
            return Ok(out);
        };
        let mut queue = VecDeque::from([thread]);
        let mut seen = HashSet::new();
        while let Some(thread) = queue.pop_front() {
            if !seen.insert(thread.id.clone()) {
                continue;
            }
            let mut children = self.db().list_subthreads(&thread.id).await?;
            children.sort_by(|a, b| a.id.0.cmp(&b.id.0));
            out.push(thread);
            queue.extend(children);
        }
        Ok(out)
    }

    fn new_job(
        &self,
        dedup_key: String,
        thread_id: &ThreadId,
        kind: &str,
        ref_id: Option<String>,
        payload: Option<serde_json::Value>,
    ) -> OutboxJob {
        OutboxJob {
            dedup_key,
            command_id: format!("{}{}", self.own_prefix(), MessageId::generate()),
            thread_id: thread_id.clone(),
            job: kind.to_owned(),
            ref_id,
            payload_json: payload.map(|p| p.to_string()),
        }
    }

    /// Enqueue whatever `root`'s family has not published yet. `initial`
    /// (sharing) also publishes a changes snapshot of every thread;
    /// `statuses` are subagents whose task state changed.
    pub(crate) async fn sync_family(
        &self,
        root: &ThreadId,
        statuses: &HashSet<ThreadId>,
        initial: bool,
    ) -> Result<usize> {
        let Some(link) = self.db().collab_link(root).await? else {
            return Ok(0);
        };
        let _serialized = self.inner.sync_lock.lock().await;
        let known: HashMap<ThreadId, FamilyRow> = self
            .db()
            .collab_family_of(root)
            .await?
            .into_iter()
            .map(|row| (row.thread_id.clone(), row))
            .collect();
        let threads = self.family_threads(root).await?;
        let mut jobs = Vec::new();
        let mut rows = Vec::new();
        let mut new_children: HashSet<ThreadId> = HashSet::new();
        for thread in &threads {
            if thread.id != *root && !known.contains_key(&thread.id) {
                new_children.insert(thread.id.clone());
            }
        }
        for thread in &threads {
            let children: Vec<Thread> = threads
                .iter()
                .filter(|child| {
                    child.parent_thread_id.as_ref() == Some(&thread.id)
                        && new_children.contains(&child.id)
                })
                .cloned()
                .collect();
            // A shared turn of this daemon holds the thread lock until its
            // prompt is mapped to the turn's command; skip its messages
            // until then (the turn's end signals again).
            let lock = self.thread_lock(&thread.id);
            let guard = lock.try_lock();
            let mut items: Vec<Item> = children.into_iter().map(Item::Child).collect();
            let mut last_answer: Option<MessageId> = None;
            if guard.is_ok() {
                let messages = self.db().list_messages_by_thread(&thread.id).await?;
                let origins = self.db().collab_message_origins(&thread.id).await?;
                for message in messages {
                    if origins.contains_key(&message.id) {
                        continue;
                    }
                    let key = format!("m:{}", message.id);
                    if self.db().collab_outbox_has(&key).await? {
                        continue;
                    }
                    if message.role == Role::Assistant {
                        last_answer = Some(message.id.clone());
                    }
                    items.push(Item::Message(message));
                }
            }
            items.sort_by(|a, b| {
                let key = |item: &Item| match item {
                    Item::Message(m) => (ulid_time(&m.id.0).to_owned(), 1),
                    Item::Child(t) => (ulid_time(&t.id.0).to_owned(), 0),
                };
                key(a).cmp(&key(b))
            });
            for item in items {
                match item {
                    Item::Message(message) => jobs.push(self.new_job(
                        format!("m:{}", message.id),
                        &thread.id,
                        job::MESSAGE,
                        Some(message.id.to_string()),
                        None,
                    )),
                    Item::Child(child) => {
                        jobs.push(self.new_job(
                            format!("t:{}", child.id),
                            &child.id,
                            job::CHILD,
                            Some(thread.id.to_string()),
                            None,
                        ));
                        rows.push(FamilyRow {
                            thread_id: child.id.clone(),
                            root_thread_id: root.clone(),
                            origin: "local".into(),
                        });
                    }
                }
            }
            // Comments of the local person, and their resolution.
            for record in self.db().list_comments(&thread.id).await? {
                let comment = &record.comment;
                if !is_local_comment(&comment.author) {
                    continue;
                }
                jobs.push(self.new_job(
                    format!("c:{}", comment.id),
                    &thread.id,
                    job::COMMENT,
                    Some(comment.id.to_string()),
                    None,
                ));
                if let (Some(resolved_at), None) = (comment.resolved_at, &comment.parent_id) {
                    jobs.push(self.resolution_job(&thread.id, &comment.id, Some(resolved_at)));
                }
            }
            if thread.kind == ThreadKind::Review {
                if let Some(record) = self.db().latest_review_verdict(&thread.id).await? {
                    if !record.id.starts_with("collab:") {
                        jobs.push(self.verdict_job(&thread.id, &record));
                    }
                }
            }
            let mirrored = known
                .get(&thread.id)
                .is_some_and(|row| row.origin == "remote");
            if statuses.contains(&thread.id) && !mirrored {
                if let Some(record) = self.db().get_subagent(&thread.id).await? {
                    jobs.push(self.new_job(
                        format!("s:{}:{}", thread.id, MessageId::generate()),
                        &thread.id,
                        job::STATUS,
                        None,
                        Some(json!({ "status": record.status, "detail": record.detail })),
                    ));
                }
            }
            let snapshot_key = if initial {
                Some(format!("x:s:{}:{}", link.collab_thread_id, thread.id))
            } else {
                last_answer.map(|answer| format!("x:{answer}"))
            };
            if let Some(key) = snapshot_key {
                if !self.db().collab_outbox_has(&key).await? {
                    if let Some(payload) = self.changes_snapshot(thread).await {
                        jobs.push(self.new_job(key, &thread.id, job::CHANGES, None, Some(payload)));
                    }
                }
            }
        }
        let added = self.db().enqueue_collab_outbox(jobs, rows).await?;
        if added > 0 {
            self.kick_outbox();
        }
        Ok(added)
    }

    pub(crate) fn resolution_job(
        &self,
        thread_id: &ThreadId,
        comment_id: &CommentId,
        resolved_at: Option<chrono::DateTime<Utc>>,
    ) -> OutboxJob {
        match resolved_at {
            Some(at) => self.new_job(
                format!("cr:{comment_id}:{}", at.timestamp_micros()),
                thread_id,
                job::RESOLVE,
                Some(comment_id.to_string()),
                None,
            ),
            None => self.new_job(
                format!("co:{comment_id}:{}", MessageId::generate()),
                thread_id,
                job::REOPEN,
                Some(comment_id.to_string()),
                None,
            ),
        }
    }

    fn verdict_job(
        &self,
        thread_id: &ThreadId,
        record: &trama_db::ReviewVerdictRecord,
    ) -> OutboxJob {
        let (verdict, body) = match &record.verdict {
            trama_domain::subthread::ReviewVerdict::Approve => ("approved", None),
            trama_domain::subthread::ReviewVerdict::RequestChanges { summary } => {
                ("changes_requested", Some(truncate(summary, 16 * 1024 - 8)))
            }
        };
        self.new_job(
            format!("v:{}", record.id),
            thread_id,
            job::VERDICT,
            None,
            Some(json!({ "verdict": verdict, "body": body })),
        )
    }

    /// This thread's diff against where it started, bounded. Scouts share
    /// their parent's checkouts and publish none.
    async fn changes_snapshot(&self, thread: &Thread) -> Option<serde_json::Value> {
        if self
            .db()
            .get_subagent(&thread.id)
            .await
            .ok()
            .flatten()
            .is_some_and(|record| {
                record.spawned_by.profile == trama_domain::subagent::SubagentProfile::Scout
            })
        {
            return None;
        }
        let mut repos = self
            .sessions()
            .get_changes(&thread.id, ChangeBase::ThreadBase)
            .await
            .ok()?;
        let budget = MAX_SNAPSHOT_DIFF_BYTES / repos.len().max(1);
        for repo in &mut repos {
            if repo.diff.len() > budget {
                repo.diff = truncate(&repo.diff, budget);
                repo.truncated = true;
            }
            if repo.files.len() > MAX_SNAPSHOT_FILES {
                repo.files.truncate(MAX_SNAPSHOT_FILES);
                repo.truncated = true;
            }
            repo.title = truncate(&repo.title, 120);
        }
        Some(json!({
            "captured_at": Utc::now(),
            "base": ChangeBase::ThreadBase,
            "repos": repos,
        }))
    }

    /// A local comment changed (`CommentCreated` is picked up by the scan):
    /// publish its resolution, reopening or deletion after its creation.
    pub(crate) fn comment_changed(&self, thread_id: ThreadId, change: CommentChange) {
        let service = self.clone();
        tokio::spawn(async move {
            let Some(root) = service.family_root(&thread_id).await else {
                return;
            };
            if let Err(err) = service.sync_family(&root, &HashSet::new(), false).await {
                crate::log::warn(format!("shared thread not synced: {err}"));
            }
            let job = match change {
                CommentChange::Resolved(id, at) => {
                    service.resolution_job(&thread_id, &id, Some(at))
                }
                CommentChange::Reopened(id) => service.resolution_job(&thread_id, &id, None),
                CommentChange::Deleted(id) => service.new_job(
                    format!("cd:{id}"),
                    &thread_id,
                    job::DELETE_COMMENT,
                    Some(id.to_string()),
                    None,
                ),
            };
            let _serialized = service.inner.sync_lock.lock().await;
            match service.db().enqueue_collab_outbox(vec![job], vec![]).await {
                Ok(_) => service.kick_outbox(),
                Err(err) => crate::log::warn(format!("comment change not queued: {err}")),
            }
        });
    }

    /// Wait (bounded) until `thread_id` has nothing left to publish, so a
    /// new turn lands after the history it follows.
    pub(crate) async fn flush_thread(&self, thread_id: &ThreadId) {
        if let Some(root) = self.family_root(thread_id).await {
            let _ = self.sync_family(&root, &HashSet::new(), false).await;
        }
        let deadline = tokio::time::Instant::now() + outbox::FLUSH_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            match self
                .db()
                .collab_outbox_pending_for(std::slice::from_ref(thread_id))
                .await
            {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    self.kick_outbox();
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                }
            }
        }
    }
}

/// A change to a local comment after its creation.
pub(crate) enum CommentChange {
    Resolved(CommentId, chrono::DateTime<Utc>),
    Reopened(CommentId),
    Deleted(CommentId),
}
