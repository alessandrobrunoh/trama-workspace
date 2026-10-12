//! `claude` CLI process adapter.
//!
//! Each session is a long-lived child (`claude -p --input-format stream-json
//! --output-format stream-json --verbose --include-partial-messages`). Turns
//! are written as NDJSON user messages on stdin. `--bare` is intentionally
//! **not** passed: it disables CLAUDE.md, skills, and MCP discovery.
//!
//! Interrupt sends SIGINT to the child pid via `kill -INT` on Unix. The
//! SIGKILL fallback is [`tokio::process::Child::kill`].
//!
//! Billing: with [`BillingMode::Subscription`] (the default) the child never
//! sees `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`, so Claude Code bills the
//! user's Claude login instead of the API. [`BillingMode::ApiKey`] keeps them.
//!
//! Approval: when [`SessionConfig::approval_hook`] is set, a
//! `PermissionRequest` hook is injected with `--settings` (ADR 0002). Claude
//! Code runs it for every tool call that would prompt; the hook asks the
//! daemon and prints the decision. The decision never flows through this
//! adapter, so [`AgentAdapter::respond_approval`] returns
//! [`DomainError::Invalid`].

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_channel::Sender;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use trama_domain::agent::{
    AgentAdapter, AgentEvent, AgentKind, AgentPrompt, AgentSession, ApprovalDecision,
    ApprovalHookConfig, BillingMode, PermissionMode, SessionConfig,
};
use trama_domain::ids::AgentSessionId;
use trama_domain::{DomainError, Result};

use super::protocol::{ClaudeLine, parse_line};
use super::translate::{TranslateState, translate_line_in};

const SHUTDOWN_GRACE: Duration = Duration::from_millis(1500);

/// Variables that make Claude Code bill the Anthropic API instead of the
/// subscription login. Removed from the child unless billing is `ApiKey`.
pub const API_BILLING_ENV: [&str; 2] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

struct SessionState {
    child: Child,
    stdin: Option<ChildStdin>,
    pid: Option<u32>,
    external_session_id: Option<String>,
    model: Option<String>,
    reader: JoinHandle<()>,
}

type SessionMap = HashMap<AgentSessionId, SessionState>;

/// Native Claude Code adapter. One process per [`AgentSessionId`].
pub struct ClaudeAdapter {
    binary: String,
    sessions: Arc<Mutex<SessionMap>>,
}

impl ClaudeAdapter {
    /// Resolve the CLI and build an adapter.
    pub fn new() -> Result<Self> {
        Ok(Self::with_binary(Self::detect()?))
    }

    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// `CLAUDE_BIN` if set, otherwise `claude` on `PATH`.
    pub fn detect() -> Result<String> {
        detect_claude_binary(std::env::var("CLAUDE_BIN").ok(), std::env::var_os("PATH"))
    }

    pub fn binary(&self) -> &str {
        &self.binary
    }
}

pub(crate) fn detect_claude_binary(
    claude_bin: Option<String>,
    path_var: Option<std::ffi::OsString>,
) -> Result<String> {
    if let Some(explicit) = claude_bin {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            let as_path = PathBuf::from(trimmed);
            if as_path.is_file() {
                return Ok(as_path.to_string_lossy().into_owned());
            }
            if as_path.components().count() > 1 {
                return Err(DomainError::Agent(format!(
                    "CLAUDE_BIN is set to `{trimmed}` but that path is not a file. \
                     Install Claude Code or point CLAUDE_BIN at the `claude` binary."
                )));
            }
            if let Some(found) = find_on_path(trimmed, path_var.as_deref()) {
                return Ok(found.to_string_lossy().into_owned());
            }
            return Err(DomainError::Agent(format!(
                "CLAUDE_BIN is set to `{trimmed}` but it was not found on PATH. \
                 Install Claude Code or point CLAUDE_BIN at the `claude` binary."
            )));
        }
    }

    match find_on_path("claude", path_var.as_deref()) {
        Some(found) => Ok(found.to_string_lossy().into_owned()),
        None => Err(DomainError::Agent(
            "Claude Code CLI not found. Set CLAUDE_BIN to the `claude` executable, \
             or install Claude Code so `claude` is on PATH (https://code.claude.com)."
                .to_string(),
        )),
    }
}

