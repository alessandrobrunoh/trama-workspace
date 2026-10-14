use super::*;

// -- store ----------------------------------------------------------------

/// The selected thread's comments, oldest first.
pub struct CommentStore {
    client: DaemonClient,
    thread: Option<ThreadId>,
    comments: Vec<CommentSnapshot>,
    /// Set when a command could not be sent; cleared by the next success.
    pub notice: Option<String>,
}

impl CommentStore {
    pub fn new(client: DaemonClient) -> Self {
        Self {
            client,
            thread: None,
            comments: Vec::new(),
            notice: None,
        }
    }

    /// Follow the selected thread. The daemon does not replay comments on
    /// subscribe; the net layer sends `ListComments` with every selection.
    pub fn set_thread(&mut self, thread: Option<ThreadId>, cx: &mut Context<Self>) {
        if self.thread != thread {
            self.thread = thread;
            self.comments.clear();
            self.notice = None;
            cx.notify();
        }
    }

    /// Apply a daemon message. Returns whether it was a comment message.
    pub fn apply(&mut self, message: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        let Some(thread) = self.thread.clone() else {
            return is_comment_message(message);
        };
        let handled = is_comment_message(message);
        if apply_comment_message(&mut self.comments, &thread, message) {
            cx.notify();
        }
        handled
    }

    /// Show these comments for `thread` without a daemon (the demo).
    pub fn load_demo(&mut self, thread: ThreadId, comments: Vec<CommentSnapshot>) {
        self.thread = Some(thread);
        self.comments = comments;
    }

    pub fn groups(&self) -> Vec<CommentGroup> {
        group_comments(&self.comments)
    }

    pub(super) fn send(&mut self, message: ClientMessage, cx: &mut Context<Self>) -> SendStatus {
        let status = self.client.send(message);
        self.notice = match status {
            SendStatus::Enqueued => None,
            SendStatus::Rejected => {
                Some("The daemon is not connected; the comment change was not sent.".into())
            }
        };
        cx.notify();
        status
    }

    pub fn create(
        &mut self,
        target: CommentTarget,
        body: String,
        parent_comment_id: Option<CommentId>,
        cx: &mut Context<Self>,
    ) -> SendStatus {
        let Some(thread_id) = self.thread.clone() else {
            return SendStatus::Rejected;
        };
        self.send(
            ClientMessage::CreateComment {
                thread_id,
                target,
                body,
                parent_comment_id,
            },
            cx,
        )
    }

    pub fn resolve(&mut self, comment_id: CommentId, cx: &mut Context<Self>) {
        self.send(ClientMessage::ResolveComment { comment_id }, cx);
    }

    pub fn reopen(&mut self, comment_id: CommentId, cx: &mut Context<Self>) {
        self.send(ClientMessage::ReopenComment { comment_id }, cx);
    }

    /// Take a pending comment (and its pending replies) off the next send.
    /// It stays in the thread; nothing is deleted.
    pub fn detach(&mut self, comment_id: CommentId, cx: &mut Context<Self>) {
        self.send(ClientMessage::DetachComment { comment_id }, cx);
    }

    /// Put a detached comment back on the next send.
    pub fn attach(&mut self, comment_id: CommentId, cx: &mut Context<Self>) {
        self.send(ClientMessage::AttachComment { comment_id }, cx);
    }

    pub fn delete(&mut self, comment_id: CommentId, cx: &mut Context<Self>) {
        self.send(ClientMessage::DeleteComment { comment_id }, cx);
    }

    /// Deliver the pending comments as a turn of their own.
    pub fn send_pending(&mut self, cx: &mut Context<Self>) -> SendStatus {
        let Some(thread_id) = self.thread.clone() else {
            return SendStatus::Rejected;
        };
        self.send(ClientMessage::SendComments { thread_id }, cx)
    }
}

pub(super) fn is_comment_message(message: &DaemonMessage) -> bool {
    matches!(
        message,
        DaemonMessage::Comments { .. }
            | DaemonMessage::CommentCreated(_)
            | DaemonMessage::CommentUpdated(_)
            | DaemonMessage::CommentDeleted { .. }
    )
}

/// Fold one daemon message into `comments` (for `thread` only). Returns
/// whether anything changed.
pub fn apply_comment_message(
    comments: &mut Vec<CommentSnapshot>,
    thread: &ThreadId,
    message: &DaemonMessage,
) -> bool {
    match message {
        DaemonMessage::Comments {
            thread_id,
            comments: list,
        } if thread_id == thread => {
            *comments = list.clone();
            true
        }
        DaemonMessage::CommentCreated(snapshot) | DaemonMessage::CommentUpdated(snapshot)
            if &snapshot.thread_id == thread =>
        {
            match comments.iter_mut().find(|c| c.id == snapshot.id) {
                Some(existing) => *existing = snapshot.clone(),
                None => {
                    comments.push(snapshot.clone());
                    comments.sort_by_key(|comment| comment.created_at);
                }
            }
            true
        }
        DaemonMessage::CommentDeleted {
            thread_id,
            comment_ids,
        } if thread_id == thread => {
            let before = comments.len();
            comments.retain(|c| !comment_ids.contains(&c.id));
            comments.len() != before
        }
        _ => false,
    }
}

