//! Per-turn checkpoints and forking a thread at a message.
//!
//! At the end of every completed turn each checkout's full working state is
//! committed through a throwaway index (the checkout's branch, index and
//! stash are untouched) and kept reachable by `refs/trama/checkpoints/<id>`;
//! the commits are stored in TramaDB against the turn's assistant message.
//! Forking at a message copies the parent's checkouts at that checkpoint and
//! resumes a truncated copy of the parent's Claude transcript that ends with
//! the same turn (`--resume <copy> --fork-session`).

use super::subthreads::{ForkConversation, ForkFiles};
use super::*;
use std::path::Path;
use trama_domain::checkpoint::{Checkpoint, CheckpointLabel, RepoState, WorkingChanges};
use trama_domain::ids::CheckpointId;
use trama_domain::subagent::SubagentProfile;
use trama_infra::claude::transcript;
use trama_protocol::SubthreadKind;

impl SessionManager {
    /// The thread whose checkouts `thread` works in: itself, or for a scout
    /// subagent the nearest ancestor that owns checkouts. Cycle-safe.
    pub(super) async fn checkout_owner(&self, thread: &Thread) -> Result<Thread> {
        let mut current = thread.clone();
        let mut seen = HashSet::from([current.id.clone()]);
        loop {
            let scout = self
                .db
                .get_subagent(&current.id)
                .await?
                .is_some_and(|record| record.spawned_by.profile == SubagentProfile::Scout);
            let Some(parent_id) = current.parent_thread_id.clone().filter(|_| scout) else {
                return Ok(current);
            };
            if !seen.insert(parent_id.clone()) {
                return Err(DaemonError::Invalid(format!(
                    "thread {} has a cyclic parent chain",
                    thread.id
                )));
            }
            current = self
                .db
                .get_thread(&parent_id)
                .await?
                .ok_or_else(|| DaemonError::NotFound(format!("thread {parent_id}")))?;
        }
    }

    /// Record the working state of every checkout as the checkpoint of the
    /// turn ended by `message_id`. Best effort: a failure is logged and only
    /// means that turn cannot be forked with its files.
    pub(super) async fn record_turn_checkpoint(
        &self,
        thread_id: &ThreadId,
        message_id: &MessageId,
    ) {
        if let Err(error) = self.try_record_turn_checkpoint(thread_id, message_id).await {
            crate::log::warn(format!("turn checkpoint failed for {thread_id}: {error}"));
        }
    }

    async fn try_record_turn_checkpoint(
        &self,
        thread_id: &ThreadId,
        message_id: &MessageId,
    ) -> Result<()> {
        let thread = self
            .db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let owner = self.checkout_owner(&thread).await?;
        // The turn's session already validated these checkouts.
        let mut paths = Vec::with_capacity(owner.repo_attachments.len());
        for attachment in &owner.repo_attachments {
            let Some(source) = self
                .db
                .get_repository(&attachment.repository_id)
                .await?
                .and_then(|repository| repository.local_path)
            else {
                continue;
            };
            let (path, _) = self.checkout_path(&owner.id, attachment, &source).await?;
            paths.push((attachment.repository_id.clone(), path));
        }
        if paths.is_empty() {
            return Ok(());
        }
        let id = CheckpointId::generate();
        let ref_id = id.to_string();
        let repo_states = tokio::task::spawn_blocking(move || {
            paths
                .into_iter()
                .map(|(repository_id, path)| {
                    let (commit, tree) = trama_infra::changes::record_checkpoint(&path, &ref_id)?;
                    let head_tree = trama_infra::changes::resolve_tree(&path, "HEAD").ok();
                    Ok(RepoState {
                        repository_id,
                        working_changes: if head_tree.as_deref() == Some(tree.as_str()) {
                            WorkingChanges::Clean
                        } else {
                            WorkingChanges::Dirty
                        },
                        commit: Some(commit),
                        tree: Some(tree),
                    })
                })
                .collect::<std::result::Result<Vec<_>, CheckoutError>>()
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("checkpoint task failed: {error}")))?
        .map_err(checkout_to_invalid)?;
        self.db
            .record_checkpoint(&Checkpoint {
                id,
                thread_id: thread_id.clone(),
                event_seq: 0,
                label: CheckpointLabel::Turn,
                repo_states,
                created_at: Utc::now(),
                message_id: Some(message_id.clone()),
            })
            .await?;
        Ok(())
    }

