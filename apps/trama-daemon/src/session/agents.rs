//! Subagents as Trama threads, driven by the agent through the Trama MCP
//! tools (see [`crate::mcp`]).
//!
//! A tool call arrives with the session token of the calling agent; the
//! token maps to exactly one thread, which is always the parent of what the
//! call spawns and the sender of what it sends. Spawned threads are regular
//! subthreads (kind `sub`, or `review` for reviewers) with a `subagents`
//! record. Their task prompt is queued and started when the per-thread and
//! global limits allow; a thread blocked in a Trama tool call does not count
//! as running, so a parent waiting for its children never starves them.
//! When a worker's task completes, its changes are pulled into the parent
//! once the parent is idle or blocked in a Trama tool call (`waiting_to_sync`
//! until then).

use super::subthreads::{ForkConversation, ForkFiles, ForkRequest, review_prompt, system_message};
use super::*;
use crate::mcp::{self, McpSettings};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::PoisonError;
use tokio::sync::Notify;
use trama_domain::message::MessageAnnotation;
use trama_domain::subagent::{SpawnedBy, SubagentProfile, SubagentStatus};
use trama_domain::thread::ThreadKind;
use trama_infra::claude::transcript;
use trama_protocol::SubagentSnapshot;

const MAX_TEXT_CHARS: usize = 100_000;
const MAX_REPORT_CHARS: usize = 8_000;
const MAX_WAIT_IDS: usize = 32;
const MAX_LISTED_THREADS: usize = 200;
const DEFAULT_WAIT_SECS: u64 = 600;
/// Tools a scout never gets: it works in its parent's files.
const SCOUT_DISALLOWED: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// `config.json` `"subagents"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSettings {
    /// Subagents of one thread that may run at once; more are queued.
    #[serde(default = "default_per_thread")]
    pub max_running_per_thread: usize,
    /// Subagents that may run at once across the daemon.
    #[serde(default = "default_total")]
    pub max_running_total: usize,
    /// Keep Claude Code's built-in subagent tool (`Agent`/`Task`) next to
    /// Trama's. Off by default, so delegation creates Trama threads.
    #[serde(default)]
    pub allow_builtin_subagents: bool,
}

fn default_per_thread() -> usize {
    4
}

fn default_total() -> usize {
    8
}

impl Default for SubagentSettings {
    fn default() -> Self {
        Self {
            max_running_per_thread: default_per_thread(),
            max_running_total: default_total(),
            allow_builtin_subagents: false,
        }
    }
}

#[derive(Clone)]
pub(crate) struct AgentsConfig {
    pub(super) settings: SubagentSettings,
    /// `None`: sessions get no Trama MCP server (and keep the built-in tool).
    pub(super) mcp: Option<McpSettings>,
    /// Claude Code's `projects` directory (session transcripts).
    pub(super) claude_projects_dir: PathBuf,
    /// Trama's data directory: MCP configs and forked transcripts live here.
    pub(super) data_root: PathBuf,
}

impl AgentsConfig {
    pub(super) fn new(data_root: PathBuf) -> Self {
        Self {
            settings: SubagentSettings::default(),
            mcp: None,
            claude_projects_dir: transcript::claude_config_dir()
                .map(|dir| dir.join("projects"))
                .unwrap_or_else(|| data_root.join("claude-projects")),
            data_root,
        }
    }
}

/// Who a prompt is attributed to when another thread's agent sent it.
#[derive(Debug, Clone)]
pub(super) struct PromptOrigin {
    pub(super) actor: Actor,
    pub(super) annotation: MessageAnnotation,
}

/// How a turn ended, as seen by subagent bookkeeping.
pub(super) enum TurnEnd {
    Completed,
    Failed(String),
}

#[derive(Default)]
pub(crate) struct AgentsRuntime {
    state: Mutex<AgentsState>,
    changed: Notify,
}

#[derive(Default)]
struct AgentsState {
    /// MCP token -> thread of the live session that carries it.
    tokens: HashMap<String, ThreadId>,
    thread_tokens: HashMap<ThreadId, (String, PathBuf)>,
    tasks: HashMap<ThreadId, Task>,
    queue: VecDeque<ThreadId>,
    /// Threads blocked in a Trama tool call (count of calls).
    waiting: HashMap<ThreadId, usize>,
    mailbox: HashMap<ThreadId, VecDeque<Mail>>,
}

struct Task {
    parent: ThreadId,
    profile: SubagentProfile,
    title: String,
    status: SubagentStatus,
    /// The prompt of a queued task, until it starts.
    pending: Option<(String, PromptOrigin)>,
    /// A pull into the parent is in flight.
    syncing: bool,
}

struct Mail {
    text: String,
    origin: PromptOrigin,
}

/// Marks a thread as blocked in a Trama tool call while alive.
struct WaitingGuard {
    runtime: Arc<AgentsRuntime>,
    thread: ThreadId,
}