fn find_on_path(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// CLI argv for a session. Does not include the binary name. Never adds `--bare`.
pub fn claude_cli_args(config: &SessionConfig) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--include-partial-messages".to_string(),
        "--permission-mode".to_string(),
        permission_mode_flag(&config.permission_mode).to_string(),
    ];

    if let Some(model) = config.model.as_ref().filter(|m| !m.is_empty()) {
        args.push("--model".to_string());
        args.push(model.clone());
    }
    if !config.allowed_tools.is_empty() {
        args.push("--allowed-tools".to_string());
        args.push(config.allowed_tools.join(","));
    }
    if !config.disallowed_tools.is_empty() {
        args.push("--disallowed-tools".to_string());
        args.push(config.disallowed_tools.join(","));
    }
    for dir in &config.extra_dirs {
        args.push("--add-dir".to_string());
        args.push(dir.to_string_lossy().into_owned());
    }
    if let Some(path) = &config.mcp_config_path {
        args.push("--mcp-config".to_string());
        args.push(path.to_string_lossy().into_owned());
    }
    if let Some(prompt) = config
        .append_system_prompt
        .as_ref()
        .filter(|s| !s.is_empty())
    {
        args.push("--append-system-prompt".to_string());
        args.push(prompt.clone());
    }
    if let Some(max_turns) = config.max_turns {
        args.push("--max-turns".to_string());
        args.push(max_turns.to_string());
    }
    if let Some(resume) = config
        .resume_external_session
        .as_ref()
        .filter(|s| !s.is_empty())
    {
        args.push("--resume".to_string());
        args.push(resume.clone());
    }
    if let Some(hook) = &config.approval_hook {
        // `--settings` overlays the user's settings for this session only;
        // hook entries merge with theirs rather than replacing them.
        args.push("--settings".to_string());
        args.push(approval_hook_settings(hook).to_string());
    }
    args
}

/// Settings JSON that registers `hook` as a `PermissionRequest` command hook
/// for every tool. Claude Code runs it only when a call would otherwise
/// prompt, after deny/ask rules, the permission mode, and allow rules.
pub fn approval_hook_settings(hook: &ApprovalHookConfig) -> serde_json::Value {
    let mut command = shell_quote(&hook.command.to_string_lossy());
    for arg in &hook.args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    serde_json::json!({
        "hooks": {
            "PermissionRequest": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": hook.timeout_secs,
                }],
            }],
        },
    })
}

/// Single-quote for `sh`. Hook commands run through a shell, and the daemon
/// lives under `Application Support`, so paths routinely contain spaces.
fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\\''"))
}

/// Child env keys to remove after `config.env` is applied. Under the
/// subscription default this also drops API credentials set explicitly in
/// `config.env`: API billing must be opted into with [`BillingMode::ApiKey`].
pub fn child_env_removals(config: &SessionConfig) -> &'static [&'static str] {
    match config.billing {
        BillingMode::Subscription => &API_BILLING_ENV,
        BillingMode::ApiKey => &[],
    }
}

pub fn permission_mode_flag(mode: &PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::Bypass => "bypassPermissions",
    }
}

/// One stdin turn. `content` is a string, matching Claude Code's streaming input.
pub fn user_turn_ndjson(text: &str) -> Result<String> {
    let payload = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": text,
        },
        "parent_tool_use_id": serde_json::Value::Null,
    });
    Ok(format!("{}\n", serde_json::to_string(&payload)?))
}

