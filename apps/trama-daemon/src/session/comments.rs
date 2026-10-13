//! Anchored comments: capture, relocation, delivery with the next prompt and
//! the link back to the turn that answered them.

use super::*;
use std::path::{Component, Path};
use trama_db::CommentRecord;
use trama_domain::comment::{
    AnchorStatus, CodeAnchor, Comment, CommentTarget, capture_anchor, excerpt_lines,
    relocate_anchor,
};
use trama_domain::ids::{CommentId, RepositoryId};
use trama_infra::changes;
use trama_protocol::{CommentSnapshot, CommentState};

/// The local person. Matches the actor of user prompts.
pub(super) const LOCAL_USER: &str = "user";
const MAX_BODY_CHARS: usize = 20_000;
/// Captured excerpts are stored up to this size.
const MAX_STORED_EXCERPT: usize = 64 * 1024;
/// Quotes in the delivered block are cut at this size.
const MAX_QUOTED_EXCERPT: usize = 4_000;

/// Where a comment's anchor is now. `current` holds the anchored lines at
/// the relocated position (code comments that resolved).
#[derive(Default)]
struct Located {
    status: Option<AnchorStatus>,
    current: Option<String>,
}

/// Pending comments ready to go out with a prompt.
pub(super) struct PendingDelivery {
    pub(super) ids: Vec<CommentId>,
    pub(super) block: String,
}

/// A comment mirrored from another participant (`collab:<username>`).
pub(crate) fn is_remote_author(actor: &Actor) -> bool {
    matches!(actor, Actor::Human { id } if id.starts_with("collab:"))
}

fn local_user() -> Actor {
    Actor::Human {
        id: LOCAL_USER.into(),
    }
}

fn snapshot(record: CommentRecord, located: Located) -> CommentSnapshot {
    let CommentRecord { comment, delivery } = record;
    let state = match (comment.resolved_at, delivery) {
        (Some(resolved_at), delivery) => CommentState::Resolved {
            resolved_at,
            delivery,
        },
        (None, Some(delivery)) => CommentState::Delivered(delivery),
        (None, None) => CommentState::Pending,
    };
    CommentSnapshot {
        id: comment.id,
        thread_id: comment.thread_id,
        parent_comment_id: comment.parent_id,
        author: comment.author,
        target: comment.target,
        body: comment.body,
        excerpt: comment.excerpt,
        created_at: comment.created_at,
        state,
        anchor_status: located.status,
    }
}

impl SessionManager {
    pub async fn create_comment(
        &self,
        thread_id: ThreadId,
        target: CommentTarget,
        body: String,
        parent_comment_id: Option<CommentId>,
    ) -> Result<CommentSnapshot> {
        let thread = self
            .db
            .get_thread(&thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let body = body.trim().to_owned();
        if body.is_empty() {
            return Err(DaemonError::Invalid(
                "comment text must not be empty".into(),
            ));
        }
        if body.chars().count() > MAX_BODY_CHARS {
            return Err(DaemonError::Invalid(format!(
                "comment text must be at most {MAX_BODY_CHARS} characters"
            )));
        }
        let (parent_id, target, excerpt) = match parent_comment_id {
            Some(parent_id) => {
                let parent = self
                    .db
                    .get_comment(&parent_id)
                    .await?
                    .ok_or_else(|| DaemonError::NotFound(format!("comment {parent_id}")))?
                    .comment;
                if parent.thread_id != thread_id {
                    return Err(DaemonError::Invalid(
                        "a reply must be in the same thread as its comment".into(),
                    ));
                }
                // Replies attach to the root so a conversation stays flat.
                let root = match parent.parent_id {
                    Some(root_id) => {
                        self.db
                            .get_comment(&root_id)
                            .await?
                            .ok_or_else(|| DaemonError::NotFound(format!("comment {root_id}")))?
                            .comment
                    }
                    None => parent,
                };
                if root.resolved_at.is_some() {
                    return Err(DaemonError::Invalid(
                        "this comment is resolved. Reopen it before replying".into(),
                    ));
                }
                (Some(root.id), root.target, root.excerpt)
            }
            None => {
                let (target, excerpt) = self.capture_target(&thread, target).await?;
                (None, target, excerpt)
            }
        };
        let comment = Comment {
            id: CommentId::generate(),
            thread_id: thread_id.clone(),
            author: local_user(),
            target,
            body,
            created_at: Utc::now(),
            resolved_at: None,
            parent_id,
            excerpt,
        };
        let event = EventRecord::new(
            Aggregate::Thread {
                id: thread_id.clone(),
            },
            local_user(),
            DomainEvent::CommentCreated {
                comment: comment.clone(),
            },
            vec![],
        )?;
        self.db.create_comment(&comment, &event).await?;
        let snapshot = self
            .comment_snapshots(
                &thread_id,
                vec![CommentRecord {
                    comment,
                    delivery: None,
                }],
            )
            .await?
            .remove(0);
        self.broadcast(&thread_id, DaemonMessage::CommentCreated(snapshot.clone()));
        Ok(snapshot)
    }

    /// Every comment of the thread, oldest first, with code anchors
    /// relocated against the thread's current checkout.
    pub async fn list_comments(&self, thread_id: &ThreadId) -> Result<Vec<CommentSnapshot>> {
        if self.db.get_thread(thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        let records = self.db.list_comments(thread_id).await?;
        self.comment_snapshots(thread_id, records).await
    }

    pub async fn resolve_comment(&self, comment_id: &CommentId) -> Result<CommentSnapshot> {
        self.set_resolved(comment_id, true).await
    }

    pub async fn reopen_comment(&self, comment_id: &CommentId) -> Result<CommentSnapshot> {
        self.set_resolved(comment_id, false).await
    }

    async fn set_resolved(&self, comment_id: &CommentId, resolve: bool) -> Result<CommentSnapshot> {
        let record = self
            .db
            .get_comment(comment_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("comment {comment_id}")))?;
        if record.comment.parent_id.is_some() {
            return Err(DaemonError::Invalid(
                "resolve or reopen the comment this reply belongs to".into(),
            ));
        }
        let thread_id = record.comment.thread_id.clone();
        if record.comment.resolved_at.is_some() != resolve {
            let resolved_at = resolve.then(Utc::now);
            let event = match resolved_at {
                Some(resolved_at) => DomainEvent::CommentResolved {
                    thread_id: thread_id.clone(),
                    comment_id: comment_id.clone(),
                    resolved_at,
                },
                None => DomainEvent::CommentReopened {
                    thread_id: thread_id.clone(),
                    comment_id: comment_id.clone(),
                },
            };
            let event = EventRecord::new(
                Aggregate::Thread {
                    id: thread_id.clone(),
                },
                local_user(),
                event,
                vec![],
            )?;
            self.db
                .set_comment_resolved(comment_id, resolved_at, &event)
                .await?;
        }
        let record = self
            .db
            .get_comment(comment_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("comment {comment_id}")))?;
        let snapshot = self
            .comment_snapshots(&thread_id, vec![record])
            .await?
            .remove(0);
        self.broadcast(&thread_id, DaemonMessage::CommentUpdated(snapshot.clone()));
        Ok(snapshot)
    }

