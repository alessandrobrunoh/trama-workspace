//! Composer backend: `@` mentionables and prompt expansion, `/` skills, and
//! per-thread model selection. Terminals live in [`super::terminal`].

use super::SessionManager;
use super::comments::LOCAL_USER;
use super::skills::{self, BUILTIN_COMMANDS, SkillRoots};
use super::terminal::Terminals;
use crate::error::{DaemonError, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use trama_db::AgentCommands;
use trama_domain::agent::AgentSession;
use trama_domain::ids::{AgentSessionId, RepositoryId, ThreadId, WorkspaceId};
use trama_domain::message::Role;
use trama_domain::thread::{Thread, ThreadKind};
use trama_protocol::{
    CheckoutStatus, DaemonMessage, MentionKind, Mentionable, ModelInfo, PromptMention, SkillInfo,
    SkillKind, SkillSource,
};

/// Models offered by `ListModels`. `default` clears the thread override.
pub(crate) const MODEL_CHOICES: &[(&str, &str)] = &[
    ("default", "Default"),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
    ("claude-opus-5-5", "Claude Opus 5.5"),
    ("claude-haiku-5-5", "Claude Haiku 5.5"),
    ("claude-fable-5-1", "Claude Fable 5.1"),
];

const DEFAULT_MENTION_LIMIT: usize = 30;
const MAX_MENTION_LIMIT: usize = 200;
const MAX_QUERY_CHARS: usize = 200;
/// Files listed per repository for `@` completion.
const MAX_INDEXED_FILES: usize = 20_000;
/// A thread's file index is reused this long between keystrokes.
const INDEX_TTL: Duration = Duration::from_secs(5);
const MAX_PROMPT_MENTIONS: usize = 50;
const MAX_PROMPT_TERMINALS: usize = 8;
const THREAD_EXCERPT_CHARS: usize = 600;

/// Someone who can be `@`-mentioned besides the local user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub id: String,
    pub name: String,
    pub detail: Option<String>,
}

/// Where people come from. The collaboration service will provide one; until
/// then nobody but the local user is listed.
pub trait PeopleSource: Send + Sync {
    fn people(&self, workspace_id: Option<&WorkspaceId>) -> Vec<Person>;
}

/// No collaborators: an empty list, never placeholder people.
pub struct NoCollaborators;

impl PeopleSource for NoCollaborators {
    fn people(&self, _workspace_id: Option<&WorkspaceId>) -> Vec<Person> {
        Vec::new()
    }
}

/// A validated, ready checkout of a thread (canonical path).
#[derive(Debug, Clone)]
pub(super) struct ReadyCheckout {
    pub repository_id: RepositoryId,
    pub title: String,
    pub path: PathBuf,
}

/// One indexed path of a checkout (`rel` uses `/`, no trailing slash).
struct IndexedPath {
    kind: MentionKind,
    checkout: usize,
    rel: String,
}

struct FileIndex {
    built: Instant,
    checkouts: Vec<ReadyCheckout>,
    paths: Arc<Vec<IndexedPath>>,
}

/// Composer state shared by every clone of a [`SessionManager`].
pub(crate) struct ComposerState {
    pub(super) terminals: Terminals,
    file_index: Mutex<HashMap<ThreadId, Arc<FileIndex>>>,
    /// Model each live session was started with, to restart it when the
    /// thread's model changes.
    live_models: Mutex<HashMap<ThreadId, Option<String>>>,
    /// Sessions shut down on purpose; their closing event pump is not an
    /// unexpected end.
    retired_sessions: Mutex<HashSet<AgentSessionId>>,
    people: Arc<dyn PeopleSource>,
    /// Home directory for user and plugin skills (`None`: skip them).
    home: Option<PathBuf>,
    local_name: OnceLock<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ComposerState {
    pub(crate) fn new() -> Self {
        Self::with(
            std::env::var_os("HOME").map(PathBuf::from),
            Arc::new(NoCollaborators),
        )
    }

    fn with(home: Option<PathBuf>, people: Arc<dyn PeopleSource>) -> Self {
        Self {
            terminals: Terminals::default(),
            file_index: Mutex::new(HashMap::new()),
            live_models: Mutex::new(HashMap::new()),
            retired_sessions: Mutex::new(HashSet::new()),
            people,
            home,
            local_name: OnceLock::new(),
        }
    }

    /// `true` once for a session retired by [`SessionManager::retire_live_session`].
    pub(super) fn take_retired(&self, session: &AgentSessionId) -> bool {
        lock(&self.retired_sessions).remove(session)
    }

    pub(super) fn note_live_model(&self, thread_id: &ThreadId, model: Option<String>) {
        lock(&self.live_models).insert(thread_id.clone(), model);
    }
}

/// Mentions and terminals sent with a prompt.
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    pub mentions: Vec<PromptMention>,
    pub terminals: Vec<String>,
}