    /// `ForkThread`: a child of `thread_id`. Without `at_message_id` it is a
    /// regular subthread of the current state. With it, the child continues
    /// the conversation up to the end of the turn that message belongs to,
    /// with that turn's files.
    pub async fn fork_thread(
        &self,
        thread_id: ThreadId,
        at_message_id: Option<MessageId>,
        title: Option<String>,
        initial_prompt: Option<String>,
        kind: SubthreadKind,
    ) -> Result<ThreadSnapshot> {
        let Some(at_message_id) = at_message_id else {
            return self
                .create_subthread(thread_id, kind, title, initial_prompt)
                .await;
        };
        let thread = self
            .db
            .get_thread(&thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let (answer, checkpoint) = self.fork_point(&thread, &at_message_id).await?;
        let path = self.truncated_transcript(&thread, &answer).await?;
        self.create_child(
            thread.id.clone(),
            kind,
            title,
            initial_prompt,
            ForkFiles::AtCheckpoint(checkpoint.clone()),
            ForkConversation::Transcript {
                path: path.to_string_lossy().into_owned(),
                last_message: answer.id.clone(),
            },
            Some((answer.id, Some(checkpoint.id))),
        )
        .await
    }

    /// The assistant message that ends the turn `at` belongs to (`at`
    /// itself, or the last completed turn before it), and its checkpoint.
    async fn fork_point(&self, thread: &Thread, at: &MessageId) -> Result<(Message, Checkpoint)> {
        let messages = self.db.list_messages_by_thread(&thread.id).await?;
        let index = messages
            .iter()
            .position(|message| &message.id == at)
            .ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "message {at} is not one of this thread's own messages; fork from the thread that owns it"
                ))
            })?;
        let answer = messages[..=index]
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant && message.claude_uuid.is_some())
            .cloned()
            .ok_or_else(|| {
                DaemonError::Invalid(
                    "no completed agent turn with a fork point ends at or before this message; fork without a message to copy the current state"
                        .into(),
                )
            })?;
        let checkpoint = self
            .db
            .checkpoint_for_message(&thread.id, &answer.id)
            .await?
            .ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "the turn ending with message {} has no file checkpoint, so it cannot be forked with its files",
                    answer.id
                ))
            })?;
        Ok((answer, checkpoint))
    }

    /// Write the chain of the parent's Claude transcript that ends with
    /// `answer` to a private copy under `<data>/forks/` and return its path.
    async fn truncated_transcript(&self, thread: &Thread, answer: &Message) -> Result<PathBuf> {
        let leaf = answer
            .claude_uuid
            .clone()
            .ok_or_else(|| DaemonError::Invalid("the message has no transcript uuid".into()))?;
        let Actor::Agent { session } = &answer.actor else {
            return Err(DaemonError::Invalid(
                "only an agent message can end a forked conversation".into(),
            ));
        };
        let projects = self.agents_config.claude_projects_dir.clone();
        let mut sources = Vec::new();
        // The answer's own session first, then the thread's other sessions,
        // then what each ancestor's conversation was resumed from.
        let mut sessions = Vec::new();
        if let Some(own) = self.db.get_agent_session(session).await? {
            sessions.push(own);
        }
        sessions.extend(self.db.list_agent_sessions_for_thread(&thread.id).await?);
        let mut resumed = Vec::new();
        let mut current = thread.id.clone();
        let mut seen = HashSet::from([current.clone()]);
        while let Some(fork) = self.db.get_subthread_fork(&current).await? {
            if let Some(source) = fork.parent_external_session_id.clone() {
                resumed.push(source);
            }
            if !seen.insert(fork.parent_thread_id.clone()) {
                break;
            }
            sessions.extend(
                self.db
                    .list_agent_sessions_for_thread(&fork.parent_thread_id)
                    .await?,
            );
            current = fork.parent_thread_id;
        }
        let forks_dir = self.agents_config.data_root.join("forks");
        let destination = forks_dir.join(format!("{}.jsonl", ulid_like()));
        tokio::task::spawn_blocking(move || {
            for session in &sessions {
                let Some(external) = session.external_session_id.as_deref() else {
                    continue;
                };
                if let Some(path) =
                    transcript::find_session_transcript(&projects, &session.cwd, external)
                {
                    if !sources.contains(&path) {
                        sources.push(path);
                    }
                }
            }
            for source in &resumed {
                if let Some(path) =
                    transcript::find_session_transcript(&projects, Path::new("/"), source)
                {
                    if !sources.contains(&path) {
                        sources.push(path);
                    }
                }
            }
            if sources.is_empty() {
                return Err(DaemonError::Invalid(
                    "the agent's Claude Code transcript was not found, so the conversation cannot be forked"
                        .into(),
                ));
            }
            let content = transcript::truncate_transcript(&sources, &leaf)
                .map_err(|error| DaemonError::Invalid(error.to_string()))?;
            transcript::write_private(&destination, &content)
                .map_err(|error| DaemonError::Invalid(error.to_string()))?;
            Ok(destination)
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("transcript task failed: {error}")))?
    }
}

/// A fresh, sortable, path-safe name.
fn ulid_like() -> String {
    MessageId::generate().to_string()
}
