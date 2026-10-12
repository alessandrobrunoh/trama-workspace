//! Subthreads (plain, review, land): checkout copies, inherited
//! conversation, changes, pull changes, review verdicts and land results.

use super::*;
use std::path::Path;
use trama_db::{ReviewVerdictRecord, SubthreadFork};
use trama_domain::checkpoint::{RepoState, WorkingChanges};
use trama_domain::ids::RepositoryId;
use trama_domain::subthread::{LandOutcome, RepoForkPoint, RepoPullResult, ReviewVerdict};
use trama_domain::thread::ThreadKind;
use trama_infra::changes;
use trama_protocol::{ChangeBase, RepoChanges, SubthreadKind};

/// Line a land subthread ends its final message with.
pub(super) const LAND_RESULT_MARKER: &str = "TRAMA_LAND_RESULT:";
const REVIEW_VERDICT_REF: &str = "refs/trama/review-verdict";

/// Releases `Reservation::Checkouts` on the reserved threads when dropped.
struct CheckoutsGuard {
    inner: Arc<Mutex<Inner>>,
    threads: Vec<ThreadId>,
}

impl Drop for CheckoutsGuard {
    fn drop(&mut self) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for thread_id in &self.threads {
            if let Some(thread) = inner.threads.get_mut(thread_id) {
                if thread.reservation == Some(Reservation::Checkouts) {
                    thread.reservation = None;
                }
            }
        }
    }
}

/// One managed checkout of a thread, validated.
pub(super) struct ResolvedCheckout {
    pub(super) repository_id: RepositoryId,
    pub(super) title: String,
    pub(super) path: PathBuf,
    source: PathBuf,
    base_revision: String,
}

impl SessionManager {
    /// Reserve threads before any await so a turn, migration, fork or pull
    /// cannot run concurrently on them. Released by the returned guard.
    fn reserve_checkouts(&self, threads: &[ThreadId]) -> Result<CheckoutsGuard> {
        let mut inner = self.lock();
        if threads.iter().any(|id| {
            inner
                .threads
                .get(id)
                .is_some_and(|thread| thread.reservation.is_some())
        }) {
            return Err(DaemonError::Busy);
        }
        for id in threads {
            inner.threads.entry(id.clone()).or_default().reservation = Some(Reservation::Checkouts);
        }
        Ok(CheckoutsGuard {
            inner: Arc::clone(&self.inner),
            threads: threads.to_vec(),
        })
    }