    /// Delete one of the local person's pending comments, with its replies.
    /// Returns the thread and the removed ids.
    pub async fn delete_comment(
        &self,
        comment_id: &CommentId,
    ) -> Result<(ThreadId, Vec<CommentId>)> {
        let record = self
            .db
            .get_comment(comment_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("comment {comment_id}")))?;
        if record.comment.author != local_user() {
            return Err(DaemonError::Invalid(
                "only your own comments can be deleted".into(),
            ));
        }
        if record.delivery.is_some() {
            return Err(DaemonError::Invalid(
                "this comment was already sent to the agent. Resolve it instead".into(),
            ));
        }
        let thread_id = record.comment.thread_id.clone();
        let replies: Vec<CommentRecord> = self
            .db
            .list_comments(&thread_id)
            .await?
            .into_iter()
            .filter(|reply| reply.comment.parent_id.as_ref() == Some(comment_id))
            .collect();
        if replies.iter().any(|reply| reply.delivery.is_some()) {
            return Err(DaemonError::Invalid(
                "replies to this comment were already sent to the agent. Resolve it instead".into(),
            ));
        }
        let mut ids: Vec<CommentId> = replies.into_iter().map(|reply| reply.comment.id).collect();
        ids.push(comment_id.clone());
        let event = EventRecord::new(
            Aggregate::Thread {
                id: thread_id.clone(),
            },
            local_user(),
            DomainEvent::CommentsDeleted {
                thread_id: thread_id.clone(),
                comment_ids: ids.clone(),
            },
            vec![],
        )?;
        self.db.delete_comments(&ids, &event).await?;
        self.broadcast(
            &thread_id,
            DaemonMessage::CommentDeleted {
                thread_id: thread_id.clone(),
                comment_ids: ids.clone(),
            },
        );
        Ok((thread_id, ids))
    }

    /// Tell subscribers about comments the collab mirror wrote: created
    /// (`created`) or changed.
    pub(crate) async fn notify_comments(
        &self,
        thread_id: &ThreadId,
        ids: &[CommentId],
        created: bool,
    ) {
        if !created {
            self.broadcast_comment_updates(thread_id, ids).await;
            return;
        }
        let records = match self.db.list_comments(thread_id).await {
            Ok(records) => records
                .into_iter()
                .filter(|record| ids.contains(&record.comment.id))
                .collect(),
            Err(err) => {
                crate::log::warn(format!("failed to load comments: {err}"));
                return;
            }
        };
        match self.comment_snapshots(thread_id, records).await {
            Ok(snapshots) => {
                for snapshot in snapshots {
                    self.broadcast(thread_id, DaemonMessage::CommentCreated(snapshot));
                }
            }
            Err(err) => crate::log::warn(format!("failed to snapshot comments: {err}")),
        }
    }

    /// Thread a comment belongs to (for routing replies).
    pub async fn comment_thread(&self, comment_id: &CommentId) -> Result<ThreadId> {
        self.db
            .comment_thread(comment_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("comment {comment_id}")))
    }

    /// Pending, unresolved comments of the thread formatted as one block, or
    /// `None` when nothing is pending. A reply to a delivered comment goes out
    /// with the comment it answers (marked as already discussed).
    pub(super) async fn pending_delivery(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<PendingDelivery>> {
        let records = self.db.list_comments(thread_id).await?;
        let resolved_roots: HashSet<CommentId> = records
            .iter()
            .filter(|record| {
                record.comment.parent_id.is_none() && record.comment.resolved_at.is_some()
            })
            .map(|record| record.comment.id.clone())
            .collect();
        // Comments mirrored from other participants of a shared thread went
        // (or go) to their own agent; this one learns of them through the
        // shared-thread digest instead.
        let pending: HashSet<CommentId> = records
            .iter()
            .filter(|record| record.delivery.is_none())
            .filter(|record| !is_remote_author(&record.comment.author))
            .filter(|record| {
                let root = record
                    .comment
                    .parent_id
                    .as_ref()
                    .unwrap_or(&record.comment.id);
                !resolved_roots.contains(root)
            })
            .map(|record| record.comment.id.clone())
            .collect();
        if pending.is_empty() {
            return Ok(None);
        }
        // Roots with something pending, in creation order, and their replies.
        let roots: Vec<&CommentRecord> = records
            .iter()
            .filter(|record| record.comment.parent_id.is_none())
            .filter(|root| {
                pending.contains(&root.comment.id)
                    || records.iter().any(|reply| {
                        reply.comment.parent_id.as_ref() == Some(&root.comment.id)
                            && pending.contains(&reply.comment.id)
                    })
            })
            .collect();
        let root_records: Vec<CommentRecord> = roots.iter().map(|root| (*root).clone()).collect();
        let (located, titles) = self.locate(thread_id, &root_records).await?;
        let messages = self.transcript(thread_id).await?;
        let mut entries = Vec::with_capacity(roots.len());
        let mut ids = Vec::new();
        for (root, located) in roots.into_iter().zip(located) {
            let mut lines = Vec::new();
            if pending.contains(&root.comment.id) {
                ids.push(root.comment.id.clone());
                lines.push(format!("Comment: {}", root.comment.body));
            } else {
                lines.push(format!("Earlier comment: {}", root.comment.body));
            }
            for reply in records
                .iter()
                .filter(|reply| reply.comment.parent_id.as_ref() == Some(&root.comment.id))
            {
                if pending.contains(&reply.comment.id) {
                    ids.push(reply.comment.id.clone());
                    lines.push(format!("Reply: {}", reply.comment.body));
                } else {
                    lines.push(format!("Earlier reply: {}", reply.comment.body));
                }
            }
            entries.push(DeliveryEntry {
                passage: passage(&root.comment, &located, &titles, &messages),
                lines,
            });
        }
        Ok(Some(PendingDelivery {
            ids,
            block: format_comment_block(&entries),
        }))
    }

    /// Snapshots of the given comments (sent as `CommentUpdated`).
    pub(super) async fn broadcast_comment_updates(&self, thread_id: &ThreadId, ids: &[CommentId]) {
        if ids.is_empty() {
            return;
        }
        let records = match self.db.list_comments(thread_id).await {
            Ok(records) => records
                .into_iter()
                .filter(|record| ids.contains(&record.comment.id))
                .collect(),
            Err(err) => {
                crate::log::warn(format!("failed to load comments: {err}"));
                return;
            }
        };
        match self.comment_snapshots(thread_id, records).await {
            Ok(snapshots) => {
                for snapshot in snapshots {
                    self.broadcast(thread_id, DaemonMessage::CommentUpdated(snapshot));
                }
            }
            Err(err) => crate::log::warn(format!("failed to snapshot comments: {err}")),
        }
    }

    /// The turn that carried comments ended with `answer`: link them to it.
    pub(super) async fn link_comment_answers(&self, thread_id: &ThreadId, answer: &MessageId) {
        let Some(delivered) = self.take_turn_delivery(thread_id) else {
            return;
        };
        let result = async {
            let ids = self.db.comments_delivered_with(&delivered).await?;
            if ids.is_empty() {
                return Ok::<_, DaemonError>(ids);
            }
            let event = EventRecord::new(
                Aggregate::Thread {
                    id: thread_id.clone(),
                },
                Actor::System,
                DomainEvent::CommentsAnswered {
                    thread_id: thread_id.clone(),
                    delivered_message_id: delivered.clone(),
                    answer_message_id: answer.clone(),
                },
                vec![],
            )?;
            self.db
                .record_comment_answer(&delivered, answer, &event)
                .await?;
            Ok(ids)
        }
        .await;
        match result {
            Ok(ids) => self.broadcast_comment_updates(thread_id, &ids).await,
            Err(err) => crate::log::warn(format!("failed to link comment answers: {err}")),
        }
    }

    /// The prompt that carried comments never reached the agent: make them
    /// pending again so the next send delivers them.
    pub(super) async fn return_undelivered_comments(&self, thread_id: &ThreadId) {
        let Some(delivered) = self.take_turn_delivery(thread_id) else {
            return;
        };
        let result = async {
            let ids = self.db.comments_delivered_with(&delivered).await?;
            if ids.is_empty() {
                return Ok::<_, DaemonError>(ids);
            }
            let event = EventRecord::new(
                Aggregate::Thread {
                    id: thread_id.clone(),
                },
                Actor::System,
                DomainEvent::CommentDeliveryFailed {
                    thread_id: thread_id.clone(),
                    comment_ids: ids.clone(),
                    message_id: delivered.clone(),
                },
                vec![],
            )?;
            self.db
                .return_comments_to_pending(&delivered, &event)
                .await?;
            Ok(ids)
        }
        .await;
        match result {
            Ok(ids) => self.broadcast_comment_updates(thread_id, &ids).await,
            Err(err) => crate::log::warn(format!("failed to return comments to pending: {err}")),
        }
    }

    pub(super) fn take_turn_delivery(&self, thread_id: &ThreadId) -> Option<MessageId> {
        let mut inner = self.lock();
        inner
            .threads
            .get_mut(thread_id)
            .and_then(|thread| thread.comments_delivered_with.take())
    }

    async fn comment_snapshots(
        &self,
        thread_id: &ThreadId,
        records: Vec<CommentRecord>,
    ) -> Result<Vec<CommentSnapshot>> {
        let (located, _) = self.locate(thread_id, &records).await?;
        Ok(records
            .into_iter()
            .zip(located)
            .map(|(record, located)| snapshot(record, located))
            .collect())
    }

    /// Relocate code anchors against the thread's current checkout. A file
    /// that cannot be read (missing checkout, deleted file) is orphaned.
    /// Also returns repository titles for formatting.
    async fn locate(
        &self,
        thread_id: &ThreadId,
        records: &[CommentRecord],
    ) -> Result<(Vec<Located>, HashMap<RepositoryId, String>)> {
        let anchors: Vec<Option<CodeAnchor>> = records
            .iter()
            .map(|record| match &record.comment.target {
                CommentTarget::Code { anchor } => Some(anchor.clone()),
                _ => None,
            })
            .collect();
        let mut titles = HashMap::new();
        let mut roots = HashMap::new();
        let needs_checkouts = records
            .iter()
            .any(|record| !matches!(record.comment.target, CommentTarget::MessageRange { .. }));
        if needs_checkouts {
            if let Some(thread) = self.db.get_thread(thread_id).await? {
                match self.resolve_checkouts(&thread).await {
                    Ok(checkouts) => {
                        for checkout in checkouts {
                            titles.insert(checkout.repository_id.clone(), checkout.title);
                            roots.insert(checkout.repository_id, checkout.path);
                        }
                    }
                    Err(err) => crate::log::warn(format!(
                        "comments: checkouts of thread {thread_id} unavailable: {err}"
                    )),
                }
            }
        }
        let located = tokio::task::spawn_blocking(move || {
            anchors
                .into_iter()
                .map(|anchor| match anchor {
                    None => Located::default(),
                    Some(anchor) => {
                        let content = roots
                            .get(&anchor.repository_id)
                            .and_then(|root| checkout_file(root, &anchor.path).ok())
                            .and_then(|path| std::fs::read_to_string(path).ok());
                        let Some(content) = content else {
                            return Located {
                                status: Some(AnchorStatus::Orphaned),
                                current: None,
                            };
                        };
                        let status = relocate_anchor(&anchor, &content);
                        let current = match status {
                            AnchorStatus::Resolved { line } => excerpt_lines(
                                &content,
                                line,
                                anchor.end_line - anchor.start_line + 1,
                            ),
                            _ => None,
                        };
                        Located {
                            status: Some(status),
                            current,
                        }
                    }
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("anchor relocation task failed: {error}")))?;
        Ok((located, titles))
    }

    /// The messages a thread shows (inherited, then its own).
    async fn transcript(&self, thread_id: &ThreadId) -> Result<Vec<Message>> {
        let mut messages = self.inherited_messages(thread_id).await?;
        messages.extend(self.db.list_messages_by_thread(thread_id).await?);
        Ok(messages)
    }

    /// Validate a new root comment's target and capture its excerpt.
    async fn capture_target(
        &self,
        thread: &Thread,
        target: CommentTarget,
    ) -> Result<(CommentTarget, Option<String>)> {
        match target {
            CommentTarget::MessageRange {
                message_id,
                start_byte,
                end_byte,
            } => {
                let message = self
                    .transcript(&thread.id)
                    .await?
                    .into_iter()
                    .find(|message| message.id == message_id)
                    .ok_or_else(|| {
                        DaemonError::NotFound(format!(
                            "message {message_id} in thread {}",
                            thread.id
                        ))
                    })?;
                let text = message.text();
                if start_byte >= end_byte
                    || end_byte > text.len()
                    || !text.is_char_boundary(start_byte)
                    || !text.is_char_boundary(end_byte)
                {
                    return Err(DaemonError::Invalid(format!(
                        "range {start_byte}..{end_byte} is not a non-empty character range of the message text ({} bytes)",
                        text.len()
                    )));
                }
                let excerpt = text[start_byte..end_byte].to_owned();
                Ok((
                    CommentTarget::MessageRange {
                        message_id,
                        start_byte,
                        end_byte,
                    },
                    Some(cap(excerpt, MAX_STORED_EXCERPT)),
                ))
            }
            CommentTarget::Code { anchor } => {
                let root = self.checkout_root(thread, &anchor.repository_id).await?;
                let CodeAnchor {
                    repository_id,
                    path,
                    revision,
                    start_line,
                    end_line,
                    ..
                } = anchor;
                tokio::task::spawn_blocking(move || {
                    let file = checkout_file(&root, &path)?;
                    let content = std::fs::read_to_string(&file).map_err(|error| {
                        DaemonError::Invalid(format!(
                            "cannot read {} in the thread's checkout: {error}",
                            path.display()
                        ))
                    })?;
                    let (anchor, text) = capture_anchor(
                        repository_id,
                        path.clone(),
                        revision,
                        &content,
                        start_line,
                        end_line,
                    )
                    .ok_or_else(|| {
                        DaemonError::Invalid(format!(
                            "lines {start_line}-{end_line} are not in {} ({} lines)",
                            path.display(),
                            content.lines().count()
                        ))
                    })?;
                    Ok((
                        CommentTarget::Code { anchor },
                        Some(cap(text, MAX_STORED_EXCERPT)),
                    ))
                })
                .await
                .map_err(|error| DaemonError::Invalid(format!("anchor task failed: {error}")))?
            }
            CommentTarget::DiffHunk {
                repository_id,
                path,
                base_revision,
                hunk_header,
            } => {
                let root = self.checkout_root(thread, &repository_id).await?;
                checkout_relative(&path)?;
                let hunk_header = hunk_header.trim_end().to_owned();
                if !hunk_header.starts_with("@@") {
                    return Err(DaemonError::Invalid(
                        "hunk_header must be a unified diff hunk header (@@ ... @@)".into(),
                    ));
                }
                let (path, base_revision, hunk_header, excerpt) =
                    tokio::task::spawn_blocking(move || {
                        let diff =
                            changes::diff(&root, &base_revision).map_err(checkout_to_invalid)?;
                        let hunk = find_hunk(&diff.diff, &path, &hunk_header).ok_or_else(|| {
                            DaemonError::Invalid(format!(
                                "hunk {hunk_header} of {} is not in the diff against {base_revision}",
                                path.display()
                            ))
                        })?;
                        Ok::<_, DaemonError>((path, base_revision, hunk_header, hunk))
                    })
                    .await
                    .map_err(|error| DaemonError::Invalid(format!("diff task failed: {error}")))??;
                Ok((
                    CommentTarget::DiffHunk {
                        repository_id,
                        path,
                        base_revision,
                        hunk_header,
                    },
                    Some(cap(excerpt, MAX_STORED_EXCERPT)),
                ))
            }
        }
    }

    async fn checkout_root(
        &self,
        thread: &Thread,
        repository_id: &RepositoryId,
    ) -> Result<PathBuf> {
        self.resolve_checkouts(thread)
            .await?
            .into_iter()
            .find(|checkout| &checkout.repository_id == repository_id)
            .map(|checkout| checkout.path)
            .ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "repository {repository_id} is not attached to thread {}",
                    thread.id
                ))
            })
    }
}