// -- grouping -------------------------------------------------------------

/// A root comment and its replies, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct CommentGroup {
    pub root: CommentSnapshot,
    pub replies: Vec<CommentSnapshot>,
}

/// Where a whole comment thread stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Something in it waits for the next prompt.
    Pending,
    /// Nothing waits for the next prompt and nothing was sent: the author
    /// took it off the send. It stays in the thread.
    Detached,
    /// Everything was sent; the latest delivery.
    Delivered(CommentDelivery),
    /// A person resolved the root; the latest delivery, if any.
    Resolved(Option<CommentDelivery>),
}

impl CommentGroup {
    pub fn id(&self) -> &CommentId {
        &self.root.id
    }

    pub fn all(&self) -> impl Iterator<Item = &CommentSnapshot> {
        std::iter::once(&self.root).chain(self.replies.iter())
    }

    pub fn len(&self) -> usize {
        1 + self.replies.len()
    }

    pub fn pending_count(&self) -> usize {
        if matches!(self.root.state, CommentState::Resolved { .. }) {
            return 0;
        }
        self.all()
            .filter(|c| c.state == CommentState::Pending)
            .count()
    }

    pub(super) fn latest_delivery(&self) -> Option<CommentDelivery> {
        self.all()
            .filter_map(|c| match &c.state {
                CommentState::Delivered(delivery) => Some(delivery.clone()),
                CommentState::Resolved { delivery, .. } => delivery.clone(),
                CommentState::Pending | CommentState::Detached { .. } => None,
            })
            .max_by_key(|delivery| delivery.turn)
    }

    pub fn phase(&self) -> Phase {
        if matches!(self.root.state, CommentState::Resolved { .. }) {
            return Phase::Resolved(self.latest_delivery());
        }
        if self.pending_count() > 0 {
            return Phase::Pending;
        }
        let detached = self
            .all()
            .any(|c| matches!(c.state, CommentState::Detached { .. }));
        match self.latest_delivery() {
            // A reply taken off the send does not hide what was sent.
            Some(delivery) => Phase::Delivered(delivery),
            None if detached => Phase::Detached,
            None => Phase::Pending,
        }
    }

    /// Open threads show as cards; delivered and resolved ones collapse
    /// into markers until expanded.
    pub fn open_by_default(&self) -> bool {
        self.phase() == Phase::Pending
    }
}

/// Group snapshots into threads: replies under their root (a reply to a
/// reply joins the same root), roots in creation order. A reply whose
/// parent is unknown stands alone rather than disappearing.
pub fn group_comments(comments: &[CommentSnapshot]) -> Vec<CommentGroup> {
    let root_of = |comment: &CommentSnapshot| -> Option<CommentId> {
        let mut current = comment;
        let mut hops = 0;
        while let Some(parent_id) = &current.parent_comment_id {
            current = comments.iter().find(|c| &c.id == parent_id)?;
            hops += 1;
            if hops > comments.len() {
                return None;
            }
        }
        Some(current.id.clone())
    };
    let mut groups: Vec<CommentGroup> = Vec::new();
    for comment in comments {
        let root = if comment.parent_comment_id.is_some() {
            root_of(comment).filter(|root| root != &comment.id)
        } else {
            None
        };
        match root.and_then(|root| groups.iter_mut().find(|g| g.root.id == root)) {
            Some(group) => group.replies.push(comment.clone()),
            None => groups.push(CommentGroup {
                root: comment.clone(),
                replies: Vec::new(),
            }),
        }
    }
    groups
}

/// Comments delivered with the user prompt `message_id`, oldest first:
/// the copy shown on a sent message. Detaching or clearing pending comments
/// never touches these.
pub fn delivered_with<'a>(
    groups: &'a [CommentGroup],
    message_id: &MessageId,
) -> Vec<&'a CommentSnapshot> {
    groups
        .iter()
        .flat_map(CommentGroup::all)
        .filter(|c| match &c.state {
            CommentState::Delivered(delivery) => &delivery.message_id == message_id,
            CommentState::Resolved {
                delivery: Some(delivery),
                ..
            } => &delivery.message_id == message_id,
            _ => false,
        })
        .collect()
}

/// Pending comments across all threads (roots and replies).
pub fn pending_total(groups: &[CommentGroup]) -> usize {
    groups.iter().map(CommentGroup::pending_count).sum()
}
