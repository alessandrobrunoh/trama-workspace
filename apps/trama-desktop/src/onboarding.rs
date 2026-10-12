//! Pure onboarding state for an empty daemon database.
//!
//! The UI records a workspace name, an existing local Git directory, and an
//! alias, then a thread. Nothing here opens a socket, launches a shell, or
//! calls a model. A path is only a [`PathBuf`] the daemon is asked to attach.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use trama_domain::ids::{ThreadId, WorkspaceId};
use trama_protocol::{
    ClientMessage, PROTOCOL_VERSION, RepositorySnapshot, ThreadSnapshot, WorkspaceSnapshot,
};

pub const OFFLINE_BANNER: &str = "daemon offline -- reconnecting automatically. If it is not running, start `cargo run -p trama-daemon`, or use Start daemon. Workspace, repository, and thread changes are disabled.";

pub const MIGRATION_READY: &str = "Isolated checkout is ready. The transcript was preserved and your original files were not modified. This is a new Claude session, not a rewind of agent memory. Send your prompt again yourself -- nothing was sent automatically.";

/// How the desktop is currently attached to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Offline,
    Online,
    /// Handshake refused. The reconnect loop has stopped; the user restarts both sides.
    ProtocolMismatch {
        daemon_protocol: u32,
    },
}

/// One user or server step. Select requests are applied by the socket client;
/// the rest map onto [`ClientMessage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnboardingRequest {
    CreateWorkspace {
        name: String,
    },
    AddRepository {
        workspace_id: WorkspaceId,
        path: PathBuf,
        alias: String,
    },
    ListRepositories {
        workspace_id: WorkspaceId,
    },
    CreateThread {
        workspace_id: WorkspaceId,
        title: String,
    },
    MigrateLegacyThread {
        thread_id: ThreadId,
    },
    SelectWorkspace {
        workspace_id: WorkspaceId,
    },
    SelectThread {
        thread_id: ThreadId,
    },
}

impl OnboardingRequest {
    /// Wire message for database actions. Selection is not itself a client
    /// message; the socket layer turns it into list and subscribe calls.
    pub fn client_message(&self) -> Option<ClientMessage> {
        match self {
            Self::CreateWorkspace { name } => {
                Some(ClientMessage::CreateWorkspace { name: name.clone() })
            }
            Self::AddRepository {
                workspace_id,
                path,
                alias,
            } => Some(ClientMessage::AddRepository {
                workspace_id: workspace_id.clone(),
                path: path.clone(),
                alias: alias.clone(),
            }),
            Self::ListRepositories { workspace_id } => Some(ClientMessage::ListRepositories {
                workspace_id: workspace_id.clone(),
            }),
            Self::CreateThread {
                workspace_id,
                title,
            } => Some(ClientMessage::CreateThread {
                workspace_id: workspace_id.clone(),
                title: title.clone(),
            }),
            Self::MigrateLegacyThread { thread_id } => Some(ClientMessage::MigrateLegacyThread {
                thread_id: thread_id.clone(),
            }),
            Self::SelectWorkspace { .. } | Self::SelectThread { .. } => None,
        }
    }