impl PromptContext {
    pub fn is_empty(&self) -> bool {
        self.mentions.is_empty() && self.terminals.is_empty()
    }
}

/// A Claude Code slash command or skill invocation (`/name ...`).
pub(super) fn is_slash_command(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('/') else {
        return false;
    };
    let name = rest.split_whitespace().next().unwrap_or("");
    skills::valid_name(name)
}

/// Put the context around the user's text. A slash command must stay first
/// for Claude Code to run it, so the context follows it there.
pub(super) fn compose_with_context(text: &str, context: &str) -> String {
    if context.is_empty() {
        text.to_owned()
    } else if text.trim().is_empty() {
        context.to_owned()
    } else if is_slash_command(text) {
        format!("{text}\n\n{context}")
    } else {
        format!("{context}\n\n{text}")
    }
}

/// Validate and normalize a model id; `None`, `""` and `"default"` clear
/// the override.
pub(super) fn normalize_model(model: Option<String>) -> Result<Option<String>> {
    let Some(model) = model.map(|m| m.trim().to_owned()) else {
        return Ok(None);
    };
    if model.is_empty() || model == "default" {
        return Ok(None);
    }
    let valid = model.len() <= 100
        && model.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '[' | ']' | ':' | '@')
        });
    if !valid {
        return Err(DaemonError::Invalid(format!("invalid model id {model:?}")));
    }
    Ok(Some(model))
}

pub(crate) fn model_choices() -> Vec<ModelInfo> {
    MODEL_CHOICES
        .iter()
        .map(|(id, label)| ModelInfo {
            id: (*id).into(),
            label: (*label).into(),
        })
        .collect()
}

/// Fuzzy match score of `query` in `candidate` (higher is better), `None`
/// when not every query character appears in order. Rewards word-boundary
/// and consecutive matches and matches in the last path segment.
pub(super) fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(fold)
        .collect();
    if query.is_empty() {
        return Some(0);
    }
    let original: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = original.iter().copied().map(fold).collect();
    let mut score = 0i64;
    let mut next = 0usize;
    let mut previous: Option<usize> = None;
    for (index, c) in lower.iter().enumerate() {
        if next < query.len() && *c == query[next] {
            score += 1;
            if index > 0 && previous == Some(index - 1) {
                score += 5;
            }
            let boundary = index == 0
                || matches!(original[index - 1], '/' | '_' | '-' | '.' | ' ')
                || (original[index].is_uppercase() && original[index - 1].is_lowercase());
            if boundary {
                score += 8;
            }
            previous = Some(index);
            next += 1;
        }
    }
    if next < query.len() {
        return None;
    }
    let query: String = query.into_iter().collect();
    let lower: String = lower.into_iter().collect();
    if lower.contains(&query) {
        score += 15;
    }
    let base = lower.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if base == query {
        score += 60;
    } else if base.split('.').next() == Some(query.as_str()) {
        score += 45;
    } else if base.starts_with(&query) {
        score += 30;
    } else if base.contains(&query) {
        score += 12;
    }
    score -= original.len() as i64 / 8;
    Some(score)
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Files (tracked plus untracked non-ignored) of one checkout, bounded.
fn list_checkout_files(path: &Path) -> Vec<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut files: BTreeSet<String> = BTreeSet::new();
    for raw in output.stdout.split(|b| *b == 0) {
        if files.len() >= MAX_INDEXED_FILES {
            break;
        }
        if raw.is_empty() {
            continue;
        }
        if let Ok(rel) = std::str::from_utf8(raw) {
            files.insert(rel.to_owned());
        }
    }
    files.into_iter().collect()
}

fn build_index(checkouts: &[ReadyCheckout]) -> Vec<IndexedPath> {
    let mut out = Vec::new();
    for (checkout, ready) in checkouts.iter().enumerate() {
        let files = list_checkout_files(&ready.path);
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for file in &files {
            let mut end = 0;
            while let Some(at) = file[end..].find('/') {
                end += at;
                dirs.insert(file[..end].to_owned());
                end += 1;
            }
        }
        out.extend(dirs.into_iter().map(|rel| IndexedPath {
            kind: MentionKind::Directory,
            checkout,
            rel,
        }));
        out.extend(files.into_iter().map(|rel| IndexedPath {
            kind: MentionKind::File,
            checkout,
            rel,
        }));
    }
    out
}