#[async_trait::async_trait]
impl AgentAdapter for ClaudeAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::ClaudeCode
    }

    async fn start(
        &self,
        config: SessionConfig,
        events: Sender<AgentEvent>,
    ) -> Result<AgentSession> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(claude_cli_args(&config))
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for (key, value) in &config.env {
            cmd.env(key, value);
        }
        for key in child_env_removals(&config) {
            cmd.env_remove(key);
        }

        let mut child = cmd.spawn().map_err(|err| {
            DomainError::Agent(format!(
                "failed to spawn `{}`: {err}. Check CLAUDE_BIN / PATH.",
                self.binary
            ))
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DomainError::Agent("claude child stdout was not piped".to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| DomainError::Agent("claude child stdin was not piped".to_string()))?;
        let pid = child.id();
        let session_id = AgentSessionId::generate();
        let model = config.model.clone();
        let cwd = config.cwd.clone();

        let session = AgentSession {
            id: session_id.clone(),
            external_session_id: None,
            model,
            cwd,
        };

        // Register before the reader runs so `system`/`init` can store
        // `session_id` back onto this entry.
        let (reader_go, reader_wait) = tokio::sync::oneshot::channel::<()>();
        let reader_events = events;
        let reader_sessions = Arc::clone(&self.sessions);
        let reader_id = session_id.clone();
        let reader = tokio::spawn(async move {
            let _ = reader_wait.await;
            read_stdout(stdout, reader_events, reader_sessions, reader_id).await;
        });

        self.sessions.lock().await.insert(
            session_id,
            SessionState {
                child,
                stdin: Some(stdin),
                pid,
                external_session_id: None,
                model: config.model,
                reader,
            },
        );
        let _ = reader_go.send(());

        Ok(session)
    }

    async fn send(&self, session: &AgentSession, prompt: AgentPrompt) -> Result<()> {
        let mut sessions = self.sessions.lock().await;
        let state = sessions
            .get_mut(&session.id)
            .ok_or_else(|| DomainError::NotFound(format!("agent session {}", session.id)))?;
        let stdin = state.stdin.as_mut().ok_or_else(|| {
            DomainError::Invalid(format!("agent session {} stdin is closed", session.id))
        })?;
        let line = user_turn_ndjson(&prompt.text)?;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn respond_approval(
        &self,
        _session: &AgentSession,
        _request_id: &str,
        _decision: ApprovalDecision,
    ) -> Result<()> {
        // Claude Code blocks on the `PermissionRequest` hook process, which
        // receives the decision from the daemon's approval broker. There is
        // no in-band channel on this process to answer through.
        Err(DomainError::Invalid(
            "Claude Code approvals are answered through the PermissionRequest hook; \
             no pending request is held by the adapter"
                .to_string(),
        ))
    }

    async fn interrupt(&self, session: &AgentSession) -> Result<()> {
        let sessions = self.sessions.lock().await;
        let state = sessions
            .get(&session.id)
            .ok_or_else(|| DomainError::NotFound(format!("agent session {}", session.id)))?;
        let pid = state.pid.ok_or_else(|| {
            DomainError::Agent(format!("agent session {} has no pid", session.id))
        })?;
        drop(sessions);

        // Unix: SIGINT via `kill -INT`. SIGKILL fallback is `child.kill()`.
        #[cfg(unix)]
        {
            let pid_str = pid.to_string();
            let status = tokio::task::spawn_blocking(move || {
                std::process::Command::new("kill")
                    .args(["-INT", &pid_str])
                    .status()
            })
            .await
            .map_err(|err| DomainError::Agent(format!("join kill -INT: {err}")))?;
            let status = status?;
            if !status.success() {
                return Err(DomainError::Agent(format!(
                    "kill -INT {pid} exited with {status}"
                )));
            }
            Ok(())
        }

        #[cfg(not(unix))]
        {
            let _ = pid;
            let mut sessions = self.sessions.lock().await;
            if let Some(state) = sessions.get_mut(&session.id) {
                state
                    .child
                    .kill()
                    .await
                    .map_err(|err| DomainError::Agent(format!("child.kill: {err}")))?;
            }
            Ok(())
        }
    }

    async fn shutdown(&self, session: &AgentSession) -> Result<()> {
        let mut sessions = self.sessions.lock().await;
        let Some(mut state) = sessions.remove(&session.id) else {
            return Ok(());
        };
        drop(sessions);

        drop(state.stdin.take());
        state.reader.abort();

        match timeout(SHUTDOWN_GRACE, state.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(DomainError::Agent(format!("wait for claude: {err}"))),
            Err(_) => {
                // SIGKILL fallback.
                let _ = state.child.kill().await;
                let _ = state.child.wait().await;
                Ok(())
            }
        }
    }
}

async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    events: Sender<AgentEvent>,
    sessions: Arc<Mutex<SessionMap>>,
    session_id: AgentSessionId,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut state = TranslateState::default();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let parsed = match parse_line(&line) {
                    Ok(Some(parsed)) => parsed,
                    Ok(None) => continue,
                    Err(err) => {
                        let event = AgentEvent::Failed {
                            message: format!("invalid claude NDJSON: {err}"),
                        };
                        if events.send(event).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };

                if let ClaudeLine::System(sys) = &parsed {
                    if sys.subtype == "init" {
                        let mut guard = sessions.lock().await;
                        if let Some(session) = guard.get_mut(&session_id) {
                            session.external_session_id = sys.session_id.clone();
                            if sys.model.is_some() {
                                session.model = sys.model.clone();
                            }
                        }
                    }
                }

                for event in translate_line_in(&parsed, &mut state) {
                    if events.send(event).await.is_err() {
                        return;
                    }
                }
            }
            Ok(None) => break,
            Err(err) => {
                let event = AgentEvent::Failed {
                    message: format!("claude stdout: {err}"),
                };
                let _ = events.send(event).await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use trama_domain::agent::AgentAdapter;

    fn sample_config() -> SessionConfig {
        SessionConfig {
            cwd: PathBuf::from("/tmp/ws"),
            model: Some("claude-opus-4-6".into()),
            permission_mode: PermissionMode::AcceptEdits,
            allowed_tools: vec!["Bash".into(), "Read".into()],
            disallowed_tools: vec!["Bash(rm *)".into()],
            extra_dirs: vec![PathBuf::from("/tmp/extra")],
            mcp_config_path: Some(PathBuf::from("/tmp/mcp.json")),
            append_system_prompt: Some("be brief".into()),
            max_turns: Some(4),
            env: BTreeMap::new(),
            resume_external_session: Some("ses_resume".into()),
            billing: BillingMode::Subscription,
            approval_hook: None,
        }
    }

    #[test]
    fn cli_args_map_session_config_and_omit_bare() {
        let args = claude_cli_args(&sample_config());
        let joined = args.join(" ");
        assert!(joined.contains("-p"));
        assert!(joined.contains("--input-format stream-json"));
        assert!(joined.contains("--output-format stream-json"));
        assert!(joined.contains("--verbose"));
        assert!(joined.contains("--include-partial-messages"));
        assert!(joined.contains("--permission-mode acceptEdits"));
        assert!(joined.contains("--model claude-opus-4-6"));
        assert!(joined.contains("--allowed-tools Bash,Read"));
        assert!(joined.contains("--disallowed-tools Bash(rm *)"));
        assert!(joined.contains("--add-dir /tmp/extra"));
        assert!(joined.contains("--mcp-config /tmp/mcp.json"));
        assert!(joined.contains("--append-system-prompt be brief"));
        assert!(joined.contains("--max-turns 4"));
        assert!(joined.contains("--resume ses_resume"));
        assert!(!args.iter().any(|a| a == "--bare"));
        assert!(!args.iter().any(|a| a == "--settings"));
    }

    #[test]
    fn approval_hook_is_injected_as_permission_request_settings() {
        let mut config = sample_config();
        config.approval_hook = Some(ApprovalHookConfig {
            command: PathBuf::from("/Users/me/Library/Application Support/it's/trama-daemon"),
            args: vec!["approval-hook".into()],
            timeout_secs: 1830,
        });
        let args = claude_cli_args(&config);
        assert!(!args.iter().any(|a| a == "--bare"));
        assert!(!args.iter().any(|a| a == "--permission-prompt-tool"));
        let at = args
            .iter()
            .position(|a| a == "--settings")
            .expect("--settings");
        let settings: serde_json::Value = serde_json::from_str(&args[at + 1]).unwrap();
        assert_eq!(
            settings,
            serde_json::json!({
                "hooks": {
                    "PermissionRequest": [{
                        "matcher": "*",
                        "hooks": [{
                            "type": "command",
                            "command": "'/Users/me/Library/Application Support/it'\\''s/trama-daemon' 'approval-hook'",
                            "timeout": 1830
                        }]
                    }]
                }
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn hook_command_survives_sh_word_splitting() {
        let hook = ApprovalHookConfig {
            command: PathBuf::from("/bin/echo"),
            args: vec!["a b".into(), "it's".into()],
            timeout_secs: 5,
        };
        let settings = approval_hook_settings(&hook);
        let command = settings["hooks"]["PermissionRequest"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string();
        let out = std::process::Command::new("sh")
            .args(["-c", &command])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "a b it's\n");
    }

    #[test]
    fn subscription_billing_strips_api_credentials() {
        let mut config = sample_config();
        assert_eq!(config.billing, BillingMode::Subscription);
        assert_eq!(
            child_env_removals(&config),
            ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
        );
        config.billing = BillingMode::ApiKey;
        assert!(child_env_removals(&config).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawned_child_env_respects_billing_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile_dir("billing-env");
        let dump = dir.join("env.txt");
        let script = dir.join("claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nenv > \"{}\"\nexec cat >/dev/null\n",
                dump.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let adapter = ClaudeAdapter::with_binary(script.to_string_lossy().into_owned());

        for (billing, expect_keys) in [
            (BillingMode::Subscription, false),
            (BillingMode::ApiKey, true),
        ] {
            let _ = std::fs::remove_file(&dump);
            let mut config = sample_config();
            config.cwd = dir.clone();
            config.extra_dirs.clear();
            config.billing = billing;
            config.env = BTreeMap::from([
                ("ANTHROPIC_API_KEY".into(), "sk-test".into()),
                ("ANTHROPIC_AUTH_TOKEN".into(), "tok-test".into()),
                ("TRAMA_KEEP".into(), "1".into()),
            ]);
            let (tx, _rx) = async_channel::unbounded();
            let session = adapter.start(config, tx).await.unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            let env = loop {
                match std::fs::read_to_string(&dump) {
                    Ok(env) if env.contains("TRAMA_KEEP=1") => break env,
                    _ if tokio::time::Instant::now() > deadline => panic!("env dump missing"),
                    _ => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            };
            adapter.shutdown(&session).await.unwrap();
            assert_eq!(
                env.contains("ANTHROPIC_API_KEY="),
                expect_keys,
                "{billing:?}"
            );
            assert_eq!(
                env.contains("ANTHROPIC_AUTH_TOKEN="),
                expect_keys,
                "{billing:?}"
            );
        }
    }

    fn tempfile_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trama-claude-{label}-{}-{}",
            std::process::id(),
            AgentSessionId::generate()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn permission_mode_cli_values() {
        assert_eq!(permission_mode_flag(&PermissionMode::Default), "default");
        assert_eq!(
            permission_mode_flag(&PermissionMode::AcceptEdits),
            "acceptEdits"
        );
        assert_eq!(permission_mode_flag(&PermissionMode::Plan), "plan");
        assert_eq!(
            permission_mode_flag(&PermissionMode::Bypass),
            "bypassPermissions"
        );
    }

    #[test]
    fn user_turn_shape() {
        let line = user_turn_ndjson("hello").unwrap();
        let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": "hello" },
                "parent_tool_use_id": null
            })
        );
    }

    #[test]
    fn detect_missing_explicit_path() {
        let err =
            detect_claude_binary(Some("/definitely/missing/claude-bin".into()), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("CLAUDE_BIN"), "{msg}");
        assert!(msg.contains("not a file"), "{msg}");
    }

    #[test]
    fn detect_missing_on_empty_path() {
        let err = detect_claude_binary(None, Some(std::ffi::OsString::from(""))).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Claude Code CLI not found"), "{msg}");
    }

    #[tokio::test]
    async fn respond_approval_points_at_the_hook() {
        let adapter = ClaudeAdapter::with_binary("claude");
        let session = AgentSession {
            id: AgentSessionId::from("s1"),
            external_session_id: None,
            model: None,
            cwd: PathBuf::from("/tmp"),
        };
        let err = adapter
            .respond_approval(&session, "req", ApprovalDecision::AllowOnce)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Invalid(msg) if msg.contains("PermissionRequest hook")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_cli_streams_fixture_and_stores_session_id() {
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use tokio::time::sleep;

        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple_text.ndjson");
        let dir = std::env::temp_dir().join(format!("trama-claude-fake-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let script = dir.join("claude");
        let script_body = format!(
            "#!/bin/sh\ncat \"{}\"\nexec cat >/dev/null\n",
            fixture.display()
        );
        std::fs::write(&script, script_body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let adapter = ClaudeAdapter::with_binary(script.to_string_lossy().into_owned());
        let (tx, rx) = async_channel::unbounded();
        let config = SessionConfig {
            cwd: std::env::temp_dir(),
            model: None,
            permission_mode: PermissionMode::Default,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            extra_dirs: Vec::new(),
            mcp_config_path: None,
            append_system_prompt: None,
            max_turns: None,
            env: BTreeMap::new(),
            resume_external_session: None,
            billing: BillingMode::Subscription,
            approval_hook: None,
        };
        let session = adapter.start(config, tx).await.unwrap();

        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(event)) => {
                    let done = matches!(event, AgentEvent::TurnCompleted { .. });
                    events.push(event);
                    if done {
                        break;
                    }
                }
                _ => {
                    if !events.is_empty() {
                        break;
                    }
                    sleep(Duration::from_millis(20)).await;
                }
            }
        }

        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::SessionStarted {
                    external_session_id: Some(id),
                    ..
                } if id == "ses_simple"
            )),
            "events: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
        );

        {
            let guard = adapter.sessions.lock().await;
            let stored = guard.get(&session.id).expect("session in registry");
            assert_eq!(stored.external_session_id.as_deref(), Some("ses_simple"));
        }

        adapter.shutdown(&session).await.unwrap();
    }
}