    pub fn sends_prompt(&self) -> bool {
        matches!(
            self.client_message(),
            Some(ClientMessage::SendPrompt { .. })
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feedback {
    Progress(String),
    Error(String),
    Notice(String),
}

impl Feedback {
    pub fn text(&self) -> &str {
        match self {
            Self::Progress(text) | Self::Error(text) | Self::Notice(text) => text,
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Workspace,
    Repository,
    Thread,
    Migration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorContext {
    Workspace,
    Repository,
    Thread,
    Migration,
    LegacySend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Onboarding {
    link: Link,
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub selected_workspace: Option<WorkspaceId>,
    pub repositories: Vec<RepositorySnapshot>,
    repositories_for: Option<WorkspaceId>,
    pub threads: Vec<ThreadSnapshot>,
    pub selected_thread: Option<ThreadId>,
    pending: Option<Pending>,
    daemon_starting: bool,
    pub workspace_feedback: Option<Feedback>,
    pub repo_feedback: Option<Feedback>,
    pub thread_feedback: Option<Feedback>,
    pub migration_feedback: Option<Feedback>,
    pub daemon_feedback: Option<Feedback>,
    drafts: HashMap<ThreadId, String>,
}

impl Onboarding {
    pub fn new() -> Self {
        Self {
            link: Link::Offline,
            workspaces: Vec::new(),
            selected_workspace: None,
            repositories: Vec::new(),
            repositories_for: None,
            threads: Vec::new(),
            selected_thread: None,
            pending: None,
            daemon_starting: false,
            workspace_feedback: None,
            repo_feedback: None,
            thread_feedback: None,
            migration_feedback: None,
            daemon_feedback: None,
            drafts: HashMap::new(),
        }
    }

    /// Keep one composer draft per thread. An empty draft drops the entry.
    pub fn save_draft(&mut self, thread_id: ThreadId, text: String) {
        if text.trim().is_empty() {
            self.drafts.remove(&thread_id);
        } else {
            self.drafts.insert(thread_id, text);
        }
    }

    pub fn draft(&self, thread_id: &ThreadId) -> Option<&str> {
        self.drafts.get(thread_id).map(String::as_str)
    }

    /// Store a rejected prompt on its own thread. The text is returned only when
    /// that thread is selected, so another thread's composer is left alone.
    pub fn note_rejected_prompt(&mut self, thread_id: ThreadId, text: String) -> Option<String> {
        let selected = self.selected_thread.clone();
        self.save_draft(thread_id.clone(), text);
        if selected.as_ref() == Some(&thread_id) {
            self.draft(&thread_id).map(str::to_string)
        } else {
            None
        }
    }

    /// Latest error, so a failed create/add/thread stays visible while chat is idle.
    pub fn alert(&self) -> Option<&str> {
        [
            self.migration_feedback.as_ref(),
            self.repo_feedback.as_ref(),
            self.workspace_feedback.as_ref(),
            self.thread_feedback.as_ref(),
            self.daemon_feedback.as_ref(),
        ]
        .into_iter()
        .flatten()
        .find(|feedback| feedback.is_error())
        .map(Feedback::text)
    }

    pub fn can_start_daemon(&self) -> bool {
        matches!(self.link, Link::Offline) && !self.daemon_starting
    }

    pub fn begin_daemon_start(&mut self) -> bool {
        if !self.can_start_daemon() {
            return false;
        }
        self.daemon_starting = true;
        self.daemon_feedback = Some(Feedback::Progress(
            "Starting the local daemon...".to_string(),
        ));
        true
    }

    pub fn finish_daemon_start(&mut self, message: impl Into<String>, started: bool) {
        self.daemon_starting = false;
        let message = message.into();
        self.daemon_feedback = Some(if started {
            Feedback::Notice(message)
        } else {
            Feedback::Error(message)
        });
    }

    pub fn link(&self) -> &Link {
        &self.link
    }

    pub fn connected(&self) -> bool {
        matches!(self.link, Link::Online)
    }

    pub fn database_actions_enabled(&self) -> bool {
        self.connected() && self.pending.is_none()
    }

    pub fn set_link(&mut self, link: Link) {
        let online = matches!(link, Link::Online);
        if !online {
            let message = match &link {
                Link::ProtocolMismatch { daemon_protocol } => {
                    protocol_mismatch_guidance(*daemon_protocol)
                }
                _ => OFFLINE_BANNER.to_string(),
            };
            self.fail_pending(message);
            self.workspaces.clear();
            self.repositories.clear();
            self.repositories_for = None;
            self.threads.clear();
        }
        self.link = link;
    }

    pub fn banner(&self) -> String {
        match self.link {
            Link::Online => "daemon: connected".to_string(),
            Link::Offline => OFFLINE_BANNER.to_string(),
            Link::ProtocolMismatch { daemon_protocol } => {
                protocol_mismatch_guidance(daemon_protocol)
            }
        }
    }

    pub fn status_detail(&self) -> String {
        match self.link {
            Link::Online => String::new(),
            Link::Offline => {
                "Daemon offline. Reconnecting automatically. Database actions are disabled."
                    .to_string()
            }
            Link::ProtocolMismatch { .. } => self.banner(),
        }
    }

    pub fn can_create_workspace(&self) -> bool {
        self.database_actions_enabled()
    }

    pub fn can_add_repository(&self) -> bool {
        self.database_actions_enabled() && self.selected_workspace.is_some()
    }

    /// New threads snapshot repositories that are enabled by default. With
    /// none loaded, creation is refused instead of opening an empty thread.
    pub fn can_create_thread(&self) -> bool {
        self.database_actions_enabled() && self.has_default_repository()
    }

    pub fn has_default_repository(&self) -> bool {
        self.repositories_loaded() && self.repositories.iter().any(|repo| repo.default_enabled)
    }

    /// Recovery is available for whatever thread is selected. There is no
    /// legacy-detection flag to wait for.
    pub fn can_prepare_checkout(&self) -> bool {
        self.database_actions_enabled() && self.selected_thread.is_some()
    }

    pub fn displayed_repo_count(&self, workspace: &WorkspaceSnapshot) -> u32 {
        if self.repositories_for.as_ref() == Some(&workspace.id) {
            self.repositories.len() as u32
        } else {
            workspace.repo_count
        }
    }

    pub fn repositories_loaded(&self) -> bool {
        self.repositories_for.is_some()
            && self.repositories_for.as_ref() == self.selected_workspace.as_ref()
    }

    pub fn submit_workspace(&mut self, name: &str) -> Option<OnboardingRequest> {
        if !self.database_actions_enabled() {
            self.note_blocked(ErrorContext::Workspace);
            return None;
        }
        let name = name.trim();
        if name.is_empty() || name.chars().any(char::is_control) {
            self.workspace_feedback = Some(Feedback::Error("Enter a workspace name.".to_string()));
            return None;
        }
        self.pending = Some(Pending::Workspace);
        self.workspace_feedback = Some(Feedback::Progress("Creating workspace...".to_string()));
        Some(OnboardingRequest::CreateWorkspace {
            name: name.to_string(),
        })
    }

    pub fn submit_repository(&mut self, path: &str, alias: &str) -> Option<OnboardingRequest> {
        if !self.can_add_repository() {
            self.note_blocked(ErrorContext::Repository);
            return None;
        }
        let workspace_id = self.selected_workspace.clone()?;
        let (path, alias) = match prepare_repository(path, alias, &self.repositories) {
            Ok(ready) => ready,
            Err(message) => {
                self.repo_feedback = Some(Feedback::Error(message));
                return None;
            }
        };
        self.pending = Some(Pending::Repository);
        self.repo_feedback = Some(Feedback::Progress(
            "Adding existing local repository...".to_string(),
        ));
        Some(OnboardingRequest::AddRepository {
            workspace_id,
            path,
            alias,
        })
    }

    pub fn submit_thread(&mut self, title: &str) -> Option<OnboardingRequest> {
        let title = title.trim();
        if title.is_empty() {
            return None;
        }
        if !self.database_actions_enabled() || self.selected_workspace.is_none() {
            self.note_blocked(ErrorContext::Thread);
            return None;
        }
        if !self.has_default_repository() {
            self.thread_feedback = Some(Feedback::Error(
                "Add an existing local repository before creating a thread. New threads only attach repositories that are enabled by default."
                    .to_string(),
            ));
            return None;
        }
        let workspace_id = self.selected_workspace.clone()?;
        self.pending = Some(Pending::Thread);
        self.thread_feedback = Some(Feedback::Progress("Creating thread...".to_string()));
        Some(OnboardingRequest::CreateThread {
            workspace_id,
            title: title.to_string(),
        })
    }

    /// Sends [`ClientMessage::MigrateLegacyThread`] for the selected thread only.
    /// Does not send a prompt.
    pub fn confirm_migration(&mut self) -> Option<OnboardingRequest> {
        if !self.can_prepare_checkout() {
            self.note_blocked(ErrorContext::Migration);
            return None;
        }
        let thread_id = self.selected_thread.clone()?;
        self.pending = Some(Pending::Migration);
        self.migration_feedback = Some(Feedback::Progress(
            "Preparing isolated checkout...".to_string(),
        ));
        Some(OnboardingRequest::MigrateLegacyThread { thread_id })
    }

    pub fn note_picker_failure(&mut self, message: impl Into<String>) {
        self.repo_feedback = Some(Feedback::Error(message.into()));
    }

    pub fn select_workspace(&mut self, id: WorkspaceId) -> Vec<OnboardingRequest> {
        if self.selected_workspace.as_ref() != Some(&id) {
            self.selected_workspace = Some(id.clone());
            self.repositories.clear();
            self.repositories_for = None;
            self.threads.clear();
            self.selected_thread = None;
            if self.pending != Some(Pending::Migration) {
                self.migration_feedback = None;
            }
        }
        vec![
            OnboardingRequest::SelectWorkspace {
                workspace_id: id.clone(),
            },
            OnboardingRequest::ListRepositories { workspace_id: id },
        ]
    }

    pub fn select_thread(&mut self, id: ThreadId) -> Vec<OnboardingRequest> {
        if self.selected_thread.as_ref() != Some(&id) && self.pending != Some(Pending::Migration) {
            self.migration_feedback = None;
        }
        self.selected_thread = Some(id.clone());
        vec![OnboardingRequest::SelectThread { thread_id: id }]
    }

    pub fn apply_workspaces(&mut self, workspaces: Vec<WorkspaceSnapshot>) {
        self.workspaces = workspaces;
        if let Some(id) = &self.selected_workspace {
            if !self.workspaces.iter().any(|workspace| &workspace.id == id) {
                self.selected_workspace = None;
                self.repositories.clear();
                self.repositories_for = None;
                self.threads.clear();
                self.selected_thread = None;
            }
        }
    }

    pub fn apply_workspace_created(
        &mut self,
        snapshot: WorkspaceSnapshot,
    ) -> Vec<OnboardingRequest> {
        let id = snapshot.id.clone();
        upsert_workspace(&mut self.workspaces, snapshot);
        if self.pending == Some(Pending::Workspace) {
            self.pending = None;
        }
        self.workspace_feedback = Some(Feedback::Notice("Workspace saved.".to_string()));
        self.select_workspace(id)
    }

    pub fn apply_repositories(
        &mut self,
        workspace_id: WorkspaceId,
        repositories: Vec<RepositorySnapshot>,
    ) {
        if self.selected_workspace.as_ref() != Some(&workspace_id) {
            return;
        }
        self.repositories = repositories;
        self.repositories_for = Some(workspace_id.clone());
        let count = self.repositories.len() as u32;
        set_repo_count(&mut self.workspaces, &workspace_id, count);
    }

    pub fn apply_repository_added(
        &mut self,
        workspace_id: WorkspaceId,
        repository: RepositorySnapshot,
    ) {
        if self.selected_workspace.as_ref() != Some(&workspace_id) {
            return;
        }
        self.repositories_for = Some(workspace_id.clone());
        upsert_repository(&mut self.repositories, repository);
        let count = self.repositories.len() as u32;
        set_repo_count(&mut self.workspaces, &workspace_id, count);
        if self.pending == Some(Pending::Repository) {
            self.pending = None;
            self.repo_feedback = Some(Feedback::Notice("Repository saved.".to_string()));
        }
    }

    /// Replaces the thread list from the server. Does not choose a thread:
    /// a leftover adopted thread must not become the active one just because
    /// it is first, since the only recovery is an explicit action.
    pub fn apply_threads(&mut self, threads: Vec<ThreadSnapshot>) {
        self.threads = threads;
        if let Some(id) = &self.selected_thread {
            if !self.threads.iter().any(|thread| &thread.id == id) {
                self.selected_thread = None;
            }
        }
    }

    pub fn thread(&self, id: &ThreadId) -> Option<&ThreadSnapshot> {
        self.threads.iter().find(|thread| &thread.id == id)
    }

    /// Add or refresh a subthread without selecting it. Ignored unless its
    /// parent belongs to the threads shown (snapshots carry no workspace).
    pub fn apply_subthread(&mut self, snapshot: ThreadSnapshot) -> bool {
        let known = self.threads.iter().any(|thread| {
            thread.id == snapshot.id || Some(&thread.id) == snapshot.parent_thread_id.as_ref()
        });
        if known {
            upsert_thread(&mut self.threads, snapshot);
        }
        known
    }

    pub fn set_review_verdict(
        &mut self,
        review_thread_id: &ThreadId,
        verdict: trama_protocol::ReviewVerdict,
    ) {
        if let Some(thread) = self
            .threads
            .iter_mut()
            .find(|thread| &thread.id == review_thread_id)
        {
            thread.review_verdict = Some(verdict);
        }
    }

    pub fn apply_thread_created(&mut self, snapshot: ThreadSnapshot) -> Vec<OnboardingRequest> {
        let id = snapshot.id.clone();
        upsert_thread(&mut self.threads, snapshot);
        if self.pending == Some(Pending::Thread) {
            self.pending = None;
        }
        self.thread_feedback = Some(Feedback::Notice(
            "Thread created. Send a prompt when you are ready.".to_string(),
        ));
        self.select_thread(id)
    }

    /// Follow the migrated snapshot from the server. The returned select
    /// resubscribes history. It never includes a prompt.
    pub fn apply_thread_migrated(&mut self, snapshot: ThreadSnapshot) -> Vec<OnboardingRequest> {
        let id = snapshot.id.clone();
        upsert_thread(&mut self.threads, snapshot);
        if self.pending == Some(Pending::Migration) {
            self.pending = None;
        }
        self.migration_feedback = Some(Feedback::Notice(MIGRATION_READY.to_string()));
        self.selected_thread = Some(id.clone());
        vec![OnboardingRequest::SelectThread { thread_id: id }]
    }

    pub fn note_error(&mut self, code: &str, message: &str) {
        let context = match self.pending {
            Some(Pending::Workspace) => ErrorContext::Workspace,
            Some(Pending::Repository) => ErrorContext::Repository,
            Some(Pending::Thread) => ErrorContext::Thread,
            Some(Pending::Migration) => ErrorContext::Migration,
            None if code == "busy" || code == "invalid" => ErrorContext::LegacySend,
            None => {
                self.daemon_feedback = Some(Feedback::Error(actionable_error(
                    code,
                    message,
                    ErrorContext::Workspace,
                )));
                return;
            }
        };
        let text = actionable_error(code, message, context);
        match context {
            ErrorContext::Workspace => {
                self.pending = None;
                self.workspace_feedback = Some(Feedback::Error(text));
            }
            ErrorContext::Repository => {
                self.pending = None;
                self.repo_feedback = Some(Feedback::Error(text));
            }
            ErrorContext::Thread => {
                self.pending = None;
                self.thread_feedback = Some(Feedback::Error(text));
            }
            ErrorContext::Migration | ErrorContext::LegacySend => {
                if self.pending == Some(Pending::Migration) {
                    self.pending = None;
                }
                self.migration_feedback = Some(Feedback::Error(text));
            }
        }
    }

    fn note_blocked(&mut self, context: ErrorContext) {
        if self.pending.is_some() {
            return;
        }
        let text = match self.link {
            Link::ProtocolMismatch { .. } => self.banner(),
            Link::Offline => OFFLINE_BANNER.to_string(),
            Link::Online => match context {
                ErrorContext::Repository => {
                    "Select a workspace before adding a repository.".to_string()
                }
                ErrorContext::Thread => "Select a workspace before creating a thread.".to_string(),
                ErrorContext::Migration => {
                    "Select a thread to prepare an isolated checkout.".to_string()
                }
                ErrorContext::Workspace | ErrorContext::LegacySend => return,
            },
        };
        let feedback = Some(Feedback::Error(text));
        match context {
            ErrorContext::Workspace => self.workspace_feedback = feedback,
            ErrorContext::Repository => self.repo_feedback = feedback,
            ErrorContext::Thread => self.thread_feedback = feedback,
            ErrorContext::Migration | ErrorContext::LegacySend => {
                self.migration_feedback = feedback
            }
        }
    }

    fn fail_pending(&mut self, message: String) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let feedback = Some(Feedback::Error(message));
        match pending {
            Pending::Workspace => self.workspace_feedback = feedback,
            Pending::Repository => self.repo_feedback = feedback,
            Pending::Thread => self.thread_feedback = feedback,
            Pending::Migration => self.migration_feedback = feedback,
        }
    }
}

impl Default for Onboarding {
    fn default() -> Self {
        Self::new()
    }
}

pub fn protocol_mismatch_guidance(daemon_protocol: u32) -> String {
    format!(
        "Protocol mismatch: the daemon speaks v{daemon_protocol} and this app speaks v{PROTOCOL_VERSION}. Quit and restart both TramaWorkspace and the local daemon (`cargo run -p trama-daemon`) together. Do not start a second daemon. This only updates the transport handshake -- stored threads, workspace ids, and messages are not migrated. Workspace, repository, and thread changes stay disabled."
    )
}

/// Turn a typed or picked path into the directory the daemon should store.
/// This never runs a command.
pub fn prepare_repository(
    path: &str,
    alias: &str,
    existing: &[RepositorySnapshot],
) -> Result<(PathBuf, String), String> {
    let alias = alias.trim();
    if alias.is_empty() || alias.chars().any(char::is_control) {
        return Err("Enter an alias.".to_string());
    }
    if alias.contains('/') || alias.contains('\\') {
        return Err("Enter an alias, not a path.".to_string());
    }
    let raw = path.trim();
    if raw.is_empty() {
        return Err("Choose an existing local repository directory.".to_string());
    }
    if is_remote_locator(raw) {
        return Err(
            "Remote cloning is not available. Choose an existing local repository directory."
                .to_string(),
        );
    }
    let path = PathBuf::from(raw);
    if !path.is_dir() {
        return Err("That path is not an existing directory.".to_string());
    }
    if !path.join(".git").exists() {
        return Err(
            "That directory is not a Git repository. Choose the repository root.".to_string(),
        );
    }
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    if existing
        .iter()
        .any(|repo| same_directory(&repo.path, &path))
    {
        return Err("That repository path is already attached.".to_string());
    }
    if existing
        .iter()
        .any(|repo| repo.title.eq_ignore_ascii_case(alias))
    {
        return Err("That alias is already used in this workspace.".to_string());
    }
    Ok((path, alias.to_string()))
}

fn actionable_error(code: &str, message: &str, context: ErrorContext) -> String {
    let code = code.trim();
    let message = message.trim();
    let mut text = match (code.is_empty(), message.is_empty()) {
        (true, true) => "The daemon returned an empty error.".to_string(),
        (true, false) => message.to_string(),
        (false, true) => code.to_string(),
        (false, false) => format!("{code}: {message}"),
    };
    if source_has_uncommitted_changes(code, message) && !text.to_ascii_lowercase().contains("stash")
    {
        text.push_str(" Commit or stash changes in the source checkout, then try again.");
    }
    match context {
        ErrorContext::Migration | ErrorContext::LegacySend if code == "busy" => {
            text.push_str(" Wait until the live session finishes, then try Prepare isolated checkout again. Nothing was sent automatically.");
        }
        ErrorContext::LegacySend => {
            text.push_str(" Choose Prepare isolated checkout for this thread. The transcript stays, original files stay untouched, and you send the prompt again yourself afterwards.");
        }
        ErrorContext::Migration => {
            text.push_str(
                " Prepare isolated checkout did not finish. Nothing was sent automatically.",
            );
        }
        ErrorContext::Workspace | ErrorContext::Repository | ErrorContext::Thread => {}
    }
    text
}

fn source_has_uncommitted_changes(code: &str, message: &str) -> bool {
    let hay = format!("{code} {message}").to_ascii_lowercase();
    [
        "dirty",
        "unstaged",
        "uncommitted",
        "working tree",
        "local changes",
    ]
    .iter()
    .any(|needle| hay.contains(needle))
}

fn is_remote_locator(raw: &str) -> bool {
    raw.contains("://") || raw.starts_with("git@")
}

fn same_directory(stored: &Path, candidate: &Path) -> bool {
    match (
        std::fs::canonicalize(stored),
        std::fs::canonicalize(candidate),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => stored == candidate,
    }
}

fn upsert_workspace(workspaces: &mut Vec<WorkspaceSnapshot>, snapshot: WorkspaceSnapshot) {
    if let Some(existing) = workspaces
        .iter_mut()
        .find(|workspace| workspace.id == snapshot.id)
    {
        *existing = snapshot;
    } else {
        workspaces.push(snapshot);
    }
}

fn upsert_repository(repositories: &mut Vec<RepositorySnapshot>, repository: RepositorySnapshot) {
    if let Some(existing) = repositories.iter_mut().find(|row| row.id == repository.id) {
        *existing = repository;
    } else {
        repositories.push(repository);
    }
}

fn upsert_thread(threads: &mut Vec<ThreadSnapshot>, snapshot: ThreadSnapshot) {
    if let Some(existing) = threads.iter_mut().find(|thread| thread.id == snapshot.id) {
        *existing = snapshot;
    } else {
        threads.push(snapshot);
    }
}

fn set_repo_count(workspaces: &mut [WorkspaceSnapshot], id: &WorkspaceId, count: u32) {
    if let Some(workspace) = workspaces.iter_mut().find(|workspace| &workspace.id == id) {
        workspace.repo_count = count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use trama_domain::ids::RepositoryId;

    fn wid(value: &str) -> WorkspaceId {
        WorkspaceId::from(value)
    }

    fn tid(value: &str) -> ThreadId {
        ThreadId::from(value)
    }

    fn rid(value: &str) -> RepositoryId {
        RepositoryId::from(value)
    }

    fn online() -> Onboarding {
        let mut onboarding = Onboarding::new();
        onboarding.set_link(Link::Online);
        onboarding
    }

    fn workspace(id: &str, title: &str, repo_count: u32) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: wid(id),
            title: title.to_string(),
            repo_count,
        }
    }

    fn thread(id: &str, title: &str) -> ThreadSnapshot {
        ThreadSnapshot {
            id: tid(id),
            title: title.to_string(),
            repo_count: 0,
            parent_thread_id: None,
            kind: trama_protocol::ThreadKind::Top,
            review_verdict: None,
        }
    }

    fn git_root(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "trama-onboarding-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join(".git")).unwrap();
        path
    }

    fn repo(id: &str, title: &str, path: PathBuf) -> RepositorySnapshot {
        RepositorySnapshot {
            id: rid(id),
            title: title.to_string(),
            path,
            default_enabled: true,
        }
    }

    #[test]
    fn subthreads_join_the_list_only_under_a_known_parent_and_keep_verdicts() {
        let mut state = online();
        state.apply_threads(vec![thread("top", "Top")]);
        state.selected_thread = Some(tid("top"));
        let mut review = thread("rev", "Review");
        review.parent_thread_id = Some(tid("top"));
        review.kind = trama_protocol::ThreadKind::Review;
        assert!(state.apply_subthread(review.clone()));
        let mut stray = thread("stray", "Stray");
        stray.parent_thread_id = Some(tid("elsewhere"));
        assert!(!state.apply_subthread(stray));
        assert_eq!(state.threads.len(), 2);
        assert_eq!(
            state.selected_thread,
            Some(tid("top")),
            "creation does not select"
        );

        state.set_review_verdict(&tid("rev"), trama_protocol::ReviewVerdict::Approve);
        assert_eq!(
            state
                .thread(&tid("rev"))
                .and_then(|t| t.review_verdict.clone()),
            Some(trama_protocol::ReviewVerdict::Approve)
        );
        assert!(
            state.apply_subthread(review),
            "re-broadcast upserts in place"
        );
        assert_eq!(state.threads.len(), 2);
    }

    #[test]
    fn empty_database_flow_creates_workspace_repo_and_thread() {
        let mut onboarding = online();
        assert!(onboarding.workspaces.is_empty());
        assert!(onboarding.submit_workspace("   ").is_none());

        let create = onboarding.submit_workspace("  Demo  ").unwrap();
        assert_eq!(
            create.client_message(),
            Some(ClientMessage::CreateWorkspace {
                name: "Demo".into()
            })
        );
        assert!(
            onboarding.workspaces.is_empty(),
            "no row before the daemon saves it"
        );
        onboarding.note_error("invalid", "could not create workspace");
        assert!(
            onboarding
                .alert()
                .unwrap()
                .contains("could not create workspace")
        );
        onboarding.workspace_feedback = None;
        onboarding.pending = None;

        let created = onboarding.apply_workspace_created(workspace("w1", "Demo", 0));
        assert_eq!(onboarding.workspaces.len(), 1);
        assert_eq!(onboarding.selected_workspace, Some(wid("w1")));
        assert!(created.iter().any(|request| {
            request.client_message()
                == Some(ClientMessage::ListRepositories {
                    workspace_id: wid("w1"),
                })
        }));
        assert!(created.iter().all(|request| !request.sends_prompt()));
        assert!(onboarding.submit_thread("too soon").is_none());
        assert!(
            onboarding
                .thread_feedback
                .as_ref()
                .unwrap()
                .text()
                .contains("enabled by default")
        );
        assert_eq!(
            onboarding.alert(),
            onboarding.thread_feedback.as_ref().map(Feedback::text)
        );

        let root = git_root("flow");
        let add = onboarding
            .submit_repository(root.to_str().unwrap(), " api ")
            .unwrap();
        let ClientMessage::AddRepository {
            workspace_id,
            path,
            alias,
        } = add.client_message().unwrap()
        else {
            panic!("add repository request");
        };
        assert_eq!(workspace_id, wid("w1"));
        assert_eq!(alias, "api");
        assert_eq!(path, std::fs::canonicalize(&root).unwrap());

        onboarding.apply_repository_added(wid("w1"), repo("r1", "api", path.clone()));
        assert_eq!(onboarding.repositories.len(), 1);
        assert_eq!(onboarding.repositories[0].title, "api");
        assert_eq!(onboarding.repositories[0].path, path);
        assert_eq!(
            onboarding.displayed_repo_count(&onboarding.workspaces[0]),
            1
        );

        let thread = onboarding.submit_thread("Fix auth").unwrap();
        assert_eq!(
            thread.client_message(),
            Some(ClientMessage::CreateThread {
                workspace_id: wid("w1"),
                title: "Fix auth".into(),
            })
        );
        assert!(!thread.sends_prompt());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn duplicate_add_is_refused_and_duplicate_reply_stays_one_row() {
        let mut onboarding = online();
        onboarding.apply_workspace_created(workspace("w1", "Demo", 0));
        let root = git_root("dup");
        let canonical = std::fs::canonicalize(&root).unwrap();
        onboarding
            .submit_repository(root.to_str().unwrap(), "api")
            .unwrap();
        onboarding.apply_repository_added(wid("w1"), repo("r1", "api", canonical.clone()));

        assert!(
            onboarding
                .submit_repository(root.to_str().unwrap(), "other")
                .is_none()
        );
        assert!(
            onboarding
                .repo_feedback
                .unwrap()
                .text()
                .contains("already attached")
        );

        onboarding.repo_feedback = None;
        onboarding.pending = None;
        let second_root = git_root("duplicate-alias");
        assert!(
            onboarding
                .submit_repository(second_root.to_str().unwrap(), "api")
                .is_none()
        );
        assert!(
            onboarding
                .repo_feedback
                .as_ref()
                .unwrap()
                .text()
                .contains("alias")
        );

        onboarding.apply_repository_added(wid("w1"), repo("r1", "api", canonical));
        assert_eq!(onboarding.repositories.len(), 1);
        assert_eq!(
            onboarding.displayed_repo_count(&onboarding.workspaces[0]),
            1
        );
        let _ = std::fs::remove_dir_all(second_root);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn server_error_stays_visible_across_list_refresh() {
        let mut onboarding = online();
        onboarding.apply_workspace_created(workspace("w1", "Demo", 3));
        onboarding.select_thread(tid("old"));
        assert!(onboarding.confirm_migration().is_some());
        onboarding.note_error("invalid", "source checkout is dirty");
        let shown = onboarding.migration_feedback.clone().unwrap();
        assert!(shown.is_error());
        assert!(shown.text().contains("invalid"));
        assert!(shown.text().contains("dirty"));
        assert!(shown.text().to_ascii_lowercase().contains("stash"));
        assert!(shown.text().contains("Nothing was sent automatically"));

        onboarding.apply_workspaces(vec![workspace("w1", "Demo", 3)]);
        onboarding.apply_threads(vec![thread("old", "Legacy")]);
        assert_eq!(onboarding.migration_feedback, Some(shown));
    }

    #[test]
    fn disconnected_actions_are_disabled() {
        let mut onboarding = Onboarding::new();
        assert!(!onboarding.database_actions_enabled());
        assert!(onboarding.submit_workspace("Demo").is_none());
        assert!(onboarding.submit_repository("/tmp", "api").is_none());
        assert!(onboarding.submit_thread("Fix").is_none());
        assert!(onboarding.confirm_migration().is_none());
        assert!(!onboarding.can_prepare_checkout());
        assert!(onboarding.banner().contains("reconnecting automatically"));
        assert!(onboarding.banner().contains("disabled"));
    }

    #[test]
    fn migration_requests_the_selected_thread_and_does_not_send_a_prompt() {
        let mut onboarding = online();
        onboarding.apply_threads(vec![thread("old", "Legacy"), thread("other", "Other")]);
        assert!(onboarding.selected_thread.is_none());
        assert!(!onboarding.can_prepare_checkout());

        onboarding.select_thread(tid("old"));
        assert!(onboarding.can_prepare_checkout());
        let request = onboarding.confirm_migration().unwrap();
        assert_eq!(
            request.client_message(),
            Some(ClientMessage::MigrateLegacyThread {
                thread_id: tid("old"),
            })
        );
        assert!(!request.sends_prompt());

        let effect = onboarding.apply_thread_migrated(thread("fresh", "Legacy"));
        assert_eq!(onboarding.selected_thread, Some(tid("fresh")));
        assert_eq!(
            effect,
            vec![OnboardingRequest::SelectThread {
                thread_id: tid("fresh"),
            }]
        );
        assert!(effect.iter().all(|request| !request.sends_prompt()));
        let notice = onboarding.migration_feedback.as_ref().unwrap();
        assert!(!notice.is_error());
        assert!(notice.text().contains("transcript was preserved"));
        assert!(notice.text().contains("not a rewind of agent memory"));
        assert!(notice.text().contains("nothing was sent automatically"));

        onboarding.apply_threads(vec![thread("old", "Legacy")]);
        assert_eq!(
            onboarding.selected_thread, None,
            "a refresh that only contains the old thread must not select it"
        );
    }

    #[test]
    fn busy_and_invalid_legacy_errors_stay_on_the_recovery_panel() {
        let mut onboarding = online();
        onboarding.select_thread(tid("old"));
        onboarding.note_error("busy", "session is live");
        let busy = onboarding
            .migration_feedback
            .as_ref()
            .unwrap()
            .text()
            .to_string();
        assert!(busy.contains("busy"));
        assert!(busy.contains("session is live"));
        assert!(busy.to_ascii_lowercase().contains("nothing was sent"));

        onboarding.note_error("invalid", "adopted checkout cannot start");
        let invalid = onboarding
            .migration_feedback
            .as_ref()
            .unwrap()
            .text()
            .to_string();
        assert!(invalid.contains("adopted checkout cannot start"));
        assert!(invalid.contains("Prepare isolated checkout"));
    }

    #[test]
    fn remote_urls_and_command_looking_paths_are_not_accepted() {
        let existing = [];
        let remote = prepare_repository("https://example.com/repo.git", "api", &existing);
        assert!(
            remote
                .unwrap_err()
                .contains("Remote cloning is not available")
        );
        let ssh = prepare_repository("git@example.com:org/repo.git", "api", &existing);
        assert!(ssh.unwrap_err().contains("existing local"));
        let injected = prepare_repository("/tmp/nope; rm -rf /", "api", &existing);
        assert!(injected.is_err());
        assert!(injected.unwrap_err().contains("not an existing directory"));
    }

    #[test]
    fn repo_count_uses_the_loaded_rows_and_otherwise_the_server() {
        let mut onboarding = online();
        onboarding.apply_workspaces(vec![workspace("w1", "Demo", 4)]);
        onboarding.select_workspace(wid("w1"));
        assert_eq!(
            onboarding.displayed_repo_count(&onboarding.workspaces[0]),
            4
        );

        let root = git_root("count");
        onboarding.apply_repositories(
            wid("w1"),
            vec![repo("r1", "api", std::fs::canonicalize(&root).unwrap())],
        );
        assert_eq!(
            onboarding.displayed_repo_count(&onboarding.workspaces[0]),
            1
        );
        assert_eq!(onboarding.workspaces[0].repo_count, 1);

        let mut disabled = repo("r2", "off", std::fs::canonicalize(&root).unwrap());
        disabled.default_enabled = false;
        onboarding.repositories = vec![disabled];
        onboarding.repositories_for = Some(wid("w1"));
        assert!(!onboarding.can_create_thread());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejected_prompt_does_not_overwrite_another_threads_draft() {
        let mut onboarding = online();
        onboarding.save_draft(tid("a"), "keep a".into());
        onboarding.save_draft(tid("b"), "keep b".into());
        onboarding.select_thread(tid("b"));
        assert!(
            onboarding
                .note_rejected_prompt(tid("a"), "resent a".into())
                .is_none()
        );
        assert_eq!(onboarding.draft(&tid("a")), Some("resent a"));
        assert_eq!(onboarding.draft(&tid("b")), Some("keep b"));
        assert_eq!(
            onboarding.note_rejected_prompt(tid("b"), "resent b".into()),
            Some("resent b".into())
        );
    }

    #[test]
    fn protocol_mismatch_tells_the_user_to_restart_both_sides() {
        let guidance = protocol_mismatch_guidance(1);
        assert!(guidance.contains(&format!("v{PROTOCOL_VERSION}")));
        assert!(guidance.contains("v1"));
        assert!(guidance.contains("restart both"));
        assert!(guidance.contains("not migrated"));
        assert!(!guidance.to_ascii_lowercase().contains("reconnecting"));

        let mut onboarding = online();
        onboarding.submit_workspace("Demo").unwrap();
        onboarding.set_link(Link::ProtocolMismatch { daemon_protocol: 1 });
        assert!(!onboarding.database_actions_enabled());
        assert!(onboarding.workspace_feedback.as_ref().unwrap().is_error());
        assert_eq!(onboarding.banner(), guidance);
    }
}