/// `rel` must stay inside the checkout: relative, no `..`.
fn checkout_relative(rel: &Path) -> Result<()> {
    let normal = rel
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
    if rel.as_os_str().is_empty() || !normal {
        return Err(DaemonError::Invalid(format!(
            "path {} must be relative to the repository root",
            rel.display()
        )));
    }
    Ok(())
}

/// The file `rel` inside `root`, refusing paths (or symlinks) that leave it.
fn checkout_file(root: &Path, rel: &Path) -> Result<PathBuf> {
    checkout_relative(rel)?;
    let root = dunce::canonicalize(root)?;
    let file = dunce::canonicalize(root.join(rel)).map_err(|error| {
        DaemonError::Invalid(format!(
            "{} is not in the thread's checkout: {error}",
            rel.display()
        ))
    })?;
    if !file.starts_with(&root) {
        return Err(DaemonError::Invalid(format!(
            "{} points outside the thread's checkout",
            rel.display()
        )));
    }
    Ok(file)
}

/// The hunk of `path` starting with `header` in a `git diff` text.
fn find_hunk(diff: &str, path: &Path, header: &str) -> Option<String> {
    let path = path.to_string_lossy();
    let new_side = format!("+++ b/{path}");
    let old_side = format!("--- a/{path}");
    let mut in_file = false;
    let mut hunk: Option<Vec<&str>> = None;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            if hunk.is_some() {
                break;
            }
            in_file = false;
            continue;
        }
        if line == new_side || line == old_side {
            in_file = true;
            continue;
        }
        if let Some(lines) = hunk.as_mut() {
            if line.starts_with("@@") {
                break;
            }
            lines.push(line);
        } else if in_file && line.starts_with(header) {
            hunk = Some(vec![line]);
        }
    }
    hunk.map(|lines| lines.join("\n"))
}