    /// Validated managed checkouts of `thread`, in attachment order.
    pub(super) async fn resolve_checkouts(&self, thread: &Thread) -> Result<Vec<ResolvedCheckout>> {
        let memberships = match &thread.workspace_id {
            Some(workspace) => self.db.list_memberships(workspace).await?,
            None => Vec::new(),
        };
        let mut out = Vec::with_capacity(thread.repo_attachments.len());
        let mut validations = Vec::new();
        for attachment in &thread.repo_attachments {
            let (Some(_), Some(base_revision), AttachMode::Isolated) = (
                attachment.checkout_id.as_ref(),
                attachment.base_revision.as_ref(),
                attachment.attach_mode,
            ) else {
                return Err(DaemonError::Invalid(format!(
                    "thread {} has no managed checkout for repository {}. Migrate it (migrate_legacy_thread) first",
                    thread.id, attachment.repository_id
                )));
            };
            let repository = self
                .db
                .get_repository(&attachment.repository_id)
                .await?
                .ok_or_else(|| {
                    DaemonError::NotFound(format!("repository {}", attachment.repository_id))
                })?;
            let source = repository.local_path.ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "repository {} has no local path",
                    attachment.repository_id
                ))
            })?;
            let (path, repo_local) = self.checkout_path(&thread.id, attachment, &source).await?;
            validations.push((
                source.clone(),
                path.clone(),
                base_revision.clone(),
                repo_local,
            ));
            let title = memberships
                .iter()
                .find(|membership| membership.repository_id == attachment.repository_id)
                .and_then(|membership| membership.alias.clone())
                .unwrap_or_else(|| {
                    source
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| attachment.repository_id.to_string())
                });
            out.push(ResolvedCheckout {
                repository_id: attachment.repository_id.clone(),
                title,
                path,
                source,
                base_revision: base_revision.clone(),
            });
        }
        let checkout_manager = self.checkout_manager.clone();
        tokio::task::spawn_blocking(move || {
            for (source, path, base, repo_local) in validations {
                validate_managed_checkout(&checkout_manager, &source, &path, &base, repo_local)?;
            }
            Ok::<_, CheckoutError>(())
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("checkout validation task failed: {error}")))?
        .map_err(checkout_to_invalid)?;
        Ok(out)
    }

    /// Thread snapshots with the latest verdict filled in for review threads.
    pub(super) async fn thread_snapshots(&self, threads: &[Thread]) -> Result<Vec<ThreadSnapshot>> {
        let mut out = Vec::with_capacity(threads.len());
        for thread in threads {
            let mut snapshot = thread_snapshot(thread);
            if thread.kind == ThreadKind::Review {
                snapshot.review_verdict = self
                    .db
                    .latest_review_verdict(&thread.id)
                    .await?
                    .map(|record| record.verdict);
            }
            out.push(snapshot);
        }
        Ok(out)
    }

    /// Parent messages a subthread inherited (up to its fork point), so a
    /// subscriber sees the conversation it continues. They keep the parent's
    /// `thread_id` inside the message.
    pub(super) async fn inherited_messages(&self, thread_id: &ThreadId) -> Result<Vec<Message>> {
        let Some(fork) = self.db.get_subthread_fork(thread_id).await? else {
            return Ok(Vec::new());
        };
        let Some(last) = fork.fork_message_id else {
            return Ok(Vec::new());
        };
        let mut messages = self
            .db
            .list_messages_by_thread(&fork.parent_thread_id)
            .await?;
        match messages.iter().position(|message| message.id == last) {
            Some(index) => {
                messages.truncate(index + 1);
                Ok(messages)
            }
            None => Ok(Vec::new()),
        }
    }

    /// The parent's agent session a subthread forks on its first turn.
    pub(super) async fn inherited_session(&self, thread: &Thread) -> Result<Option<String>> {
        if thread.parent_thread_id.is_none() {
            return Ok(None);
        }
        Ok(self
            .db
            .get_subthread_fork(&thread.id)
            .await?
            .and_then(|fork| fork.parent_external_session_id))
    }

    pub async fn create_subthread(
        &self,
        parent_thread_id: ThreadId,
        kind: SubthreadKind,
        title: Option<String>,
        initial_prompt: Option<String>,
    ) -> Result<ThreadSnapshot> {
        let parent = self
            .db
            .get_thread(&parent_thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {parent_thread_id}")))?;
        let parent_title = parent.title.clone().unwrap_or_default();
        let title = match title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            Some(title) => validate_label(title, "subthread title")?,
            None => default_subthread_title(kind, &parent_title),
        };
        let mut thread = Thread::new_subthread(&parent, kind.thread_kind(), Some(title))?;
        let _reservation = self.reserve_checkouts(std::slice::from_ref(&parent.id))?;
        let parent_checkouts = self.resolve_checkouts(&parent).await?;

        let checkout_manager = self.checkout_manager.clone();
        let thread_id = thread.id.clone();
        let requests: Vec<_> = parent_checkouts
            .iter()
            .map(|checkout| {
                (
                    checkout.path.clone(),
                    checkout.source.clone(),
                    checkout.repository_id.clone(),
                    CheckoutId::generate(),
                )
            })
            .collect();
        let copies = tokio::task::spawn_blocking(move || {
            let mut copies = Vec::with_capacity(requests.len());
            for (parent_path, source, repository_id, checkout_id) in requests {
                match checkout_manager.provision_copy(
                    &parent_path,
                    &source,
                    &thread_id,
                    &repository_id,
                    &checkout_id,
                ) {
                    Ok(copy) => copies.push((repository_id, checkout_id, copy)),
                    Err(error) => {
                        remove_fresh_copies(&copies);
                        return Err(error);
                    }
                }
            }
            Ok(copies)
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("subthread copy task failed: {error}")))?
        .map_err(checkout_to_invalid)?;

        let mut locations = Vec::with_capacity(copies.len());
        let mut repos = Vec::with_capacity(copies.len());
        for (attachment, (repository_id, checkout_id, copy)) in
            thread.repo_attachments.iter_mut().zip(&copies)
        {
            attachment.checkout_id = Some(checkout_id.clone());
            attachment.base_revision = Some(copy.base_revision.clone());
            locations.push(CheckoutLocation {
                checkout_id: checkout_id.clone(),
                repository_id: repository_id.clone(),
                path: copy.path.clone(),
            });
            repos.push(RepoForkPoint {
                repository_id: repository_id.clone(),
                base_revision: copy.base_revision.clone(),
                fork_commit: copy.fork_commit.clone(),
            });
        }
        let parent_external_session_id = self
            .db
            .latest_agent_session_for_thread(&parent.id)
            .await?
            .and_then(|session| session.external_session_id);
        let fork_message_id = self
            .db
            .list_messages_by_thread(&parent.id)
            .await?
            .last()
            .map(|message| message.id.clone());
        let fork = SubthreadFork {
            thread_id: thread.id.clone(),
            parent_thread_id: parent.id.clone(),
            parent_external_session_id: parent_external_session_id.clone(),
            fork_message_id,
            repos: repos.clone(),
        };
        let aggregate = Aggregate::Thread {
            id: thread.id.clone(),
        };
        let events = [
            EventRecord::new(
                aggregate.clone(),
                Actor::System,
                DomainEvent::ThreadCreated {
                    thread: thread.clone(),
                },
                vec![],
            )?,
            EventRecord::new(
                aggregate,
                Actor::System,
                DomainEvent::SubthreadForked {
                    thread_id: thread.id.clone(),
                    parent_thread_id: parent.id.clone(),
                    repos,
                    parent_external_session_id,
                },
                vec![],
            )?,
        ];
        if let Err(error) = self
            .db
            .create_subthread(&thread, &fork, &locations, &events)
            .await
        {
            // The copies were just created and never recorded: no user work.
            let copies = copies.clone();
            let _ = tokio::task::spawn_blocking(move || remove_fresh_copies(&copies)).await;
            return Err(error.into());
        }
        drop(_reservation);
        let snapshot = thread_snapshot(&thread);
        self.broadcast(
            &parent.id,
            DaemonMessage::SubthreadCreated(snapshot.clone()),
        );

        let first_prompt = match kind {
            SubthreadKind::Plain => initial_prompt.filter(|text| !text.trim().is_empty()),
            SubthreadKind::Review => Some(review_prompt(
                &parent_title,
                &parent_checkouts,
                initial_prompt.as_deref(),
            )),
            SubthreadKind::Land => {
                let copies: Vec<_> = copies
                    .iter()
                    .map(|(_, _, copy)| copy.path.clone())
                    .collect();
                let skill = tokio::task::spawn_blocking(move || {
                    copies.iter().find_map(|p| find_land_skill(p))
                })
                .await
                .ok()
                .flatten();
                Some(land_prompt(
                    &parent_title,
                    &parent_checkouts,
                    skill.as_deref(),
                    initial_prompt.as_deref(),
                ))
            }
        };
        if let Some(text) = first_prompt {
            if let Err(error) = self.send_prompt(thread.id.clone(), text).await {
                self.append_system_notice(
                    &thread.id,
                    format!("The subthread's first prompt was not run: {error}"),
                )
                .await;
            }
        }
        Ok(snapshot)
    }

    pub async fn list_subthreads(
        &self,
        parent_thread_id: &ThreadId,
    ) -> Result<Vec<ThreadSnapshot>> {
        if self.db.get_thread(parent_thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {parent_thread_id}")));
        }
        let threads = self.db.list_subthreads(parent_thread_id).await?;
        self.thread_snapshots(&threads).await
    }

    /// Per-repository diff of the thread's working state against `base`.
    pub async fn get_changes(
        &self,
        thread_id: &ThreadId,
        base: ChangeBase,
    ) -> Result<Vec<RepoChanges>> {
        let thread = self
            .db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let fork = self.db.get_subthread_fork(thread_id).await?;
        let checkouts = self.resolve_checkouts(&thread).await?;
        let mut requests = Vec::with_capacity(checkouts.len());
        for checkout in checkouts {
            let base_rev = match base {
                ChangeBase::ThreadBase => fork
                    .as_ref()
                    .and_then(|fork| {
                        fork.repos
                            .iter()
                            .find(|repo| repo.repository_id == checkout.repository_id)
                            .map(|repo| repo.fork_commit.clone())
                    })
                    .unwrap_or_else(|| checkout.base_revision.clone()),
                ChangeBase::Head => "HEAD".to_owned(),
                ChangeBase::LastTurn => changes::LAST_TURN_REF.to_owned(),
            };
            requests.push((checkout, base_rev));
        }
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::with_capacity(requests.len());
            for (checkout, base_rev) in requests {
                let commit = changes::resolve_commit(&checkout.path, &base_rev)
                    .map_err(checkout_to_invalid)?
                    .ok_or_else(|| {
                        DaemonError::Invalid(match base {
                            ChangeBase::LastTurn => format!(
                                "no agent turn has started in {} yet, so there is no last-turn base",
                                checkout.title
                            ),
                            _ => format!("base {base_rev} is missing in {}", checkout.title),
                        })
                    })?;
                let diff = changes::diff(&checkout.path, &commit).map_err(checkout_to_invalid)?;
                out.push(RepoChanges {
                    repository_id: checkout.repository_id,
                    title: checkout.title,
                    base_revision: commit,
                    diff: diff.diff,
                    truncated: diff.truncated,
                    files: diff.files,
                });
            }
            Ok(out)
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("diff task failed: {error}")))?
    }

    /// Apply a subthread's changes since its fork point to its parent's
    /// checkouts. Never commits; conflicts are reported per file. Returns the
    /// parent thread id and per-repository results.
    pub async fn pull_changes(
        &self,
        subthread_id: &ThreadId,
    ) -> Result<(ThreadId, Vec<RepoPullResult>)> {
        let sub = self
            .db
            .get_thread(subthread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {subthread_id}")))?;
        let (Some(parent_id), Some(fork)) = (
            sub.parent_thread_id.clone(),
            self.db.get_subthread_fork(subthread_id).await?,
        ) else {
            return Err(DaemonError::Invalid(
                "only a subthread's changes can be pulled into its parent".into(),
            ));
        };
        let parent = self
            .db
            .get_thread(&parent_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {parent_id}")))?;
        let _reservation = self.reserve_checkouts(&[parent.id.clone(), sub.id.clone()])?;
        let sub_checkouts = self.resolve_checkouts(&sub).await?;
        let parent_checkouts = self.resolve_checkouts(&parent).await?;
        let mut requests = Vec::with_capacity(sub_checkouts.len());
        for checkout in sub_checkouts {
            let fork_commit = fork
                .repos
                .iter()
                .find(|repo| repo.repository_id == checkout.repository_id)
                .map(|repo| repo.fork_commit.clone())
                .ok_or_else(|| {
                    DaemonError::Invalid(format!(
                        "subthread has no fork point for {}",
                        checkout.title
                    ))
                })?;
            let target = parent_checkouts
                .iter()
                .find(|candidate| candidate.repository_id == checkout.repository_id)
                .map(|candidate| candidate.path.clone())
                .ok_or_else(|| {
                    DaemonError::Invalid(format!(
                        "the parent thread has no checkout for {}",
                        checkout.title
                    ))
                })?;
            requests.push((checkout.repository_id, checkout.path, fork_commit, target));
        }
        let repos = tokio::task::spawn_blocking(move || {
            requests
                .into_iter()
                .map(|(repository_id, path, fork_commit, target)| {
                    changes::pull(&path, &fork_commit, &target).map(|files| RepoPullResult {
                        repository_id,
                        files,
                    })
                })
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("pull task failed: {error}")))?
        .map_err(checkout_to_invalid)?;
        drop(_reservation);

        let message = system_message(&parent.id, pull_summary(&sub, &repos));
        let event = EventRecord::new(
            Aggregate::Thread {
                id: parent.id.clone(),
            },
            Actor::System,
            DomainEvent::ChangesPulled {
                from_thread_id: sub.id.clone(),
                into_thread_id: parent.id.clone(),
                repos: repos.clone(),
            },
            vec![],
        )?;
        self.db.append_message_with_event(&message, &event).await?;
        self.broadcast(
            &parent.id,
            DaemonMessage::MessageAppended {
                thread_id: parent.id.clone(),
                message,
            },
        );
        self.broadcast(
            &parent.id,
            DaemonMessage::ChangesPulled {
                subthread_id: sub.id.clone(),
                parent_thread_id: parent.id.clone(),
                repos: repos.clone(),
            },
        );
        Ok((parent.id, repos))
    }

    /// Record a review subthread's verdict with a snapshot of what the review
    /// changed, and post it to the parent thread. Returns the parent id.
    pub async fn submit_review_verdict(
        &self,
        review_thread_id: &ThreadId,
        verdict: ReviewVerdict,
    ) -> Result<ThreadId> {
        let review = self
            .db
            .get_thread(review_thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {review_thread_id}")))?;
        let (ThreadKind::Review, Some(parent_id)) = (review.kind, review.parent_thread_id.clone())
        else {
            return Err(DaemonError::Invalid(
                "a verdict can only be submitted for a review subthread".into(),
            ));
        };
        let verdict = match verdict {
            ReviewVerdict::RequestChanges { summary } => ReviewVerdict::RequestChanges {
                summary: validate_summary(&summary)?,
            },
            approve => approve,
        };
        let fork = self.db.get_subthread_fork(review_thread_id).await?;
        let checkouts = self.resolve_checkouts(&review).await?;
        let requests: Vec<_> = checkouts
            .into_iter()
            .map(|checkout| {
                let fork_commit = fork.as_ref().and_then(|fork| {
                    fork.repos
                        .iter()
                        .find(|repo| repo.repository_id == checkout.repository_id)
                        .map(|repo| repo.fork_commit.clone())
                });
                (checkout.repository_id, checkout.path, fork_commit)
            })
            .collect();
        let snapshot = tokio::task::spawn_blocking(move || {
            requests
                .into_iter()
                .map(|(repository_id, path, fork_commit)| {
                    let commit = changes::record_snapshot(
                        &path,
                        REVIEW_VERDICT_REF,
                        "trama: review verdict snapshot",
                    )?;
                    let tree = trama_infra::changes::resolve_tree(&path, &commit)?;
                    let fork_tree = match &fork_commit {
                        Some(fork) => Some(trama_infra::changes::resolve_tree(&path, fork)?),
                        None => None,
                    };
                    Ok(RepoState {
                        repository_id,
                        commit: Some(commit),
                        working_changes: if fork_tree.as_deref() == Some(tree.as_str()) {
                            WorkingChanges::Clean
                        } else if fork_tree.is_some() {
                            WorkingChanges::Dirty
                        } else {
                            WorkingChanges::Unknown
                        },
                        tree: Some(tree),
                    })
                })
                .collect::<std::result::Result<Vec<_>, CheckoutError>>()
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("review snapshot task failed: {error}")))?
        .map_err(checkout_to_invalid)?;

        let record = ReviewVerdictRecord {
            id: trama_domain::ids::CheckpointId::generate().to_string(),
            review_thread_id: review.id.clone(),
            parent_thread_id: parent_id.clone(),
            verdict: verdict.clone(),
            snapshot: snapshot.clone(),
            created_at: Utc::now(),
        };
        let message = system_message(&parent_id, verdict_text(&review, &verdict, &snapshot));
        let event = EventRecord::new(
            Aggregate::Thread {
                id: parent_id.clone(),
            },
            Actor::System,
            DomainEvent::ReviewVerdictRecorded {
                review_thread_id: review.id.clone(),
                parent_thread_id: parent_id.clone(),
                verdict: verdict.clone(),
                snapshot,
            },
            vec![],
        )?;
        self.db
            .record_review_verdict(&record, &message, &event)
            .await?;
        self.broadcast(
            &parent_id,
            DaemonMessage::MessageAppended {
                thread_id: parent_id.clone(),
                message,
            },
        );
        let posted = DaemonMessage::ReviewVerdictPosted {
            review_thread_id: review.id.clone(),
            parent_thread_id: parent_id.clone(),
            verdict,
        };
        self.broadcast(&parent_id, posted.clone());
        self.broadcast(&review.id, posted);
        Ok(parent_id)
    }

    /// Snapshot each managed checkout's working state at turn start, so
    /// `ChangeBase::LastTurn` can diff against it. Best effort.
    pub(super) async fn snapshot_turn_start(&self, thread_id: &ThreadId) {
        let Ok(Some(thread)) = self.db.get_thread(thread_id).await else {
            return;
        };
        // `send_prompt` validated these checkouts (session_config) before the
        // turn was accepted, so only their recorded paths are needed here.
        let mut paths = Vec::with_capacity(thread.repo_attachments.len());
        for attachment in &thread.repo_attachments {
            let Ok(Some(repository)) = self.db.get_repository(&attachment.repository_id).await
            else {
                continue;
            };
            let Some(source) = repository.local_path else {
                continue;
            };
            if let Ok((path, _)) = self.checkout_path(&thread.id, attachment, &source).await {
                paths.push(path);
            }
        }
        let result = tokio::task::spawn_blocking(move || {
            for path in paths {
                changes::record_snapshot(&path, changes::LAST_TURN_REF, "trama: turn start")?;
            }
            Ok::<_, CheckoutError>(())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => crate::log::warn(format!("turn snapshot failed: {error}")),
            Err(error) => crate::log::warn(format!("turn snapshot task failed: {error}")),
        }
    }

    /// A land subthread reported its result: record it on the parent and
    /// post a visible entry there.
    pub(super) async fn record_land_result(
        &self,
        thread_id: &ThreadId,
        outcome: LandOutcome,
        summary: String,
    ) {
        let Ok(Some(thread)) = self.db.get_thread(thread_id).await else {
            return;
        };
        let (ThreadKind::Land, Some(parent_id)) = (thread.kind, thread.parent_thread_id.clone())
        else {
            return;
        };
        let label = match outcome {
            LandOutcome::Landed => "landed the changes",
            LandOutcome::Failed => "could not land the changes",
            LandOutcome::Proposed => "proposed a land skill",
        };
        let message = system_message(
            &parent_id,
            format!(
                "Land subthread \"{}\" {label}: {summary}",
                thread.title.clone().unwrap_or_default()
            ),
        );
        let event = match EventRecord::new(
            Aggregate::Thread {
                id: parent_id.clone(),
            },
            Actor::System,
            DomainEvent::LandResultRecorded {
                land_thread_id: thread.id.clone(),
                parent_thread_id: parent_id.clone(),
                outcome,
                summary: summary.clone(),
            },
            vec![],
        ) {
            Ok(event) => event,
            Err(error) => {
                crate::log::warn(format!("failed to encode land result: {error}"));
                return;
            }
        };
        if let Err(error) = self.db.append_message_with_event(&message, &event).await {
            crate::log::warn(format!("failed to persist land result: {error}"));
            return;
        }
        self.broadcast(
            &parent_id,
            DaemonMessage::MessageAppended {
                thread_id: parent_id.clone(),
                message,
            },
        );
        let result = DaemonMessage::LandResult {
            land_thread_id: thread.id.clone(),
            parent_thread_id: parent_id.clone(),
            outcome,
            summary,
        };
        self.broadcast(&parent_id, result.clone());
        self.broadcast(&thread.id, result);
    }
}

fn remove_fresh_copies(
    copies: &[(
        RepositoryId,
        CheckoutId,
        trama_infra::checkout::ProvisionedCopy,
    )],
) {
    for (_, _, copy) in copies {
        let _ = std::fs::remove_dir_all(&copy.path);
    }
}

fn system_message(thread_id: &ThreadId, text: String) -> Message {
    Message {
        id: MessageId::generate(),
        thread_id: thread_id.clone(),
        parent_id: None,
        actor: Actor::System,
        role: Role::System,
        content: vec![ContentBlock::Text { text }],
        created_at: Utc::now(),
    }
}

fn validate_summary(summary: &str) -> Result<String> {
    let summary = summary.trim();
    if summary.is_empty() {
        return Err(DaemonError::Invalid(
            "requesting changes needs a summary of what to change".into(),
        ));
    }
    if summary.chars().count() > 4000 {
        return Err(DaemonError::Invalid(
            "review summary must be at most 4000 characters".into(),
        ));
    }
    Ok(summary.to_owned())
}

fn default_subthread_title(kind: SubthreadKind, parent_title: &str) -> String {
    let prefix = match kind {
        SubthreadKind::Plain => "Subthread",
        SubthreadKind::Review => "Review",
        SubthreadKind::Land => "Land",
    };
    let title = if parent_title.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {parent_title}")
    };
    title.chars().take(MAX_LABEL_LEN).collect()
}

fn repo_lines(checkouts: &[ResolvedCheckout]) -> String {
    checkouts
        .iter()
        .map(|checkout| {
            format!(
                "- {}: base revision {} (`git diff {}` shows every change, including uncommitted and untracked files)",
                checkout.title, checkout.base_revision, checkout.base_revision
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn review_prompt(
    parent_title: &str,
    checkouts: &[ResolvedCheckout],
    extra: Option<&str>,
) -> String {
    let mut prompt = format!(
        "You are a review subthread of the thread \"{parent_title}\". Your working directories are copies of that thread's checkouts at their current state, including uncommitted and untracked changes. The thread's changes are measured from these base revisions:\n{}\n\n\
         1. First write a change guide: an ordered walkthrough of the changes, grouping related edits and explaining the intent of each step so a reader can follow them in order.\n\
         2. Then review the changes for correctness bugs, risks, missing tests and simpler alternatives. You may fix issues in your copies; your edits stay here until the user pulls them into the parent thread.\n\
         3. End with a recommendation: approve, or request changes with a short summary. The user records the verdict.",
        repo_lines(checkouts)
    );
    if let Some(extra) = extra.map(str::trim).filter(|text| !text.is_empty()) {
        prompt.push_str("\n\nAdditional instructions from the user:\n");
        prompt.push_str(extra);
    }
    prompt
}

fn land_prompt(
    parent_title: &str,
    checkouts: &[ResolvedCheckout],
    skill: Option<&Path>,
    extra: Option<&str>,
) -> String {
    let mut prompt = format!(
        "You are a land subthread of the thread \"{parent_title}\". Your working directories are copies of that thread's checkouts at their current state, including uncommitted and untracked changes, based on:\n{}\n\n",
        repo_lines(checkouts)
    );
    match skill {
        Some(skill) => prompt.push_str(&format!(
            "Land these changes by following the repository's land skill at `{}`. Follow it exactly; ask before any step it marks as needing confirmation.",
            skill.display()
        )),
        None => prompt.push_str(
            "This repository has no land skill (a SKILL.md under .claude/skills or .agents/skills whose frontmatter has `metadata: { trama-action: land }`). Do not land anything. Instead propose one: inspect how this repository is integrated (branches, commit style, CI, pull requests, merge rules), draft that SKILL.md, and explain the steps it would take.",
        ),
    }
    prompt.push_str(&format!(
        "\n\nNever push, merge or open pull requests unless the skill says to and the user's permissions allow it. When you are done, end your final message with one line: `{LAND_RESULT_MARKER} <landed|failed|proposed> - <one-line summary>`."
    ));
    if let Some(extra) = extra.map(str::trim).filter(|text| !text.is_empty()) {
        prompt.push_str("\n\nAdditional instructions from the user:\n");
        prompt.push_str(extra);
    }
    prompt
}

/// First land skill in `checkout` (sorted), relative to it: a `SKILL.md`
/// under `.claude/skills/*/` or `.agents/skills/*/` whose frontmatter sets
/// `trama-action: land` (inside `metadata`).
pub(super) fn find_land_skill(checkout: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for root in [".claude/skills", ".agents/skills"] {
        let Ok(entries) = std::fs::read_dir(checkout.join(root)) else {
            continue;
        };
        for entry in entries.flatten() {
            let skill = Path::new(root).join(entry.file_name()).join("SKILL.md");
            candidates.push(skill);
        }
    }
    candidates.sort();
    candidates.into_iter().find(|relative| {
        std::fs::read_to_string(checkout.join(relative))
            .is_ok_and(|text| frontmatter_is_land(&text))
    })
}

fn frontmatter_is_land(text: &str) -> bool {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return false;
    }
    let frontmatter: Vec<&str> = lines.take_while(|line| line.trim() != "---").collect();
    let mut in_metadata = false;
    for line in frontmatter {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let trimmed = line.trim();
        if !indented {
            in_metadata = trimmed.starts_with("metadata:");
        }
        if !in_metadata {
            continue;
        }
        if let Some(index) = trimmed.find("trama-action") {
            let rest = trimmed[index + "trama-action".len()..]
                .trim_start_matches(['"', '\''])
                .trim_start();
            let Some(value) = rest.strip_prefix(':') else {
                continue;
            };
            let value = value.trim_start().trim_start_matches(['"', '\'']);
            if value.starts_with("land")
                && !value[4..]
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_alphanumeric() || ch == '-' || ch == '_')
            {
                return true;
            }
        }
    }
    false
}

/// `TRAMA_LAND_RESULT: <landed|failed|proposed> - <summary>` on its own
/// line (backticks allowed). The last such line wins.
pub(super) fn parse_land_result(text: &str) -> Option<(LandOutcome, String)> {
    text.lines().rev().find_map(|line| {
        let line = line.trim().trim_matches('`').trim();
        let rest = line.strip_prefix(LAND_RESULT_MARKER)?.trim();
        let (word, summary) = rest
            .split_once(|ch: char| ch.is_whitespace() || ch == ':')
            .unwrap_or((rest, ""));
        let outcome = match word.trim().to_ascii_lowercase().as_str() {
            "landed" => LandOutcome::Landed,
            "failed" => LandOutcome::Failed,
            "proposed" => LandOutcome::Proposed,
            _ => return None,
        };
        let summary = summary
            .trim()
            .trim_start_matches(['-', ':', '\u{2014}', '\u{2013}'])
            .trim();
        Some((outcome, summary.to_owned()))
    })
}

fn pull_summary(sub: &Thread, repos: &[RepoPullResult]) -> String {
    use trama_domain::subthread::PullFileOutcome;
    let files = repos.iter().flat_map(|repo| repo.files.iter());
    let (mut applied, mut merged, mut unchanged) = (0, 0, 0);
    let mut conflicts = Vec::new();
    for file in files {
        match &file.outcome {
            PullFileOutcome::Applied => applied += 1,
            PullFileOutcome::Merged => merged += 1,
            PullFileOutcome::Unchanged => unchanged += 1,
            PullFileOutcome::Conflict { .. } => conflicts.push(file.path.clone()),
        }
    }
    let mut text = format!(
        "Pulled changes from subthread \"{}\": {applied} applied, {merged} merged, {unchanged} already present, {} conflicts. Nothing was committed.",
        sub.title.clone().unwrap_or_default(),
        conflicts.len()
    );
    if !conflicts.is_empty() {
        text.push_str("\nConflicts: ");
        text.push_str(&conflicts.join(", "));
    }
    text
}

fn verdict_text(review: &Thread, verdict: &ReviewVerdict, snapshot: &[RepoState]) -> String {
    let title = review.title.clone().unwrap_or_default();
    let mut text = match verdict {
        ReviewVerdict::Approve => format!("Review \"{title}\" approved the changes."),
        ReviewVerdict::RequestChanges { summary } => {
            format!("Review \"{title}\" requested changes:\n{summary}")
        }
    };
    if snapshot
        .iter()
        .any(|state| state.working_changes == WorkingChanges::Dirty)
    {
        text.push_str(
            "\nThe review also edited files; use Pull Changes on the review to apply them.",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeAdapter, scripted_events};
    use tokio::time::{Duration, timeout};
    use trama_domain::agent::AgentEvent;
    use trama_domain::subthread::PullFileOutcome;
    use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership};

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Trama Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Trama Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    struct Fixture {
        dir: tempfile::TempDir,
        db_path: PathBuf,
        source: PathBuf,
        workspace: WorkspaceId,
    }

    /// File-backed DB with one workspace and one source repo. `skill`
    /// commits a land skill into the source.
    async fn fixture(skill: bool) -> (Fixture, TramaDb) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("trama.db");
        let db = TramaDb::open(&db_path).unwrap();
        let workspace = Workspace::new("subthreads");
        db.create_workspace(&workspace).await.unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        std::fs::write(source.join("same.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(source.join("conflict.txt"), "base\n").unwrap();
        if skill {
            let skill_dir = source.join(".claude/skills/ship");
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                "---\nname: ship\nmetadata: { trama-action: land }\n---\nPush and merge.\n",
            )
            .unwrap();
        }
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "initial"]);
        let repo = Repository::new(None, Some(source.clone()), Some("main".into()));
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
                source: source.canonicalize().unwrap(),
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

    async fn checkout(mgr: &SessionManager, thread_id: &ThreadId) -> PathBuf {
        let thread = mgr.db.get_thread(thread_id).await.unwrap().unwrap();
        mgr.resolve_checkouts(&thread).await.unwrap().remove(0).path
    }

    async fn wait_turn(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) {
        loop {
            let msg = timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("turn event")
                .expect("channel open");
            match msg {
                DaemonMessage::UsageSnapshot { .. } => return,
                DaemonMessage::AgentEventEnvelope {
                    event: AgentEvent::Failed { message },
                    ..
                } => panic!("turn failed: {message}"),
                _ => {}
            }
        }
    }

    async fn run_prompt(mgr: &SessionManager, thread_id: &ThreadId, text: &str) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sub = mgr.subscribe(thread_id, tx).await.unwrap();
        // Drain the replayed history first.
        while rx.try_recv().is_ok() {}
        mgr.send_prompt(thread_id.clone(), text.into())
            .await
            .unwrap();
        wait_turn(&mut rx).await;
        mgr.unsubscribe(thread_id, sub).unwrap();
    }

    fn configs(fake: &FakeAdapter) -> Vec<SessionConfig> {
        fake.start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn prompts(fake: &FakeAdapter) -> Vec<String> {
        fake.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[tokio::test]
    async fn subthread_copies_parent_state_inherits_conversation_and_survives_restart() {
        let (fixture, db) = fixture(false).await;
        let fake = fast_fake();
        let mgr = manager(&fixture, db, &fake);
        let parent = mgr
            .create_thread(fixture.workspace.clone(), "feature".into())
            .await
            .unwrap();
        run_prompt(&mgr, &parent.id, "build it").await;
        let parent_path = checkout(&mgr, &parent.id).await;
        std::fs::write(parent_path.join("same.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(parent_path.join("untracked.txt"), "new\n").unwrap();
        let parent_status = git(
            &parent_path,
            &["status", "--porcelain", "--untracked-files=all"],
        );

        let sub = mgr
            .create_subthread(parent.id.clone(), SubthreadKind::Plain, None, None)
            .await
            .unwrap();
        assert_eq!(sub.parent_thread_id.as_ref(), Some(&parent.id));
        assert_eq!(sub.kind, ThreadKind::Sub);
        assert_eq!(sub.title, "Subthread: feature");
        let sub_path = checkout(&mgr, &sub.id).await;
        assert!(
            sub_path.starts_with(
                fixture
                    .source
                    .join(".trama/worktrees")
                    .join(sub.id.to_string())
            )
        );
        assert_eq!(
            std::fs::read_to_string(sub_path.join("same.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
        assert_eq!(
            std::fs::read_to_string(sub_path.join("untracked.txt")).unwrap(),
            "new\n"
        );
        // Parent and primary repo untouched.
        assert_eq!(
            git(
                &parent_path,
                &["status", "--porcelain", "--untracked-files=all"]
            ),
            parent_status
        );
        assert_eq!(
            std::fs::read_to_string(fixture.source.join("same.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
        assert!(git(&fixture.source, &["status", "--porcelain"]).is_empty());
        // Diverges.
        std::fs::write(sub_path.join("same.txt"), "sub only\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(parent_path.join("same.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );

        // No nesting.
        let nested = mgr
            .create_subthread(sub.id.clone(), SubthreadKind::Plain, None, None)
            .await
            .unwrap_err();
        assert_eq!(nested.code(), "invalid");

        // Subscribing replays the inherited parent conversation first.
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&sub.id, tx).await.unwrap();
        let mut inherited = Vec::new();
        while let Ok(DaemonMessage::MessageAppended { message, .. }) = rx.try_recv() {
            inherited.push(message);
        }
        assert_eq!(inherited.len(), 2);
        assert!(
            inherited
                .iter()
                .all(|message| message.thread_id == parent.id)
        );

        // First turn forks the parent's Claude session in the copy.
        mgr.send_prompt(sub.id.clone(), "continue".into())
            .await
            .unwrap();
        wait_turn(&mut rx).await;
        let config = configs(&fake).pop().unwrap();
        assert_eq!(
            config.resume_external_session.as_deref(),
            Some("fake-ext-1")
        );
        assert!(config.fork_session);
        assert_eq!(config.cwd, sub_path);

        // Changes: thread base (fork point) vs head vs last turn.
        let since_fork = mgr
            .get_changes(&sub.id, ChangeBase::ThreadBase)
            .await
            .unwrap();
        assert_eq!(since_fork[0].files.len(), 1);
        assert_eq!(since_fork[0].files[0].path, "same.txt");
        assert_eq!(since_fork[0].title, "app");
        let since_head = mgr.get_changes(&sub.id, ChangeBase::Head).await.unwrap();
        assert_eq!(since_head[0].files.len(), 2);
        // The turn-start snapshot runs beside the turn; wait for it.
        let snapshot_path = sub_path.clone();
        timeout(Duration::from_secs(10), async move {
            while changes::resolve_commit(&snapshot_path, changes::LAST_TURN_REF)
                .unwrap()
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("turn-start snapshot");
        let since_turn = mgr
            .get_changes(&sub.id, ChangeBase::LastTurn)
            .await
            .unwrap();
        assert!(since_turn[0].files.is_empty());
        std::fs::write(sub_path.join("after-turn.txt"), "x\n").unwrap();
        let since_turn = mgr
            .get_changes(&sub.id, ChangeBase::LastTurn)
            .await
            .unwrap();
        assert_eq!(since_turn[0].files.len(), 1);

        // Restart: everything is persisted and the subthread resumes itself.
        drop(mgr);
        let reopened = TramaDb::open(&fixture.db_path).unwrap();
        let after = manager(&fixture, reopened, &FakeAdapter::scripted());
        let children = after.list_subthreads(&parent.id).await.unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, sub.id);
        let all = after.list_threads(&fixture.workspace).await.unwrap();
        assert!(
            all.iter()
                .any(|t| t.id == sub.id && t.parent_thread_id.as_ref() == Some(&parent.id))
        );
        let config = after.session_config(&sub.id).await.unwrap();
        assert!(!config.fork_session);
        assert!(config.resume_external_session.is_some());
        assert_eq!(config.cwd, sub_path);
        let since_fork = after
            .get_changes(&sub.id, ChangeBase::ThreadBase)
            .await
            .unwrap();
        assert_eq!(since_fork[0].files.len(), 2);
    }

    #[tokio::test]
    async fn pull_changes_applies_reports_conflicts_and_never_commits() {
        let (fixture, db) = fixture(false).await;
        let slow = FakeAdapter::new(scripted_events(), Duration::from_millis(60));
        let mgr = manager(&fixture, db, &slow);
        let parent = mgr
            .create_thread(fixture.workspace.clone(), "feature".into())
            .await
            .unwrap();
        // A running parent turn blocks forking.
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&parent.id, tx).await.unwrap();
        mgr.send_prompt(parent.id.clone(), "work".into())
            .await
            .unwrap();
        let busy = mgr
            .create_subthread(parent.id.clone(), SubthreadKind::Plain, None, None)
            .await
            .unwrap_err();
        assert_eq!(busy.code(), "busy");
        wait_turn(&mut rx).await;

        let sub = mgr
            .create_subthread(
                parent.id.clone(),
                SubthreadKind::Plain,
                Some("try".into()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(sub.title, "try");
        let parent_path = checkout(&mgr, &parent.id).await;
        let sub_path = checkout(&mgr, &sub.id).await;
        let parent_head = git(&parent_path, &["rev-parse", "HEAD"]);
        std::fs::write(sub_path.join("fresh.txt"), "from sub\n").unwrap();
        std::fs::write(sub_path.join("same.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(sub_path.join("conflict.txt"), "sub\n").unwrap();
        std::fs::write(parent_path.join("conflict.txt"), "parent\n").unwrap();

        let (parent_id, repos) = mgr.pull_changes(&sub.id).await.unwrap();
        assert_eq!(parent_id, parent.id);
        let files: std::collections::HashMap<_, _> = repos[0]
            .files
            .iter()
            .map(|file| (file.path.as_str(), &file.outcome))
            .collect();
        assert_eq!(files["fresh.txt"], &PullFileOutcome::Applied);
        assert_eq!(files["same.txt"], &PullFileOutcome::Applied);
        assert!(matches!(
            files["conflict.txt"],
            PullFileOutcome::Conflict {
                markers_written: true,
                ..
            }
        ));
        assert_eq!(
            std::fs::read_to_string(parent_path.join("fresh.txt")).unwrap(),
            "from sub\n"
        );
        assert!(
            std::fs::read_to_string(parent_path.join("conflict.txt"))
                .unwrap()
                .contains("<<<<<<<")
        );
        assert_eq!(git(&parent_path, &["rev-parse", "HEAD"]), parent_head);
        // Visible in the parent, and the source repo is untouched.
        let mut saw_pull = false;
        while let Ok(msg) = rx.try_recv() {
            saw_pull |= matches!(msg, DaemonMessage::ChangesPulled { .. });
        }
        assert!(saw_pull);
        let notes = mgr.db.list_messages_by_thread(&parent.id).await.unwrap();
        assert!(notes.iter().any(|m| m.text().contains("1 conflicts")));
        assert!(git(&fixture.source, &["status", "--porcelain"]).is_empty());
        // Only subthreads can be pulled.
        assert_eq!(
            mgr.pull_changes(&parent.id).await.unwrap_err().code(),
            "invalid"
        );
    }

    #[tokio::test]
    async fn review_verdict_is_persisted_posted_to_parent_and_pullable() {
        let (fixture, db) = fixture(false).await;
        let fake = fast_fake();
        let mgr = manager(&fixture, db, &fake);
        let parent = mgr
            .create_thread(fixture.workspace.clone(), "feature".into())
            .await
            .unwrap();
        let parent_path = checkout(&mgr, &parent.id).await;
        std::fs::write(parent_path.join("same.txt"), "one\nTWO\nthree\n").unwrap();

        let (review_tx, mut review_rx) = mpsc::unbounded_channel();
        let review = mgr
            .create_subthread(
                parent.id.clone(),
                SubthreadKind::Review,
                None,
                Some("focus on tests".into()),
            )
            .await
            .unwrap();
        assert_eq!(review.kind, ThreadKind::Review);
        mgr.subscribe(&review.id, review_tx).await.unwrap();
        // The first turn was started automatically with the change-guide prompt.
        let prompt = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(prompt) = prompts(&fake).pop() {
                    return prompt;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(prompt.contains("change guide"), "{prompt}");
        assert!(prompt.contains("focus on tests"));
        assert!(prompt.contains(&git(&parent_path, &["rev-parse", "HEAD"])));
        while timeout(Duration::from_millis(500), review_rx.recv())
            .await
            .is_ok()
        {}

        let review_path = checkout(&mgr, &review.id).await;
        std::fs::write(review_path.join("same.txt"), "one\nTWO\nthree\nreviewed\n").unwrap();
        let (parent_tx, mut parent_rx) = mpsc::unbounded_channel();
        mgr.subscribe(&parent.id, parent_tx).await.unwrap();
        while parent_rx.try_recv().is_ok() {}

        assert_eq!(
            mgr.submit_review_verdict(
                &review.id,
                ReviewVerdict::RequestChanges {
                    summary: "  ".into()
                }
            )
            .await
            .unwrap_err()
            .code(),
            "invalid"
        );
        let verdict = ReviewVerdict::RequestChanges {
            summary: "Add a regression test".into(),
        };
        assert_eq!(
            mgr.submit_review_verdict(&review.id, verdict.clone())
                .await
                .unwrap(),
            parent.id
        );
        let mut posted = false;
        let mut entry = None;
        while let Ok(msg) = parent_rx.try_recv() {
            match msg {
                DaemonMessage::ReviewVerdictPosted { verdict: v, .. } => posted = v == verdict,
                DaemonMessage::MessageAppended { message, .. } => entry = Some(message),
                _ => {}
            }
        }
        assert!(posted);
        let entry = entry.expect("verdict entry in parent");
        assert!(entry.text().contains("Add a regression test"));
        assert!(entry.text().contains("Pull Changes"));
        let stored = mgr
            .db
            .latest_review_verdict(&review.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.snapshot[0].working_changes, WorkingChanges::Dirty);
        // Verdicts only for review subthreads.
        assert_eq!(
            mgr.submit_review_verdict(&parent.id, ReviewVerdict::Approve)
                .await
                .unwrap_err()
                .code(),
            "invalid"
        );

        // Survives restart, visible in the snapshot and in the parent log.
        drop(mgr);
        let reopened = TramaDb::open(&fixture.db_path).unwrap();
        let after = manager(&fixture, reopened, &fast_fake());
        let children = after.list_subthreads(&parent.id).await.unwrap();
        assert_eq!(children[0].review_verdict, Some(verdict));
        let events = after
            .db
            .for_aggregate(
                &Aggregate::Thread {
                    id: parent.id.clone(),
                },
                None,
                50,
            )
            .await
            .unwrap();
        assert!(events.iter().any(|e| e.kind == "review.verdict_recorded"));
        assert!(
            after
                .db
                .list_messages_by_thread(&parent.id)
                .await
                .unwrap()
                .iter()
                .any(|m| m.text().contains("requested changes"))
        );
        // What the review changed can be pulled into the parent.
        let (_, repos) = after.pull_changes(&review.id).await.unwrap();
        assert_eq!(repos[0].files[0].outcome, PullFileOutcome::Applied);
        assert_eq!(
            std::fs::read_to_string(parent_path.join("same.txt")).unwrap(),
            "one\nTWO\nthree\nreviewed\n"
        );
    }

    #[tokio::test]
    async fn land_subthread_uses_repo_skill_and_records_reported_result() {
        let (fixture, db) = fixture(true).await;
        let mut events = scripted_events();
        events.insert(
            events.len() - 2,
            AgentEvent::TextDelta {
                text: "\nTRAMA_LAND_RESULT: landed - merged into main".into(),
            },
        );
        let fake = FakeAdapter::new(events, Duration::from_millis(1));
        let mgr = manager(&fixture, db, &fake);
        let parent = mgr
            .create_thread(fixture.workspace.clone(), "feature".into())
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&parent.id, tx).await.unwrap();
        let land = mgr
            .create_subthread(parent.id.clone(), SubthreadKind::Land, None, None)
            .await
            .unwrap();
        assert_eq!(land.kind, ThreadKind::Land);
        let result = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(DaemonMessage::LandResult {
                    outcome, summary, ..
                }) = rx.recv().await
                {
                    return (outcome, summary);
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result, (LandOutcome::Landed, "merged into main".into()));
        let prompt = prompts(&fake).pop().unwrap();
        assert!(prompt.contains(".claude/skills/ship/SKILL.md"), "{prompt}");
        assert!(prompt.contains(LAND_RESULT_MARKER));
        let events = mgr
            .db
            .for_aggregate(
                &Aggregate::Thread {
                    id: parent.id.clone(),
                },
                None,
                50,
            )
            .await
            .unwrap();
        assert!(events.iter().any(|e| e.kind == "land.result_recorded"));
        // Trama itself did not commit or push anything in the source.
        assert_eq!(
            git(&fixture.source, &["log", "--oneline"]).lines().count(),
            1
        );
    }

    #[test]
    fn land_prompt_proposes_a_skill_when_none_exists() {
        let temp = tempfile::tempdir().unwrap();
        assert!(find_land_skill(temp.path()).is_none());
        let prompt = land_prompt("feature", &[], None, None);
        assert!(prompt.contains("Instead propose one"));
        assert!(prompt.contains("trama-action: land"));
    }

    #[test]
    fn land_result_marker_is_parsed_from_the_last_matching_line() {
        assert_eq!(
            parse_land_result("done\nTRAMA_LAND_RESULT: landed - merged #12"),
            Some((LandOutcome::Landed, "merged #12".into()))
        );
        assert_eq!(
            parse_land_result("`TRAMA_LAND_RESULT: proposed \u{2014} drafted SKILL.md`"),
            Some((LandOutcome::Proposed, "drafted SKILL.md".into()))
        );
        assert_eq!(
            parse_land_result("TRAMA_LAND_RESULT: failed"),
            Some((LandOutcome::Failed, String::new()))
        );
        assert_eq!(parse_land_result("TRAMA_LAND_RESULT: maybe - hm"), None);
        assert_eq!(parse_land_result("no marker"), None);
    }

    #[test]
    fn land_skill_frontmatter_forms() {
        assert!(frontmatter_is_land(
            "---\nname: land\nmetadata: { trama-action: land }\n---\nbody"
        ));
        assert!(frontmatter_is_land(
            "---\nname: ship\nmetadata:\n  trama-action: \"land\"\n---\n"
        ));
        assert!(!frontmatter_is_land(
            "---\nname: x\nmetadata: { trama-action: landing }\n---\n"
        ));
        assert!(!frontmatter_is_land("---\ntrama-action: land\n---\n"));
        assert!(!frontmatter_is_land(
            "no frontmatter\nmetadata: { trama-action: land }"
        ));
    }
}