/// `rel` inside `checkout`, resolved; refuses absolute paths, `..`, and
/// anything (including symlinks) that resolves outside the checkout.
fn confine(checkout: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(DaemonError::Invalid(format!(
            "mentioned path {rel:?} is not inside the thread's checkouts"
        )));
    }
    let real = dunce::canonicalize(checkout.join(rel_path))
        .map_err(|_| DaemonError::Invalid(format!("mentioned path {rel:?} does not exist")))?;
    if !real.starts_with(checkout) {
        return Err(DaemonError::Invalid(format!(
            "mentioned path {rel:?} resolves outside the thread's checkouts"
        )));
    }
    Ok(real)
}

fn kind_label(kind: ThreadKind) -> &'static str {
    match kind {
        ThreadKind::Top => "thread",
        ThreadKind::Sub => "subthread",
        ThreadKind::Review => "review subthread",
        ThreadKind::Land => "land subthread",
        ThreadKind::Fork => "fork",
    }
}

fn thread_title(thread: &Thread) -> String {
    thread
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "Untitled thread".into())
}

fn excerpt(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max_chars).collect();
    cut.push('…');
    cut
}

impl SessionManager {
    /// Replace where `@` people come from (the collaboration integration).
    #[allow(dead_code)]
    pub fn with_people_source(mut self, people: Arc<dyn PeopleSource>) -> Self {
        let home = self.composer.home.clone();
        self.composer = Arc::new(ComposerState::with(home, people));
        self
    }

    /// Home directory searched for user and plugin skills (`None`: skip).
    #[allow(dead_code)]
    pub fn with_skill_home(mut self, home: Option<PathBuf>) -> Self {
        let people = Arc::clone(&self.composer.people);
        self.composer = Arc::new(ComposerState::with(home, people));
        self
    }

    /// The thread's validated isolated checkouts. Source repositories and
    /// checkouts that are not ready are never returned.
    pub(super) async fn ready_checkouts(&self, thread_id: &ThreadId) -> Result<Vec<ReadyCheckout>> {
        let rows = self.thread_checkouts(thread_id).await?;
        Ok(rows
            .into_iter()
            .filter(|row| row.status == CheckoutStatus::Ready)
            .filter_map(|row| {
                let path = dunce::canonicalize(row.checkout_path?).ok()?;
                Some(ReadyCheckout {
                    repository_id: row.repository_id,
                    title: row.title,
                    path,
                })
            })
            .collect())
    }

    async fn file_index(&self, thread_id: &ThreadId) -> Result<Arc<FileIndex>> {
        if let Some(index) = lock(&self.composer.file_index).get(thread_id) {
            if index.built.elapsed() < INDEX_TTL {
                return Ok(Arc::clone(index));
            }
        }
        let checkouts = self.ready_checkouts(thread_id).await?;
        let scan = checkouts.clone();
        let paths = tokio::task::spawn_blocking(move || build_index(&scan))
            .await
            .map_err(|error| DaemonError::Invalid(format!("file listing failed: {error}")))?;
        let index = Arc::new(FileIndex {
            built: Instant::now(),
            checkouts,
            paths: Arc::new(paths),
        });
        lock(&self.composer.file_index).insert(thread_id.clone(), Arc::clone(&index));
        Ok(index)
    }

    /// The local user's display name: Git `user.name`, else `$USER`.
    fn local_user_name(&self, checkout: Option<&Path>) -> String {
        self.composer
            .local_name
            .get_or_init(|| {
                let mut git = std::process::Command::new("git");
                if let Some(checkout) = checkout {
                    git.arg("-C").arg(checkout);
                }
                git.args(["config", "user.name"])
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .or_else(|| std::env::var("USER").ok().filter(|u| !u.is_empty()))
                    .unwrap_or_else(|| "You".into())
            })
            .clone()
    }