fn cap(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" [...]");
    }
    text
}

/// What a delivered comment is about, quoted for the agent.
struct Passage {
    heading: String,
    quote: Quote,
}

enum Quote {
    /// Transcript text, quoted with `> `.
    Prose(String),
    /// Code or a diff, fenced with an optional info string.
    Code {
        info: &'static str,
        text: String,
    },
    None,
}

struct DeliveryEntry {
    passage: Passage,
    lines: Vec<String>,
}

fn passage(
    comment: &Comment,
    located: &Located,
    titles: &HashMap<RepositoryId, String>,
    messages: &[Message],
) -> Passage {
    let repo_path = |repository_id: &RepositoryId, path: &Path| {
        let title = titles
            .get(repository_id)
            .cloned()
            .unwrap_or_else(|| repository_id.to_string());
        format!("{title}/{}", path.display())
    };
    let excerpt = comment.excerpt.clone().unwrap_or_default();
    match &comment.target {
        CommentTarget::MessageRange { message_id, .. } => {
            let whose = match messages
                .iter()
                .find(|message| &message.id == message_id)
                .map(|message| message.role)
            {
                Some(Role::Assistant) => "your message",
                Some(Role::User) => "the user's message",
                _ => "a transcript message",
            };
            Passage {
                heading: format!("On this passage of {whose}:"),
                quote: Quote::Prose(excerpt),
            }
        }
        CommentTarget::Code { anchor } => {
            let place = repo_path(&anchor.repository_id, &anchor.path);
            let len = anchor.end_line - anchor.start_line;
            let range = |start: u32| {
                if len == 0 {
                    format!("line {start}")
                } else {
                    format!("lines {start}-{}", start + len)
                }
            };
            match (located.status, &located.current) {
                (Some(AnchorStatus::Resolved { line }), Some(current)) => Passage {
                    heading: if line == anchor.start_line {
                        format!("On {place} {}:", range(line))
                    } else {
                        format!(
                            "On {place} {} (written at {}; the code has moved):",
                            range(line),
                            range(anchor.start_line)
                        )
                    },
                    quote: Quote::Code {
                        info: "",
                        text: current.clone(),
                    },
                },
                (Some(AnchorStatus::Fuzzy), _) => Passage {
                    heading: format!(
                        "On {place} {} as it was when commented (that code has since changed):",
                        range(anchor.start_line)
                    ),
                    quote: Quote::Code {
                        info: "",
                        text: excerpt,
                    },
                },
                _ => Passage {
                    heading: format!(
                        "On {place} {} as it was when commented (that code is no longer in the file):",
                        range(anchor.start_line)
                    ),
                    quote: Quote::Code {
                        info: "",
                        text: excerpt,
                    },
                },
            }
        }
        CommentTarget::DiffHunk {
            repository_id,
            path,
            hunk_header,
            ..
        } => Passage {
            heading: format!(
                "On the diff of {} at {hunk_header}:",
                repo_path(repository_id, path)
            ),
            quote: if excerpt.is_empty() {
                Quote::None
            } else {
                Quote::Code {
                    info: "diff",
                    text: excerpt,
                }
            },
        },
    }
}