impl Drop for WaitingGuard {
    fn drop(&mut self) {
        let mut state = self
            .runtime
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = state.waiting.get_mut(&self.thread) {
            *count -= 1;
            if *count == 0 {
                state.waiting.remove(&self.thread);
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    title: String,
    prompt: String,
    #[serde(default)]
    profile: SubagentProfile,
    #[serde(default)]
    wait: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadArgs {
    thread_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    ids: Vec<String>,
    #[serde(default)]
    timeout: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    thread_id: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

impl SessionManager {
    /// Subagent limits and the built-in tool switch.
    pub fn with_subagent_settings(mut self, settings: SubagentSettings) -> Self {
        Arc::make_mut(&mut self.agents_config).settings = settings;
        self
    }

    /// Give new sessions the Trama MCP server (`trama-daemon mcp`).
    pub fn with_mcp(mut self, settings: McpSettings) -> Self {
        Arc::make_mut(&mut self.agents_config).mcp = Some(settings);
        self
    }

    /// Where Claude Code keeps session transcripts (default
    /// `~/.claude/projects`, or `$CLAUDE_CONFIG_DIR/projects`).
    pub fn with_claude_projects_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        Arc::make_mut(&mut self.agents_config).claude_projects_dir = dir.into();
        self
    }

    fn agents_state(&self) -> MutexGuard<'_, AgentsState> {
        self.agents
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn thread_in_turn(&self, thread_id: &ThreadId) -> bool {
        self.lock()
            .threads
            .get(thread_id)
            .is_some_and(|thread| thread.reservation == Some(Reservation::Turn))
    }

    fn live_session_id(&self, thread_id: &ThreadId) -> Option<AgentSessionId> {
        self.lock().threads.get(thread_id).and_then(|thread| {
            thread
                .live_session
                .as_ref()
                .map(|session| session.id.clone())
        })
    }

    fn agent_actor(&self, thread_id: &ThreadId) -> Actor {
        match self.live_session_id(thread_id) {
            Some(session) => Actor::Agent { session },
            None => Actor::System,
        }
    }

    /// Give a new session the Trama MCP server (merged with any MCP config
    /// it already has), allow its tools, disallow the built-in subagent tool
    /// unless configured otherwise, and explain the tools in the system
    /// prompt. Scouts also lose the file editing tools. A failure only logs:
    /// the session then runs without Trama's tools.
    pub(super) async fn attach_trama_tools(
        &self,
        thread_id: &ThreadId,
        config: &mut SessionConfig,
    ) {
        let scout = matches!(
            self.db.get_subagent(thread_id).await,
            Ok(Some(record)) if record.spawned_by.profile == SubagentProfile::Scout
        );
        if scout {
            config
                .disallowed_tools
                .extend(SCOUT_DISALLOWED.iter().map(|tool| tool.to_string()));
        }
        let Some(settings) = self.agents_config.mcp.clone() else {
            return;
        };
        let token = approval::generate_token();
        let user = config
            .mcp_config_path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let content = mcp::mcp_config(&settings, &token, user);
        let path = self
            .agents_config
            .data_root
            .join("mcp")
            .join(format!("{thread_id}.json"));
        let written = serde_json::to_string_pretty(&content)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                transcript::write_private(&path, &text).map_err(|error| error.to_string())
            });
        if let Err(error) = written {
            crate::log::warn(format!(
                "could not write the Trama MCP config for {thread_id}: {error}"
            ));
            return;
        }
        config.mcp_config_path = Some(path.clone());
        config.allowed_tools.push(mcp::ALLOW_RULE.to_owned());
        if !self.agents_config.settings.allow_builtin_subagents {
            config.disallowed_tools.extend(
                mcp::BUILTIN_SUBAGENT_TOOLS
                    .iter()
                    .map(|tool| tool.to_string()),
            );
        }
        config.append_system_prompt = Some(match config.append_system_prompt.take() {
            Some(existing) if !existing.trim().is_empty() => {
                format!("{existing}\n\n{}", mcp::SYSTEM_PROMPT)
            }
            _ => mcp::SYSTEM_PROMPT.to_owned(),
        });
        let mut state = self.agents_state();
        if let Some((old, _)) = state
            .thread_tokens
            .insert(thread_id.clone(), (token.clone(), path))
        {
            state.tokens.remove(&old);
        }
        state.tokens.insert(token, thread_id.clone());
    }

    /// Revoke the session's MCP token and remove its config file.
    pub(super) fn drop_trama_tools(&self, thread_id: &ThreadId) {
        let removed = {
            let mut state = self.agents_state();
            let removed = state.thread_tokens.remove(thread_id);
            if let Some((token, _)) = &removed {
                state.tokens.remove(token);
            }
            removed
        };
        if let Some((_, path)) = removed {
            let _ = std::fs::remove_file(path);
        }
    }

    /// The MCP token of `thread_id`'s live session.
    #[cfg(test)]
    pub(super) fn mcp_token(&self, thread_id: &ThreadId) -> Option<String> {
        self.agents_state()
            .thread_tokens
            .get(thread_id)
            .map(|(token, _)| token.clone())
    }

    /// The thread a live session token acts as, if any.
    pub(super) fn mcp_caller(&self, token: &str) -> Option<ThreadId> {
        self.agents_state().tokens.get(token).cloned()
    }

    /// Run one Trama MCP tool for the session holding `token`. Returns
    /// `(is_error, text)` for the agent. Mutations run in their own task, so
    /// a cancelled call (the agent was interrupted) never stops halfway.
    pub async fn mcp_tool_call(
        &self,
        token: &str,
        tool: &str,
        arguments: serde_json::Value,
        tool_use_id: Option<String>,
    ) -> (bool, String) {
        let Some(caller) = self.mcp_caller(token) else {
            return (
                true,
                "This TramaWorkspace token does not belong to a live session.".to_owned(),
            );
        };
        let _waiting = self.enter_tool(&caller).await;
        let result = match tool {
            mcp::TOOL_SPAWN => self.tool_spawn(&caller, arguments, tool_use_id).await,
            mcp::TOOL_STATUS => self.tool_status(&caller, arguments).await,
            mcp::TOOL_WAIT => self.tool_wait(&caller, arguments).await,
            mcp::TOOL_SEND => self.tool_send(&caller, arguments).await,
            mcp::TOOL_LIST => self.tool_list(&caller, arguments).await,
            other => Err(DaemonError::Invalid(format!("unknown tool `{other}`"))),
        };
        match result {
            Ok(text) => (false, text),
            Err(error) => (true, error.to_string()),
        }
    }

    /// The caller is blocked in a tool call: it stops counting as running,
    /// and its finished workers can merge into it now.
    async fn enter_tool(&self, caller: &ThreadId) -> WaitingGuard {
        let guard = {
            let mut state = self.agents_state();
            *state.waiting.entry(caller.clone()).or_default() += 1;
            WaitingGuard {
                runtime: Arc::clone(&self.agents),
                thread: caller.clone(),
            }
        };
        let manager = self.clone();
        let caller = caller.clone();
        let _ = tokio::spawn(async move {
            manager.sync_children(&caller).await;
            manager.drain_queue().await;
        })
        .await;
        guard
    }

    /// Workers of `parent` that finished and wait to merge into it.
    async fn sync_children(&self, parent: &ThreadId) {
        let children: Vec<ThreadId> = {
            let mut state = self.agents_state();
            state
                .tasks
                .iter_mut()
                .filter(|(_, task)| {
                    &task.parent == parent
                        && task.status == SubagentStatus::WaitingToSync
                        && !task.syncing
                })
                .map(|(id, task)| {
                    task.syncing = true;
                    id.clone()
                })
                .collect()
        };
        for child in children {
            self.sync_worker(&child).await;
        }
    }

    async fn tool_spawn(
        &self,
        caller: &ThreadId,
        arguments: serde_json::Value,
        tool_use_id: Option<String>,
    ) -> Result<String> {
        let args: SpawnArgs = parse_args(arguments)?;
        let title = validate_label(&args.title, "subagent title")?;
        let prompt = validate_text(&args.prompt, "prompt")?;
        let manager = self.clone();
        let parent = caller.clone();
        let profile = args.profile;
        let child = tokio::spawn(async move {
            manager
                .spawn_subagent(&parent, title, prompt, profile, tool_use_id)
                .await
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("spawn task failed: {error}")))??;
        if !args.wait {
            let status = self.task_status(&child).unwrap_or(SubagentStatus::Queued);
            return Ok(format!(
                "Spawned {} subagent thread {child} ({}). It runs on its own; call wait_for_threads with this id to collect its result, or thread_status to check on it.",
                profile.label(),
                status_label(status),
            ));
        }
        let done = self
            .wait_for(std::slice::from_ref(&child), mcp::MAX_WAIT)
            .await;
        self.describe_threads(std::slice::from_ref(&child), done)
            .await
    }

    /// Create the child thread of `parent_id` for `profile`, post a link row
    /// in the parent, and queue its task.
    pub(super) async fn spawn_subagent(
        &self,
        parent_id: &ThreadId,
        title: String,
        prompt: String,
        profile: SubagentProfile,
        tool_use_id: Option<String>,
    ) -> Result<ThreadId> {
        let parent = self
            .db
            .get_thread(parent_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {parent_id}")))?;
        let parent_title = parent.title.clone().unwrap_or_default();
        let (kind, files) = match profile {
            SubagentProfile::Worker => (ThreadKind::Sub, ForkFiles::Current),
            SubagentProfile::Scout => (ThreadKind::Sub, ForkFiles::Shared),
            SubagentProfile::Reviewer => (ThreadKind::Review, ForkFiles::Current),
        };
        let child = self
            .fork_child(
                &parent,
                ForkRequest {
                    kind,
                    title: title.clone(),
                    files,
                    conversation: ForkConversation::Fresh,
                    allow_parent_turn: true,
                    spawned_by: Some(SpawnedBy {
                        parent_thread_id: parent.id.clone(),
                        tool_use_id,
                        profile,
                    }),
                    forked_at: None,
                },
            )
            .await?;
        let child_id = child.thread.id.clone();
        let actor = self.agent_actor(&parent.id);
        let mut row = system_message(
            &parent.id,
            format!(
                "Spawned {} subagent \"{title}\" (thread {child_id}).",
                profile.label()
            ),
        );
        row.actor = actor.clone();
        row.annotation = Some(MessageAnnotation::SubagentSpawned {
            child_thread_id: child_id.clone(),
            title: title.clone(),
            profile,
        });
        self.append_annotated(row).await;
        let task_prompt = match profile {
            SubagentProfile::Worker => format!(
                "You are a worker subagent spawned by the agent of the TramaWorkspace thread \"{parent_title}\". You work in your own copy of that thread's checkouts (their current files, uncommitted changes included). When you finish, your changes are merged into the parent's checkouts automatically, so do not commit, push or open pull requests unless the task says so. End with a short summary of what you changed and anything left to do.\n\nTask:\n{prompt}"
            ),
            SubagentProfile::Scout => format!(
                "You are a scout subagent spawned by the agent of the TramaWorkspace thread \"{parent_title}\". You work directly in that thread's checkouts and must not modify files (the file editing tools are disabled). Investigate, then end with a concise report of your findings.\n\nTask:\n{prompt}"
            ),
            SubagentProfile::Reviewer => {
                review_prompt(&parent_title, &child.parent_checkouts, Some(&prompt))
            }
        };
        let origin = PromptOrigin {
            actor,
            annotation: MessageAnnotation::AgentMessageReceived {
                from_thread_id: parent.id.clone(),
                from_title: parent_title,
            },
        };
        {
            let mut state = self.agents_state();
            state.tasks.insert(
                child_id.clone(),
                Task {
                    parent: parent.id.clone(),
                    profile,
                    title,
                    status: SubagentStatus::Queued,
                    pending: Some((task_prompt, origin)),
                    syncing: false,
                },
            );
            state.queue.push_back(child_id.clone());
        }
        self.broadcast_status(&child_id, &parent.id, SubagentStatus::Queued, None);
        self.drain_queue().await;
        Ok(child_id)
    }

    /// Start queued tasks while the limits allow, oldest first.
    async fn drain_queue(&self) {
        let settings = self.agents_config.settings.clone();
        let starts: Vec<(ThreadId, String, PromptOrigin)> = {
            let mut state = self.agents_state();
            let mut started = Vec::new();
            let mut index = 0;
            while index < state.queue.len() {
                let running: Vec<&ThreadId> = state
                    .tasks
                    .iter()
                    .filter(|(id, task)| {
                        task.status == SubagentStatus::Running && !state.waiting.contains_key(*id)
                    })
                    .map(|(_, task)| &task.parent)
                    .collect();
                if running.len() >= settings.max_running_total.max(1) {
                    break;
                }
                let id = state.queue[index].clone();
                let Some(parent) = state.tasks.get(&id).map(|task| task.parent.clone()) else {
                    state.queue.remove(index);
                    continue;
                };
                let siblings = running.iter().filter(|p| ***p == parent).count();
                if siblings >= settings.max_running_per_thread.max(1) {
                    index += 1;
                    continue;
                }
                state.queue.remove(index);
                if let Some(task) = state.tasks.get_mut(&id) {
                    task.status = SubagentStatus::Running;
                    if let Some((prompt, origin)) = task.pending.take() {
                        started.push((id, prompt, origin));
                    }
                }
            }
            started
        };
        for (id, prompt, origin) in starts {
            self.persist_status(&id, SubagentStatus::Running, None)
                .await;
            if let Err(error) = self
                .start_turn_with(
                    id.clone(),
                    prompt,
                    false,
                    PromptContext::default(),
                    Some(origin),
                )
                .await
            {
                self.finish_task(
                    &id,
                    SubagentStatus::Failed,
                    Some(format!("its task could not start: {error}")),
                )
                .await;
            }
        }
    }

    /// Called whenever a turn of any thread ends (after its reservation is
    /// released). Runs on its own task so the event pump never blocks.
    pub(super) fn after_turn(&self, thread_id: &ThreadId, end: TurnEnd) {
        let manager = self.clone();
        let thread_id = thread_id.clone();
        tokio::spawn(async move { manager.handle_turn_end(thread_id, end).await });
    }

    async fn handle_turn_end(&self, thread_id: ThreadId, end: TurnEnd) {
        let task = {
            let state = self.agents_state();
            state
                .tasks
                .get(&thread_id)
                .filter(|task| task.status == SubagentStatus::Running && task.pending.is_none())
                .map(|task| (task.parent.clone(), task.profile))
        };
        if let Some((parent, profile)) = task {
            match end {
                TurnEnd::Failed(reason) => {
                    self.finish_task(&thread_id, SubagentStatus::Failed, Some(reason))
                        .await;
                }
                TurnEnd::Completed if profile == SubagentProfile::Worker => {
                    let parent_busy = self.thread_in_turn(&parent);
                    let sync_now = {
                        let mut state = self.agents_state();
                        let waiting = state.waiting.contains_key(&parent);
                        match state.tasks.get_mut(&thread_id) {
                            Some(task) if waiting || !parent_busy => {
                                task.syncing = true;
                                true
                            }
                            Some(task) => {
                                task.status = SubagentStatus::WaitingToSync;
                                false
                            }
                            None => false,
                        }
                    };
                    if sync_now {
                        self.sync_worker(&thread_id).await;
                    } else {
                        self.persist_status(
                            &thread_id,
                            SubagentStatus::WaitingToSync,
                            Some("finished; its changes are merged when the parent thread is idle or waiting".into()),
                        )
                        .await;
                    }
                }
                TurnEnd::Completed => {
                    self.finish_task(&thread_id, SubagentStatus::Completed, None)
                        .await;
                }
            }
        }
        // This thread is idle now: merge finished workers, then deliver mail.
        if !self.thread_in_turn(&thread_id) {
            self.sync_children(&thread_id).await;
        }
        self.deliver_mail(&thread_id).await;
        self.drain_queue().await;
        self.agents.changed.notify_waiters();
    }

    /// Pull a finished worker's changes into its parent and complete it.
    async fn sync_worker(&self, child: &ThreadId) {
        let Some((parent, title)) = self
            .agents_state()
            .tasks
            .get(child)
            .map(|task| (task.parent.clone(), task.title.clone()))
        else {
            return;
        };
        let into_running_parent = self.agents_state().waiting.contains_key(&parent);
        match self.pull_changes_with(child, into_running_parent).await {
            Ok((_, repos)) => {
                let conflicts: Vec<String> = repos
                    .iter()
                    .flat_map(|repo| repo.files.iter())
                    .filter(|file| {
                        matches!(
                            file.outcome,
                            trama_domain::subthread::PullFileOutcome::Conflict { .. }
                        )
                    })
                    .map(|file| file.path.clone())
                    .collect();
                let files: usize = repos.iter().map(|repo| repo.files.len()).sum();
                let detail = if conflicts.is_empty() {
                    format!("{files} changed files merged into the parent")
                } else {
                    format!(
                        "{files} changed files merged into the parent, with conflicts in: {}",
                        conflicts.join(", ")
                    )
                };
                self.finish_task(child, SubagentStatus::Completed, Some(detail))
                    .await;
            }
            Err(DaemonError::Busy) => {
                if let Some(task) = self.agents_state().tasks.get_mut(child) {
                    task.syncing = false;
                    task.status = SubagentStatus::WaitingToSync;
                }
                self.persist_status(
                    child,
                    SubagentStatus::WaitingToSync,
                    Some("the parent thread is busy; its changes are merged when it is idle or waiting".into()),
                )
                .await;
            }
            Err(error) => {
                let detail = format!(
                    "finished, but its changes could not be merged into the parent ({error}); use Pull Changes on \"{title}\""
                );
                self.finish_task(child, SubagentStatus::Completed, Some(detail))
                    .await;
            }
        }
    }

    /// End a task: persist, tell both threads, and post a row in the parent.
    async fn finish_task(&self, child: &ThreadId, status: SubagentStatus, detail: Option<String>) {
        let Some((parent, title)) = ({
            let mut state = self.agents_state();
            state.tasks.get_mut(child).map(|task| {
                task.status = status;
                task.syncing = false;
                task.pending = None;
                (task.parent.clone(), task.title.clone())
            })
        }) else {
            return;
        };
        self.persist_status(child, status, detail.clone()).await;
        let mut text = match status {
            SubagentStatus::Failed => format!("Subagent \"{title}\" failed"),
            _ => format!("Subagent \"{title}\" finished"),
        };
        if let Some(detail) = &detail {
            text.push_str(": ");
            text.push_str(detail);
        }
        text.push('.');
        let mut row = system_message(&parent, text);
        row.annotation = Some(MessageAnnotation::SubagentFinished {
            child_thread_id: child.clone(),
            title,
            status,
        });
        self.append_annotated(row).await;
        self.agents.changed.notify_waiters();
    }

    async fn persist_status(
        &self,
        child: &ThreadId,
        status: SubagentStatus,
        detail: Option<String>,
    ) {
        if let Err(error) = self
            .db
            .set_subagent_status(child, status, detail.clone())
            .await
        {
            crate::log::warn(format!("failed to persist subagent status: {error}"));
        }
        let parent = self
            .agents_state()
            .tasks
            .get(child)
            .map(|task| task.parent.clone());
        if let Some(parent) = parent {
            self.broadcast_status(child, &parent, status, detail);
        }
        self.agents.changed.notify_waiters();
    }

    fn broadcast_status(
        &self,
        child: &ThreadId,
        parent: &ThreadId,
        status: SubagentStatus,
        detail: Option<String>,
    ) {
        let message = DaemonMessage::SubagentStatusChanged {
            thread_id: child.clone(),
            parent_thread_id: parent.clone(),
            status,
            detail,
        };
        self.broadcast(parent, message.clone());
        self.broadcast(child, message);
    }

    /// Persist a message (row plus event) and show it to the subscribers.
    async fn append_annotated(&self, message: Message) {
        let thread_id = message.thread_id.clone();
        let event = match EventRecord::new(
            Aggregate::Thread {
                id: thread_id.clone(),
            },
            message.actor.clone(),
            DomainEvent::MessageAdded {
                message: message.clone(),
            },
            vec![],
        ) {
            Ok(event) => event,
            Err(error) => {
                crate::log::warn(format!("failed to encode message: {error}"));
                return;
            }
        };
        if let Err(error) = self.db.append_message_with_event(&message, &event).await {
            crate::log::warn(format!("failed to persist message: {error}"));
            return;
        }
        self.broadcast(
            &thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message,
            },
        );
    }

    fn task_status(&self, thread_id: &ThreadId) -> Option<SubagentStatus> {
        self.agents_state()
            .tasks
            .get(thread_id)
            .map(|task| task.status)
    }

    /// The subagent state a snapshot shows. A task the daemon no longer
    /// tracks (it restarted mid-task) is reported as failed.
    pub(super) async fn subagent_snapshot(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<SubagentSnapshot>> {
        let Some(record) = self.db.get_subagent(thread_id).await? else {
            return Ok(None);
        };
        let (status, detail) = match self.task_status(thread_id) {
            Some(status) => (status, record.detail),
            None if record.status.is_active() => (
                SubagentStatus::Failed,
                Some("interrupted: the daemon stopped before the task finished".into()),
            ),
            None => (record.status, record.detail),
        };
        Ok(Some(SubagentSnapshot {
            spawned_by: record.spawned_by,
            status,
            detail,
        }))
    }

    /// Every id is settled: its task is no longer queued, running or merging,
    /// and no turn is running on it.
    fn settled(&self, ids: &[ThreadId]) -> bool {
        let busy_tasks = {
            let state = self.agents_state();
            ids.iter().any(|id| {
                state
                    .tasks
                    .get(id)
                    .is_some_and(|task| task.status.is_active() || task.syncing)
            })
        };
        !busy_tasks && !ids.iter().any(|id| self.thread_in_turn(id))
    }

    /// Wait until `ids` are settled or `timeout` passes; true if settled.
    pub(super) async fn wait_for(&self, ids: &[ThreadId], timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.agents.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.settled(ids) {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.settled(ids);
            }
        }
    }

    /// The caller may only see and message threads of its own workspace.
    async fn visible_thread(&self, caller: &ThreadId, raw_id: &str) -> Result<Thread> {
        let id = ThreadId::from(raw_id.trim());
        let caller_thread = self
            .db
            .get_thread(caller)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {caller}")))?;
        match self.db.get_thread(&id).await? {
            Some(thread)
                if thread.workspace_id.is_some()
                    && thread.workspace_id == caller_thread.workspace_id =>
            {
                Ok(thread)
            }
            _ => Err(DaemonError::NotFound(format!(
                "thread {id} is not in this workspace"
            ))),
        }
    }

    async fn tool_status(&self, caller: &ThreadId, arguments: serde_json::Value) -> Result<String> {
        let args: ThreadArgs = parse_args(arguments)?;
        let thread = self.visible_thread(caller, &args.thread_id).await?;
        let settled = self.settled(std::slice::from_ref(&thread.id));
        self.describe_threads(std::slice::from_ref(&thread.id), settled)
            .await
    }

    async fn tool_wait(&self, caller: &ThreadId, arguments: serde_json::Value) -> Result<String> {
        let args: WaitArgs = parse_args(arguments)?;
        if args.ids.is_empty() || args.ids.len() > MAX_WAIT_IDS {
            return Err(DaemonError::Invalid(format!(
                "ids must list 1 to {MAX_WAIT_IDS} thread ids"
            )));
        }
        let mut ids = Vec::with_capacity(args.ids.len());
        for raw in &args.ids {
            let thread = self.visible_thread(caller, raw).await?;
            if &thread.id == caller {
                return Err(DaemonError::Invalid(
                    "a thread cannot wait for itself".into(),
                ));
            }
            if !ids.contains(&thread.id) {
                ids.push(thread.id);
            }
        }
        let timeout = Duration::from_secs(
            args.timeout
                .unwrap_or(DEFAULT_WAIT_SECS)
                .clamp(1, mcp::MAX_WAIT.as_secs()),
        );
        let done = self.wait_for(&ids, timeout).await;
        self.describe_threads(&ids, done).await
    }

    async fn tool_send(&self, caller: &ThreadId, arguments: serde_json::Value) -> Result<String> {
        let args: SendArgs = parse_args(arguments)?;
        let target = self.visible_thread(caller, &args.thread_id).await?;
        if &target.id == caller {
            return Err(DaemonError::Invalid(
                "send_to_thread needs another thread; this is your own".into(),
            ));
        }
        // A shared thread's turns go through the collab service's log (ADR
        // 0008); an agent message would run outside it, so it is refused.
        if self.db.collab_link(&target.id).await?.is_some() {
            return Err(DaemonError::Invalid(format!(
                "thread {} is shared with other people; agents cannot message shared threads, \
                 so ask the user to relay it",
                target.id
            )));
        }
        let text = validate_text(&args.message, "message")?;
        let manager = self.clone();
        let caller = caller.clone();
        tokio::spawn(async move { manager.send_agent_message(&caller, &target, text).await })
            .await
            .map_err(|error| DaemonError::Invalid(format!("send task failed: {error}")))?
    }

    /// Deliver `text` from `from`'s agent to `target`'s agent (now, or when
    /// its current turn ends) and record it in both transcripts.
    pub(super) async fn send_agent_message(
        &self,
        from: &ThreadId,
        target: &Thread,
        text: String,
    ) -> Result<String> {
        let sender = self
            .db
            .get_thread(from)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {from}")))?;
        let sender_title = sender.title.clone().unwrap_or_default();
        let target_title = target.title.clone().unwrap_or_default();
        let actor = self.agent_actor(from);
        let prompt = format!(
            "Message from the agent of thread \"{sender_title}\" (thread id {from}). Answer with send_to_thread to that id if it needs a reply.\n\n{text}"
        );
        let origin = PromptOrigin {
            actor: actor.clone(),
            annotation: MessageAnnotation::AgentMessageReceived {
                from_thread_id: from.clone(),
                from_title: sender_title,
            },
        };
        let outcome = match self
            .start_turn_with(
                target.id.clone(),
                prompt.clone(),
                false,
                PromptContext::default(),
                Some(origin.clone()),
            )
            .await
        {
            Ok(_) => {
                self.task_restarted(&target.id).await;
                format!(
                    "Delivered to thread \"{target_title}\" ({}); its agent is working on it. Use wait_for_threads to wait for it.",
                    target.id
                )
            }
            Err(DaemonError::Busy) => {
                self.agents_state()
                    .mailbox
                    .entry(target.id.clone())
                    .or_default()
                    .push_back(Mail {
                        text: prompt,
                        origin,
                    });
                if !self.thread_in_turn(&target.id) {
                    // Busy with something other than a turn (a copy or a
                    // migration): no turn end will deliver it, so retry.
                    self.retry_mail(target.id.clone());
                }
                format!(
                    "Thread \"{target_title}\" ({}) is busy; the message is delivered when its current turn ends.",
                    target.id
                )
            }
            Err(error) => return Err(error),
        };
        let mut sent = system_message(from, text);
        sent.actor = actor;
        sent.annotation = Some(MessageAnnotation::AgentMessageSent {
            to_thread_id: target.id.clone(),
            to_title: target_title,
        });
        let event = EventRecord::new(
            Aggregate::Thread { id: from.clone() },
            sent.actor.clone(),
            DomainEvent::AgentMessageSent {
                from_thread_id: from.clone(),
                to_thread_id: target.id.clone(),
                message_id: sent.id.clone(),
            },
            vec![],
        )?;
        self.db.append_message_with_event(&sent, &event).await?;
        self.broadcast(
            from,
            DaemonMessage::MessageAppended {
                thread_id: from.clone(),
                message: sent,
            },
        );
        Ok(outcome)
    }

    /// A Trama-started turn began on a subagent thread: it has a task again.
    async fn task_restarted(&self, thread_id: &ThreadId) {
        let known = self.agents_state().tasks.contains_key(thread_id);
        if !known {
            let Ok(Some(record)) = self.db.get_subagent(thread_id).await else {
                return;
            };
            let title = self
                .db
                .get_thread(thread_id)
                .await
                .ok()
                .flatten()
                .and_then(|thread| thread.title)
                .unwrap_or_default();
            self.agents_state()
                .tasks
                .entry(thread_id.clone())
                .or_insert(Task {
                    parent: record.spawned_by.parent_thread_id,
                    profile: record.spawned_by.profile,
                    title,
                    status: SubagentStatus::Running,
                    pending: None,
                    syncing: false,
                });
        }
        let changed = {
            let mut state = self.agents_state();
            match state.tasks.get_mut(thread_id) {
                Some(task) if task.status != SubagentStatus::Running => {
                    task.status = SubagentStatus::Running;
                    task.syncing = false;
                    true
                }
                _ => false,
            }
        };
        if changed || !known {
            self.persist_status(thread_id, SubagentStatus::Running, None)
                .await;
        }
    }

    /// Start the oldest queued agent message for an idle thread.
    async fn deliver_mail(&self, thread_id: &ThreadId) {
        let Some(mail) = self
            .agents_state()
            .mailbox
            .get_mut(thread_id)
            .and_then(VecDeque::pop_front)
        else {
            return;
        };
        match self
            .start_turn_with(
                thread_id.clone(),
                mail.text.clone(),
                false,
                PromptContext::default(),
                Some(mail.origin.clone()),
            )
            .await
        {
            Ok(_) => self.task_restarted(thread_id).await,
            Err(DaemonError::Busy) => {
                self.agents_state()
                    .mailbox
                    .entry(thread_id.clone())
                    .or_default()
                    .push_front(mail);
            }
            Err(error) => {
                self.append_system_notice(
                    thread_id,
                    format!(
                        "A message from another thread's agent could not be delivered: {error}"
                    ),
                )
                .await;
            }
        }
    }

    fn retry_mail(&self, thread_id: ThreadId) {
        let manager = self.clone();
        tokio::spawn(async move {
            for _ in 0..600 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let pending = manager
                    .agents_state()
                    .mailbox
                    .get(&thread_id)
                    .is_some_and(|mail| !mail.is_empty());
                if !pending || manager.thread_in_turn(&thread_id) {
                    return;
                }
                manager.deliver_mail(&thread_id).await;
            }
        });
    }

    async fn tool_list(&self, caller: &ThreadId, arguments: serde_json::Value) -> Result<String> {
        let _: NoArgs = parse_args(arguments)?;
        let caller_thread = self
            .db
            .get_thread(caller)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {caller}")))?;
        let Some(workspace) = caller_thread.workspace_id else {
            return Ok("This thread belongs to no workspace.".into());
        };
        let threads = self.db.list_threads(&workspace).await?;
        let total = threads.len();
        let snapshots = self
            .thread_snapshots(&threads[..total.min(MAX_LISTED_THREADS)])
            .await?;
        let mut out = format!("{total} threads in this workspace:\n");
        for snapshot in snapshots {
            out.push_str(&format!(
                "{}- \"{}\" id={} kind={} depth={}",
                "  ".repeat(snapshot.depth.min(16) as usize),
                snapshot.title,
                snapshot.id,
                kind_label(snapshot.kind),
                snapshot.depth
            ));
            if let Some(parent) = &snapshot.parent_thread_id {
                out.push_str(&format!(" parent={parent}"));
            }
            if let Some(subagent) = &snapshot.subagent {
                out.push_str(&format!(
                    " {} status={}",
                    subagent.spawned_by.profile.label(),
                    status_label(subagent.status)
                ));
            }
            if self.thread_in_turn(&snapshot.id) {
                out.push_str(" (turn running)");
            }
            if &snapshot.id == caller {
                out.push_str(" (you)");
            }
            out.push('\n');
        }
        if total > MAX_LISTED_THREADS {
            out.push_str(&format!("... and {} more\n", total - MAX_LISTED_THREADS));
        }
        Ok(out)
    }

    /// Status and latest final message of each thread, for the agent.
    async fn describe_threads(&self, ids: &[ThreadId], settled: bool) -> Result<String> {
        let mut out = String::new();
        if !settled {
            out.push_str("Timed out before every thread finished; call wait_for_threads again to keep waiting.\n\n");
        }
        for id in ids {
            let Some(thread) = self.db.get_thread(id).await? else {
                continue;
            };
            let snapshot = self.subagent_snapshot(id).await?;
            let status = match &snapshot {
                Some(subagent) => status_label(subagent.status).to_owned(),
                None if self.thread_in_turn(id) => "running".to_owned(),
                None => "idle".to_owned(),
            };
            out.push_str(&format!(
                "Thread \"{}\" ({id}): {status}",
                thread.title.clone().unwrap_or_default()
            ));
            if let Some(detail) = snapshot.as_ref().and_then(|s| s.detail.as_ref()) {
                out.push_str(&format!(" ({detail})"));
            }
            if self.thread_in_turn(id) && snapshot.is_some() {
                out.push_str(", a turn is running");
            }
            out.push('\n');
            let last = self
                .db
                .list_messages_by_thread(id)
                .await?
                .into_iter()
                .rev()
                .find(|message| message.role == Role::Assistant);
            match last {
                Some(message) => {
                    out.push_str("Final message:\n");
                    out.push_str(&truncate_chars(&message.text(), MAX_REPORT_CHARS));
                    out.push_str("\n\n");
                }
                None => out.push_str("No final message yet.\n\n"),
            }
        }
        Ok(out.trim_end().to_owned())
    }
}

fn parse_args<T: DeserializeOwned>(arguments: serde_json::Value) -> Result<T> {
    let arguments = if arguments.is_null() {
        serde_json::json!({})
    } else {
        arguments
    };
    serde_json::from_value(arguments)
        .map_err(|error| DaemonError::Invalid(format!("invalid arguments: {error}")))
}

fn validate_text(value: &str, what: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(DaemonError::Invalid(format!("{what} must not be empty")));
    }
    if value.chars().count() > MAX_TEXT_CHARS {
        return Err(DaemonError::Invalid(format!(
            "{what} must be at most {MAX_TEXT_CHARS} characters"
        )));
    }
    Ok(value.to_owned())
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((index, _)) => format!("{}\n[... truncated]", &text[..index]),
        None => text.to_owned(),
    }
}

fn status_label(status: SubagentStatus) -> &'static str {
    match status {
        SubagentStatus::Queued => "queued",
        SubagentStatus::Running => "running",
        SubagentStatus::WaitingToSync => "waiting to sync",
        SubagentStatus::Completed => "completed",
        SubagentStatus::Failed => "failed",
    }
}

fn kind_label(kind: ThreadKind) -> &'static str {
    match kind {
        ThreadKind::Top => "top",
        ThreadKind::Sub => "sub",
        ThreadKind::Review => "review",
        ThreadKind::Fork => "fork",
        ThreadKind::Land => "land",
    }
}