    /// `@` completion for `thread_id`, best match first.
    pub async fn list_mentionables(
        &self,
        thread_id: &ThreadId,
        query: &str,
        limit: Option<u32>,
    ) -> Result<Vec<Mentionable>> {
        let thread = self
            .db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let query: String = query.trim().chars().take(MAX_QUERY_CHARS).collect();
        let limit = limit
            .map(|limit| limit as usize)
            .unwrap_or(DEFAULT_MENTION_LIMIT)
            .clamp(1, MAX_MENTION_LIMIT);
        let empty = query.is_empty();
        let mut scored: Vec<(i64, Mentionable)> = Vec::new();

        let index = self.file_index(thread_id).await?;
        let multi = index.checkouts.len() > 1;
        for path in index.paths.iter() {
            let checkout = &index.checkouts[path.checkout];
            let mut label = if multi {
                format!("{}/{}", checkout.title, path.rel)
            } else {
                path.rel.clone()
            };
            if path.kind == MentionKind::Directory {
                label.push('/');
            }
            let score = if empty {
                let depth = path.rel.matches('/').count() as i64;
                -20 * depth - label.len() as i64 / 8
            } else {
                match fuzzy_score(&query, &label) {
                    Some(score) => score,
                    None => continue,
                }
            };
            scored.push((
                score,
                Mentionable {
                    kind: path.kind,
                    label,
                    detail: Some(checkout.title.clone()),
                    value: format!("{}:{}", checkout.repository_id, path.rel),
                },
            ));
        }

        if let Some(workspace) = &thread.workspace_id {
            let threads = self.db.list_threads(workspace).await?;
            let titles: HashMap<ThreadId, String> = threads
                .iter()
                .map(|t| (t.id.clone(), thread_title(t)))
                .collect();
            for (age, other) in threads.iter().rev().enumerate() {
                if other.id == thread.id {
                    continue;
                }
                let label = thread_title(other);
                let score = if empty {
                    -10 - age as i64
                } else {
                    match fuzzy_score(&query, &label) {
                        Some(score) => score,
                        None => continue,
                    }
                };
                let detail = match other.parent_thread_id.as_ref().and_then(|p| titles.get(p)) {
                    Some(parent) => format!("{} of {parent}", kind_label(other.kind)),
                    None => kind_label(other.kind).to_owned(),
                };
                scored.push((
                    score,
                    Mentionable {
                        kind: MentionKind::Thread,
                        label,
                        detail: Some(detail),
                        value: other.id.to_string(),
                    },
                ));
            }
        }

        let first_checkout = index.checkouts.first().map(|c| c.path.clone());
        let mut people = vec![Person {
            id: LOCAL_USER.into(),
            name: self.local_user_name(first_checkout.as_deref()),
            detail: Some("you".into()),
        }];
        people.extend(self.composer.people.people(thread.workspace_id.as_ref()));
        for person in people {
            let score = if empty {
                -15
            } else {
                match fuzzy_score(&query, &person.name) {
                    Some(score) => score,
                    None => continue,
                }
            };
            scored.push((
                score,
                Mentionable {
                    kind: MentionKind::Person,
                    label: person.name,
                    detail: person.detail,
                    value: person.id,
                },
            ));
        }

        scored.sort_by(|(a, left), (b, right)| {
            b.cmp(a)
                .then(left.label.len().cmp(&right.label.len()))
                .then_with(|| left.label.cmp(&right.label))
        });
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(_, item)| item).collect())
    }

    /// The context block for `ctx`: a `Referenced:` list (absolute checkout
    /// paths, threads, people) and terminal snapshots. Empty when `ctx` is.
    /// Fails closed on anything that is not part of this thread.
    pub(super) async fn prompt_context(
        &self,
        thread_id: &ThreadId,
        ctx: &PromptContext,
    ) -> Result<String> {
        if ctx.is_empty() {
            return Ok(String::new());
        }
        if ctx.mentions.len() > MAX_PROMPT_MENTIONS || ctx.terminals.len() > MAX_PROMPT_TERMINALS {
            return Err(DaemonError::Invalid(format!(
                "a prompt can carry at most {MAX_PROMPT_MENTIONS} mentions and {MAX_PROMPT_TERMINALS} terminals"
            )));
        }
        let thread = self
            .db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let needs_paths = ctx
            .mentions
            .iter()
            .any(|m| matches!(m.kind, MentionKind::File | MentionKind::Directory));
        let checkouts = if needs_paths {
            self.ready_checkouts(thread_id).await?
        } else {
            Vec::new()
        };
        let mut lines = Vec::new();
        let mut seen = HashSet::new();
        for mention in &ctx.mentions {
            if !seen.insert((mention.kind, mention.value.clone())) {
                continue;
            }
            match mention.kind {
                MentionKind::File | MentionKind::Directory => {
                    let (repository, rel) = mention.value.split_once(':').ok_or_else(|| {
                        DaemonError::Invalid(format!("invalid path mention {:?}", mention.value))
                    })?;
                    let checkout = checkouts
                        .iter()
                        .find(|c| c.repository_id.to_string() == repository)
                        .ok_or_else(|| {
                            DaemonError::Invalid(format!(
                                "mentioned repository {repository} is not a ready checkout of this thread"
                            ))
                        })?;
                    let path = confine(&checkout.path, rel)?;
                    if path.is_dir() {
                        lines.push(format!("- directory: {}/", path.display()));
                    } else {
                        lines.push(format!("- file: {}", path.display()));
                    }
                }
                MentionKind::Thread => {
                    lines.push(self.thread_reference(&thread, &mention.value).await?);
                }
                MentionKind::Person => {
                    let name = if mention.value == LOCAL_USER {
                        let first = checkouts.first().map(|c| c.path.clone());
                        format!(
                            "{} (the user writing this prompt)",
                            self.local_user_name(first.as_deref())
                        )
                    } else {
                        self.composer
                            .people
                            .people(thread.workspace_id.as_ref())
                            .into_iter()
                            .find(|person| person.id == mention.value)
                            .map(|person| person.name)
                            .ok_or_else(|| {
                                DaemonError::Invalid(format!("unknown person {:?}", mention.value))
                            })?
                    };
                    lines.push(format!("- person: {name}"));
                }
            }
        }
        let mut blocks = Vec::new();
        if !lines.is_empty() {
            blocks.push(format!("Referenced:\n{}", lines.join("\n")));
        }
        let mut seen_terminals = HashSet::new();
        for terminal in &ctx.terminals {
            if seen_terminals.insert(terminal.as_str()) {
                blocks.push(self.terminal_context(thread_id, terminal)?);
            }
        }
        Ok(blocks.join("\n\n"))
    }

    async fn thread_reference(&self, current: &Thread, value: &str) -> Result<String> {
        let id = ThreadId::from(value);
        let other = self
            .db
            .get_thread(&id)
            .await?
            .filter(|other| {
                other.workspace_id.is_some() && other.workspace_id == current.workspace_id
            })
            .ok_or_else(|| {
                DaemonError::Invalid(format!("thread {value} is not in this workspace"))
            })?;
        let mut line = format!(
            "- thread \"{}\" (Trama thread {}",
            thread_title(&other),
            other.id
        );
        match &other.parent_thread_id {
            Some(parent) => {
                let parent_title = self
                    .db
                    .get_thread(parent)
                    .await?
                    .map(|p| thread_title(&p))
                    .unwrap_or_else(|| parent.to_string());
                line.push_str(&format!(
                    ", {} of \"{parent_title}\")",
                    kind_label(other.kind)
                ));
            }
            None => line.push(')'),
        }
        let last = self
            .db
            .list_messages_by_thread(&other.id)
            .await?
            .into_iter()
            .rev()
            .find(|message| message.role == Role::Assistant && !message.text().trim().is_empty());
        match last {
            Some(message) => {
                line.push_str(". Its latest reply:\n");
                for text in excerpt(&message.text(), THREAD_EXCERPT_CHARS).lines() {
                    line.push_str("  > ");
                    line.push_str(text);
                    line.push('\n');
                }
                line.push_str("  ");
            }
            None => line.push_str(". It has no reply yet. "),
        }
        if self.agents_config.mcp.is_some() {
            // The TramaWorkspace MCP tools reach any thread of the workspace.
            line.push_str(&format!(
                "It is a separate Trama thread with its own session and checkouts; use the \
                 TramaWorkspace tools thread_status or send_to_thread with thread id {} to \
                 read or message it.",
                other.id
            ));
        } else {
            line.push_str(
                "It is a separate Trama thread with its own session and checkouts; you cannot \
                 read or message it from here, so ask the user to relay anything you need from it.",
            );
        }
        Ok(line)
    }

    /// Thread override, else the nearest ancestor's (a subthread, nested at
    /// any depth, follows its parent).
    pub(super) async fn thread_model_override(&self, thread: &Thread) -> Result<Option<String>> {
        if let Some(model) = self.db.thread_model(&thread.id).await? {
            return Ok(Some(model));
        }
        let Some(parent) = &thread.parent_thread_id else {
            return Ok(None);
        };
        if let Some(model) = self.db.thread_model(parent).await? {
            return Ok(Some(model));
        }
        for ancestor in self.db.thread_ancestors(parent).await? {
            if let Some(model) = self.db.thread_model(&ancestor).await? {
                return Ok(Some(model));
            }
        }
        Ok(None)
    }

    /// Persist the thread's model override and announce it. The next turn
    /// restarts the live session (resuming it) with `--model`.
    pub async fn set_thread_model(
        &self,
        thread_id: &ThreadId,
        model: Option<String>,
    ) -> Result<Option<String>> {
        let model = normalize_model(model)?;
        if self.db.get_thread(thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        self.db.set_thread_model(thread_id, model.clone()).await?;
        self.broadcast(
            thread_id,
            DaemonMessage::ThreadModelChanged {
                thread_id: thread_id.clone(),
                model: model.clone(),
            },
        );
        Ok(model)
    }

    /// `true` when the live session was started with another model than
    /// the thread should use now.
    pub(super) async fn live_model_outdated(&self, thread_id: &ThreadId) -> Result<bool> {
        let Some(started) = lock(&self.composer.live_models).get(thread_id).cloned() else {
            return Ok(false);
        };
        let thread = self
            .db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let workspace_default = match &thread.workspace_id {
            Some(id) => self
                .db
                .get_workspace(id)
                .await?
                .and_then(|ws| ws.settings.default_model),
            None => None,
        };
        let desired = self
            .thread_model_override(&thread)
            .await?
            .or(workspace_default);
        Ok(desired != started)
    }

    /// Shut the live session down between turns so the next one starts
    /// fresh (resuming the conversation). Its event pump ends quietly.
    pub(super) async fn retire_live_session(&self, thread_id: &ThreadId, session: AgentSession) {
        {
            let mut inner = self.lock();
            let token = inner.threads.get_mut(thread_id).and_then(|thread| {
                if thread.live_session.as_ref().map(|s| &s.id) == Some(&session.id) {
                    thread.live_session = None;
                }
                thread.session_allowed_tools.clear();
                thread.session_info = None;
                thread.approval_token.take()
            });
            if let Some(token) = token {
                inner.approval_tokens.remove(&token);
            }
        }
        lock(&self.composer.live_models).remove(thread_id);
        lock(&self.composer.retired_sessions).insert(session.id.clone());
        if let Err(error) = self.adapter.shutdown(&session).await {
            crate::log::warn(format!("failed to stop session for model change: {error}"));
        }
    }

    /// Persist the commands and skills the agent reported at session start.
    pub(super) async fn record_reported_commands(
        &self,
        thread_id: &ThreadId,
        slash_commands: &[String],
        skills: &[String],
    ) {
        let commands = AgentCommands {
            slash_commands: slash_commands.to_vec(),
            skills: skills.to_vec(),
        };
        if let Err(error) = self.db.record_agent_commands(thread_id, &commands).await {
            crate::log::warn(format!("failed to store reported commands: {error}"));
        }
    }

    /// `/` completion: skill and command files of the thread's checkouts,
    /// the user's and plugins', built-ins, and whatever the agent reported.
    pub async fn list_skills(&self, thread_id: &ThreadId) -> Result<Vec<SkillInfo>> {
        if self.db.get_thread(thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        let checkouts = self.ready_checkouts(thread_id).await?;
        let roots = SkillRoots {
            checkouts: checkouts.into_iter().map(|c| c.path).collect(),
            home: self.composer.home.clone(),
        };
        let found = tokio::task::spawn_blocking(move || skills::discover(&roots))
            .await
            .map_err(|error| DaemonError::Invalid(format!("skill discovery failed: {error}")))?;
        let reported = self.db.agent_commands(thread_id).await?.unwrap_or_default();
        let mut items: BTreeMap<String, SkillInfo> = BTreeMap::new();
        for item in found {
            items.entry(item.name.clone()).or_insert(item);
        }
        for (name, description) in BUILTIN_COMMANDS {
            items.entry((*name).into()).or_insert(SkillInfo {
                name: (*name).into(),
                description: Some((*description).into()),
                source: SkillSource::Builtin,
                kind: SkillKind::Command,
            });
        }
        let reported_skills: HashSet<&String> = reported.skills.iter().collect();
        for name in reported.slash_commands.iter().chain(&reported.skills) {
            if !skills::valid_name(name) {
                continue;
            }
            items.entry(name.clone()).or_insert(SkillInfo {
                name: name.clone(),
                description: None,
                source: SkillSource::Builtin,
                kind: if reported_skills.contains(name) {
                    SkillKind::Skill
                } else {
                    SkillKind::Command
                },
            });
        }
        Ok(items.into_values().collect())
    }
}