const BLOCK_OPEN: &str = "<review-comments>";
const BLOCK_CLOSE: &str = "</review-comments>";
const BLOCK_INTRO: &str = "The user left comments on specific passages. Each quotes the passage it is about; address every comment.";

/// The block prepended to the prompt. Stable format; the UI may collapse it
/// by its first and last lines.
fn format_comment_block(entries: &[DeliveryEntry]) -> String {
    let mut out = String::new();
    out.push_str(BLOCK_OPEN);
    out.push('\n');
    out.push_str(BLOCK_INTRO);
    out.push('\n');
    for (index, entry) in entries.iter().enumerate() {
        out.push('\n');
        out.push_str(&format!("[{}] {}\n", index + 1, entry.passage.heading));
        match &entry.passage.quote {
            Quote::Prose(text) => {
                for line in cap(text.clone(), MAX_QUOTED_EXCERPT).lines() {
                    out.push_str("> ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Quote::Code { info, text } => {
                let text = cap(text.clone(), MAX_QUOTED_EXCERPT);
                let fence = fence_for(&text);
                out.push_str(&format!("{fence}{info}\n{text}\n{fence}\n"));
            }
            Quote::None => {}
        }
        for line in &entry.lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(BLOCK_CLOSE);
    out
}

/// A backtick fence longer than any backtick run inside `text`.
fn fence_for(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

/// The prompt sent to the agent (and stored as the user message).
pub(super) fn compose_prompt(block: Option<&str>, text: &str) -> String {
    match block {
        Some(block) if text.trim().is_empty() => block.to_owned(),
        Some(block) => format!("{block}\n\n{text}"),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeAdapter, scripted_events};
    use tokio::time::{Duration, timeout};
    use trama_domain::workspace::{Repository, WorkspaceMembership};
    use trama_protocol::{ChangeBase, SubthreadKind};

    const LIB: &str = "// header\nfn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
    const NOTES: &str = "intro\nalpha\nbravo\nTARGET ONE\nTARGET TWO\ncharlie\ndelta\n";

    fn git(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Trama Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Trama Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "git {args:?}");
    }

    struct Fixture {
        dir: tempfile::TempDir,
        db_path: PathBuf,
        workspace: WorkspaceId,
    }

    /// File-backed DB, one workspace, one committed repo aliased "app".
    async fn fixture() -> (Fixture, TramaDb) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("trama.db");
        let db = TramaDb::open(&db_path).unwrap();
        let workspace = Workspace::new("comments");
        db.create_workspace(&workspace).await.unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir_all(source.join("src")).unwrap();
        git(&source, &["init", "-b", "main"]);
        std::fs::write(source.join("src/lib.rs"), LIB).unwrap();
        std::fs::write(source.join("notes.txt"), NOTES).unwrap();
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
                db_path,
                workspace: workspace.id,
                dir,
            },
            db,
        )
    }

    fn manager(fixture: &Fixture, db: TramaDb, fake: &FakeAdapter) -> SessionManager {
        SessionManager::with_checkout_root(
            db,
            Arc::new(fake.clone()),
            fixture.dir.path().join("managed"),
        )
    }

    fn fast_fake() -> FakeAdapter {
        FakeAdapter::new(scripted_events(), Duration::from_millis(1))
    }

    fn prompts(fake: &FakeAdapter) -> Vec<String> {
        fake.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn checkout(mgr: &SessionManager, thread_id: &ThreadId) -> (RepositoryId, PathBuf) {
        let thread = mgr.db.get_thread(thread_id).await.unwrap().unwrap();
        let checkout = mgr.resolve_checkouts(&thread).await.unwrap().remove(0);
        (checkout.repository_id, checkout.path)
    }

    /// Wait for the end of a turn (`UsageSnapshot`), collecting messages.
    async fn wait_turn(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> Vec<DaemonMessage> {
        let mut seen = Vec::new();
        loop {
            let msg = timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("turn event")
                .expect("channel open");
            match &msg {
                DaemonMessage::UsageSnapshot { .. } => return seen,
                DaemonMessage::AgentEventEnvelope {
                    event: AgentEvent::Failed { message },
                    ..
                } => panic!("turn failed: {message}"),
                _ => seen.push(msg),
            }
        }
    }

    async fn subscribed(
        mgr: &SessionManager,
        thread_id: &ThreadId,
    ) -> mpsc::UnboundedReceiver<DaemonMessage> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(thread_id, tx).await.unwrap();
        while rx.try_recv().is_ok() {}
        rx
    }

    fn code(repository_id: &RepositoryId, path: &str, start: u32, end: u32) -> CommentTarget {
        CommentTarget::Code {
            anchor: CodeAnchor {
                repository_id: repository_id.clone(),
                path: path.into(),
                revision: 0,
                start_line: start,
                end_line: end,
                // Ignored: the daemon hashes the checkout itself.
                text_hash: Some("bogus".into()),
                context_hash: None,
            },
        }
    }

    fn by_id<'a>(comments: &'a [CommentSnapshot], id: &CommentId) -> &'a CommentSnapshot {
        comments.iter().find(|c| &c.id == id).expect("comment")
    }

    const INTRO: &str = "<review-comments>\nThe user left comments on specific passages. Each quotes the passage it is about; address every comment.\n";

    #[tokio::test]
    async fn comments_are_created_replied_delivered_with_the_next_prompt_and_resolved() {
        let (fixture, db) = fixture().await;
        let fake = fast_fake();
        let mgr = manager(&fixture, db, &fake);
        let thread = mgr
            .create_thread(fixture.workspace.clone(), "feature".into())
            .await
            .unwrap();
        let tid = thread.id.clone();
        let mut rx = subscribed(&mgr, &tid).await;
        mgr.send_prompt(tid.clone(), "hello".into()).await.unwrap();
        wait_turn(&mut rx).await;
        let reply_msg = mgr
            .db
            .list_messages_by_thread(&tid)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(reply_msg.text(), "Hello from the mock agent.");
        let (repo, _) = checkout(&mgr, &tid).await;

        // Transcript span, code lines, and a reply.
        let on_text = mgr
            .create_comment(
                tid.clone(),
                CommentTarget::MessageRange {
                    message_id: reply_msg.id.clone(),
                    start_byte: 0,
                    end_byte: 5,
                },
                " Say it louder ".into(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(on_text.body, "Say it louder");
        assert_eq!(on_text.excerpt.as_deref(), Some("Hello"));
        assert_eq!(on_text.state, CommentState::Pending);
        assert!(on_text.anchor_status.is_none());
        assert!(matches!(
            rx.try_recv().unwrap(),
            DaemonMessage::CommentCreated(snapshot) if snapshot.id == on_text.id
        ));
        let on_code = mgr
            .create_comment(
                tid.clone(),
                code(&repo, "src/lib.rs", 2, 3),
                "Rename these".into(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            on_code.excerpt.as_deref(),
            Some("fn alpha() {}\nfn beta() {}")
        );
        assert_eq!(
            on_code.anchor_status,
            Some(AnchorStatus::Resolved { line: 2 })
        );
        let CommentTarget::Code { anchor } = &on_code.target else {
            panic!("code target");
        };
        assert_ne!(anchor.text_hash.as_deref(), Some("bogus"));
        let reply = mgr
            .create_comment(
                tid.clone(),
                CommentTarget::MessageRange {
                    message_id: MessageId::from("ignored"),
                    start_byte: 0,
                    end_byte: 1,
                },
                "And in Italian".into(),
                Some(on_text.id.clone()),
            )
            .await
            .unwrap();
        assert_eq!(reply.parent_comment_id.as_ref(), Some(&on_text.id));
        assert_eq!(reply.target, on_text.target);
        // A reply to a reply attaches to the root.
        let nested = mgr
            .create_comment(
                tid.clone(),
                on_text.target.clone(),
                "Grazie".into(),
                Some(reply.id.clone()),
            )
            .await
            .unwrap();
        assert_eq!(nested.parent_comment_id.as_ref(), Some(&on_text.id));
        let (_, deleted) = mgr.delete_comment(&nested.id).await.unwrap();
        assert_eq!(deleted, vec![nested.id.clone()]);

        // Invalid targets are rejected without storing anything.
        for (target, code_name) in [
            (code(&repo, "src/lib.rs", 4, 9), "invalid"),
            (code(&repo, "../secret", 1, 1), "invalid"),
            (code(&repo, "missing.rs", 1, 1), "invalid"),
            (
                CommentTarget::MessageRange {
                    message_id: reply_msg.id.clone(),
                    start_byte: 3,
                    end_byte: 3,
                },
                "invalid",
            ),
            (
                CommentTarget::MessageRange {
                    message_id: MessageId::from("nope"),
                    start_byte: 0,
                    end_byte: 1,
                },
                "not_found",
            ),
        ] {
            let err = mgr
                .create_comment(tid.clone(), target, "x".into(), None)
                .await
                .unwrap_err();
            assert_eq!(err.code(), code_name, "{err}");
        }
        let empty = mgr
            .create_comment(tid.clone(), on_text.target.clone(), "  ".into(), None)
            .await
            .unwrap_err();
        assert_eq!(empty.code(), "invalid");

        // A throwaway pending comment can be deleted with its replies.
        let doomed = mgr
            .create_comment(
                tid.clone(),
                code(&repo, "src/lib.rs", 4, 4),
                "meh".into(),
                None,
            )
            .await
            .unwrap();
        let doomed_reply = mgr
            .create_comment(
                tid.clone(),
                doomed.target.clone(),
                "meh too".into(),
                Some(doomed.id.clone()),
            )
            .await
            .unwrap();
        while rx.try_recv().is_ok() {}
        let (_, deleted) = mgr.delete_comment(&doomed.id).await.unwrap();
        assert_eq!(deleted, vec![doomed_reply.id.clone(), doomed.id.clone()]);
        assert!(matches!(
            rx.try_recv().unwrap(),
            DaemonMessage::CommentDeleted { comment_ids, .. } if comment_ids == deleted
        ));

        let listed = mgr.list_comments(&tid).await.unwrap();
        assert_eq!(
            listed.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            vec![on_text.id.clone(), on_code.id.clone(), reply.id.clone()]
        );
        assert!(listed.iter().all(|c| c.state == CommentState::Pending));

        // The next prompt carries every pending comment, quoted.
        let sent = mgr
            .send_prompt(tid.clone(), "please fix".into())
            .await
            .unwrap();
        let expected = format!(
            "{INTRO}\n[1] On this passage of your message:\n> Hello\nComment: Say it louder\nReply: And in Italian\n\n[2] On app/src/lib.rs lines 2-3:\n```\nfn alpha() {{}}\nfn beta() {{}}\n```\nComment: Rename these\n</review-comments>\n\nplease fix"
        );
        assert_eq!(sent.text(), expected);
        let seen = wait_turn(&mut rx).await;
        assert_eq!(prompts(&fake).last().unwrap(), &expected);
        let updates = seen
            .iter()
            .filter(|msg| matches!(msg, DaemonMessage::CommentUpdated(_)))
            .count();
        assert_eq!(updates, 6, "delivered, then answered, for each of 3");
        let answer = mgr
            .db
            .list_messages_by_thread(&tid)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let listed = mgr.list_comments(&tid).await.unwrap();
        for comment in &listed {
            let CommentState::Delivered(delivery) = &comment.state else {
                panic!("delivered: {:?}", comment.state);
            };
            assert_eq!(delivery.message_id, sent.id);
            assert_eq!(delivery.turn, 2);
            assert_eq!(delivery.answer_message_id.as_ref(), Some(&answer.id));
        }

        // Delivered comments cannot be deleted; roots resolve and reopen.
        assert_eq!(
            mgr.delete_comment(&on_code.id).await.unwrap_err().code(),
            "invalid"
        );
        assert_eq!(
            mgr.resolve_comment(&reply.id).await.unwrap_err().code(),
            "invalid"
        );
        let resolved = mgr.resolve_comment(&on_code.id).await.unwrap();
        let CommentState::Resolved { delivery, .. } = &resolved.state else {
            panic!("resolved");
        };
        assert_eq!(delivery.as_ref().unwrap().turn, 2);
        let late = mgr
            .create_comment(
                tid.clone(),
                on_code.target.clone(),
                "one more".into(),
                Some(on_code.id.clone()),
            )
            .await
            .unwrap_err();
        assert_eq!(late.code(), "invalid");
        let reopened = mgr.reopen_comment(&on_code.id).await.unwrap();
        assert!(matches!(reopened.state, CommentState::Delivered(_)));
        mgr.resolve_comment(&on_code.id).await.unwrap();

        // Nothing pending: the prompt goes out unchanged.
        mgr.send_prompt(tid.clone(), "next".into()).await.unwrap();
        wait_turn(&mut rx).await;
        assert_eq!(prompts(&fake).last().unwrap(), "next");
    }

    #[tokio::test]
    async fn send_comments_alone_replies_to_delivered_comments_and_restart() {
        let (fixture, db) = fixture().await;
        let fake = fast_fake();
        let mgr = manager(&fixture, db, &fake);
        let thread = mgr
            .create_thread(fixture.workspace.clone(), "feature".into())
            .await
            .unwrap();
        let tid = thread.id.clone();
        let (repo, _) = checkout(&mgr, &tid).await;
        let mut rx = subscribed(&mgr, &tid).await;
        assert_eq!(
            mgr.send_comments(tid.clone()).await.unwrap_err().code(),
            "invalid"
        );
        assert!(prompts(&fake).is_empty());

        let first = mgr
            .create_comment(
                tid.clone(),
                code(&repo, "src/lib.rs", 4, 4),
                "Why gamma?".into(),
                None,
            )
            .await
            .unwrap();
        let sent = mgr.send_comments(tid.clone()).await.unwrap();
        let block = format!(
            "{INTRO}\n[1] On app/src/lib.rs line 4:\n```\nfn gamma() {{}}\n```\nComment: Why gamma?\n</review-comments>"
        );
        assert_eq!(sent.text(), block);
        wait_turn(&mut rx).await;
        assert_eq!(prompts(&fake), vec![block]);

        // A reply to a delivered comment goes out with its context.
        let reply = mgr
            .create_comment(
                tid.clone(),
                first.target.clone(),
                "Keep it, but document it".into(),
                Some(first.id.clone()),
            )
            .await
            .unwrap();
        mgr.send_prompt(tid.clone(), "thanks".into()).await.unwrap();
        wait_turn(&mut rx).await;
        assert_eq!(
            prompts(&fake).last().unwrap(),
            &format!(
                "{INTRO}\n[1] On app/src/lib.rs line 4:\n```\nfn gamma() {{}}\n```\nEarlier comment: Why gamma?\nReply: Keep it, but document it\n</review-comments>\n\nthanks"
            )
        );
        let pending = mgr
            .create_comment(
                tid.clone(),
                code(&repo, "src/lib.rs", 1, 1),
                "later".into(),
                None,
            )
            .await
            .unwrap();
        let before = mgr.list_comments(&tid).await.unwrap();

        // Restart: states, turns, answers and anchors survive.
        drop(mgr);
        let reopened = TramaDb::open(&fixture.db_path).unwrap();
        let after = manager(&fixture, reopened, &fast_fake());
        let listed = after.list_comments(&tid).await.unwrap();
        assert_eq!(listed, before);
        let turn_of = |id: &CommentId| match &by_id(&listed, id).state {
            CommentState::Delivered(delivery) => {
                assert!(delivery.answer_message_id.is_some());
                delivery.turn
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(turn_of(&first.id), 1);
        assert_eq!(turn_of(&reply.id), 2);
        assert_eq!(by_id(&listed, &pending.id).state, CommentState::Pending);
        let kinds: Vec<String> = after
            .db
            .for_aggregate(&Aggregate::Thread { id: tid.clone() }, None, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.kind)
            .filter(|kind| kind.starts_with("comment."))
            .collect();
        assert_eq!(
            kinds,
            [
                "comment.created",
                "comment.delivered",
                "comment.answered",
                "comment.created",
                "comment.delivered",
                "comment.answered",
                "comment.created"
            ]
        );
    }

    #[tokio::test]
    async fn code_anchors_relocate_go_fuzzy_and_orphan_and_diff_hunks_quote_the_hunk() {
        let (fixture, db) = fixture().await;
        let fake = fast_fake();
        let mgr = manager(&fixture, db, &fake);
        let thread = mgr
            .create_thread(fixture.workspace.clone(), "feature".into())
            .await
            .unwrap();
        let tid = thread.id.clone();
        let (repo, path) = checkout(&mgr, &tid).await;
        let target = mgr
            .create_comment(
                tid.clone(),
                code(&repo, "notes.txt", 4, 5),
                "Split these".into(),
                None,
            )
            .await
            .unwrap();
        let status = |listed: Vec<CommentSnapshot>| by_id(&listed, &target.id).anchor_status;

        // Edit above: resolved at the new line, quoted from there.
        std::fs::write(path.join("notes.txt"), format!("new 1\nnew 2\n{NOTES}")).unwrap();
        assert_eq!(
            status(mgr.list_comments(&tid).await.unwrap()),
            Some(AnchorStatus::Resolved { line: 6 })
        );
        let delivery = mgr.pending_delivery(&tid).await.unwrap().unwrap();
        assert!(
            delivery.block.contains(
                "[1] On app/notes.txt lines 6-7 (written at lines 4-5; the code has moved):\n```\nTARGET ONE\nTARGET TWO\n```"
            ),
            "{}",
            delivery.block
        );

        // Anchored lines rewritten, context kept: fuzzy, quoted as written.
        std::fs::write(
            path.join("notes.txt"),
            NOTES.replace("TARGET ONE\nTARGET TWO", "REWRITTEN"),
        )
        .unwrap();
        assert_eq!(
            status(mgr.list_comments(&tid).await.unwrap()),
            Some(AnchorStatus::Fuzzy)
        );
        let delivery = mgr.pending_delivery(&tid).await.unwrap().unwrap();
        assert!(
            delivery.block.contains(
                "[1] On app/notes.txt lines 4-5 as it was when commented (that code has since changed):\n```\nTARGET ONE\nTARGET TWO\n```"
            ),
            "{}",
            delivery.block
        );

        // File deleted: orphaned, never reattached.
        std::fs::remove_file(path.join("notes.txt")).unwrap();
        assert_eq!(
            status(mgr.list_comments(&tid).await.unwrap()),
            Some(AnchorStatus::Orphaned)
        );
        let delivery = mgr.pending_delivery(&tid).await.unwrap().unwrap();
        assert!(
            delivery
                .block
                .contains("that code is no longer in the file")
        );
        std::fs::write(path.join("notes.txt"), "unrelated\n").unwrap();
        assert_eq!(
            status(mgr.list_comments(&tid).await.unwrap()),
            Some(AnchorStatus::Orphaned)
        );
        mgr.resolve_comment(&target.id).await.unwrap();

        // Diff hunk comments capture the hunk they point at.
        std::fs::write(path.join("src/lib.rs"), LIB.replace("beta", "BETA")).unwrap();
        let changes = mgr.get_changes(&tid, ChangeBase::ThreadBase).await.unwrap();
        let base = changes[0].base_revision.clone();
        let header = changes[0]
            .diff
            .lines()
            .skip_while(|line| *line != "+++ b/src/lib.rs")
            .find(|line| line.starts_with("@@"))
            .unwrap()
            .to_owned();
        let hunk = mgr
            .create_comment(
                tid.clone(),
                CommentTarget::DiffHunk {
                    repository_id: repo.clone(),
                    path: "src/lib.rs".into(),
                    base_revision: base.clone(),
                    hunk_header: header.clone(),
                },
                "Why uppercase?".into(),
                None,
            )
            .await
            .unwrap();
        let excerpt = hunk.excerpt.clone().unwrap();
        assert!(excerpt.starts_with(&header));
        assert!(
            excerpt.contains("-fn beta() {}\n+fn BETA() {}"),
            "{excerpt}"
        );
        assert!(hunk.anchor_status.is_none());
        let missing = mgr
            .create_comment(
                tid.clone(),
                CommentTarget::DiffHunk {
                    repository_id: repo,
                    path: "src/lib.rs".into(),
                    base_revision: base,
                    hunk_header: "@@ -90,1 +90,1 @@".into(),
                },
                "?".into(),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(missing.code(), "invalid");
        let delivery = mgr.pending_delivery(&tid).await.unwrap().unwrap();
        assert_eq!(delivery.ids, vec![hunk.id.clone()]);
        assert!(delivery.block.contains(&format!(
            "[1] On the diff of app/src/lib.rs at {header}:\n```diff\n{excerpt}\n```\nComment: Why uppercase?"
        )));
    }

    #[tokio::test]
    async fn subthread_comments_stay_in_the_subthread() {
        let (fixture, db) = fixture().await;
        let fake = fast_fake();
        let mgr = manager(&fixture, db, &fake);
        let parent = mgr
            .create_thread(fixture.workspace.clone(), "feature".into())
            .await
            .unwrap();
        let (repo, _) = checkout(&mgr, &parent.id).await;
        let on_parent = mgr
            .create_comment(
                parent.id.clone(),
                code(&repo, "src/lib.rs", 2, 2),
                "parent note".into(),
                None,
            )
            .await
            .unwrap();
        let sub = mgr
            .create_subthread(parent.id.clone(), SubthreadKind::Plain, None, None)
            .await
            .unwrap();
        assert!(mgr.list_comments(&sub.id).await.unwrap().is_empty());
        let (_, sub_path) = checkout(&mgr, &sub.id).await;
        std::fs::write(sub_path.join("src/lib.rs"), format!("// sub\n{LIB}")).unwrap();
        let on_sub = mgr
            .create_comment(
                sub.id.clone(),
                code(&repo, "src/lib.rs", 3, 3),
                "sub note".into(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(on_sub.excerpt.as_deref(), Some("fn alpha() {}"));
        // Cross-thread replies are refused.
        let crossed = mgr
            .create_comment(
                sub.id.clone(),
                on_parent.target.clone(),
                "x".into(),
                Some(on_parent.id.clone()),
            )
            .await
            .unwrap_err();
        assert_eq!(crossed.code(), "invalid");

        let mut rx = subscribed(&mgr, &sub.id).await;
        mgr.send_prompt(sub.id.clone(), "go".into()).await.unwrap();
        wait_turn(&mut rx).await;
        let prompt = prompts(&fake).pop().unwrap();
        assert!(
            prompt.contains("sub note") && !prompt.contains("parent note"),
            "{prompt}"
        );
        let parent_comments = mgr.list_comments(&parent.id).await.unwrap();
        assert_eq!(parent_comments.len(), 1);
        assert_eq!(parent_comments[0].state, CommentState::Pending);
        // Each anchor resolves against its own thread's checkout.
        assert_eq!(
            parent_comments[0].anchor_status,
            Some(AnchorStatus::Resolved { line: 2 })
        );
        let sub_comments = mgr.list_comments(&sub.id).await.unwrap();
        assert!(matches!(sub_comments[0].state, CommentState::Delivered(_)));
    }

    #[tokio::test]
    async fn comments_return_to_pending_when_the_agent_never_ran() {
        let (fixture, db) = fixture().await;
        let fake = FakeAdapter::fail_start();
        let mgr = manager(&fixture, db, &fake);
        let thread = mgr
            .create_thread(fixture.workspace.clone(), "feature".into())
            .await
            .unwrap();
        let tid = thread.id.clone();
        let (repo, _) = checkout(&mgr, &tid).await;
        let comment = mgr
            .create_comment(
                tid.clone(),
                code(&repo, "src/lib.rs", 2, 2),
                "note".into(),
                None,
            )
            .await
            .unwrap();
        let mut rx = subscribed(&mgr, &tid).await;
        mgr.send_comments(tid.clone()).await.unwrap();
        let mut states = Vec::new();
        loop {
            match timeout(Duration::from_secs(10), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                DaemonMessage::CommentUpdated(snapshot) => states.push(snapshot.state),
                DaemonMessage::Error { .. } => break,
                _ => {}
            }
        }
        assert!(matches!(states[0], CommentState::Delivered(_)));
        assert_eq!(states.last(), Some(&CommentState::Pending));
        let listed = mgr.list_comments(&tid).await.unwrap();
        assert_eq!(by_id(&listed, &comment.id).state, CommentState::Pending);
        assert!(mgr.pending_delivery(&tid).await.unwrap().is_some());
    }

    #[test]
    fn fences_outgrow_backticks_and_prompt_composes() {
        assert_eq!(fence_for("plain"), "```");
        assert_eq!(fence_for("has ``` inside"), "````");
        assert_eq!(compose_prompt(None, "hi"), "hi");
        assert_eq!(compose_prompt(Some("B"), "  "), "B");
        assert_eq!(compose_prompt(Some("B"), "hi"), "B\n\nhi");
    }

    #[test]
    fn paths_must_stay_inside_the_checkout() {
        assert!(checkout_relative(Path::new("src/lib.rs")).is_ok());
        assert!(checkout_relative(Path::new("./src/lib.rs")).is_ok());
        assert!(checkout_relative(Path::new("../x")).is_err());
        assert!(checkout_relative(Path::new("/etc/passwd")).is_err());
        assert!(checkout_relative(Path::new("")).is_err());
    }

    #[test]
    fn finds_hunks_by_path_and_header() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@ fn a\n-x\n+y\n@@ -9 +9 @@\n-z\n+w\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1,2 +1,2 @@\n-q\n+r\n";
        assert_eq!(
            find_hunk(diff, Path::new("a.rs"), "@@ -1,2 +1,2 @@").as_deref(),
            Some("@@ -1,2 +1,2 @@ fn a\n-x\n+y")
        );
        assert_eq!(
            find_hunk(diff, Path::new("a.rs"), "@@ -9 +9 @@").as_deref(),
            Some("@@ -9 +9 @@\n-z\n+w")
        );
        assert_eq!(
            find_hunk(diff, Path::new("b.rs"), "@@ -1,2 +1,2 @@").as_deref(),
            Some("@@ -1,2 +1,2 @@\n-q\n+r")
        );
        assert!(find_hunk(diff, Path::new("c.rs"), "@@ -1,2 +1,2 @@").is_none());
    }
}
