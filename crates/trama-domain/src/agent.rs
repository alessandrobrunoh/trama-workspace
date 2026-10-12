use crate::error::Result;
use crate::ids::AgentSessionId;
use async_channel::Sender;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Which agent runtime implements the adapter. ACP stays an adapter for
/// *other* agents — never the path for Claude Code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    ClaudeCode,
    Acp,
    Codex,
    Gemini,
    Builtin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    Bypass,
}

/// Which credentials the agent CLI may bill against.
///
/// `Subscription` (the default) strips `ANTHROPIC_API_KEY` and
/// `ANTHROPIC_AUTH_TOKEN` from the child environment so Claude Code uses the
/// user's Claude login. `ApiKey` is an explicit opt-in that keeps them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingMode {
    #[default]
    Subscription,
    ApiKey,
}

/// What the agent reported it is billing against, normalized from the
/// runtime's own field (Claude's `system/init` `apiKeySource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingSource {
    /// Claude subscription login (claude.ai OAuth).
    Subscription,
    /// Anthropic API key, `apiKeyHelper`, or a Console-managed key.
    ApiKey,
    /// A value this build does not recognize; see the raw source.
    Unknown,
}

/// Out-of-process approval hook the agent runs when a tool call needs a
/// permission decision. For Claude Code this is a `PermissionRequest` hook
/// injected with `--settings` (see ADR 0002).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalHookConfig {
    /// Executable to run. Passed shell-quoted, so spaces are fine.
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    /// Seconds the agent waits for the hook before cancelling it. A
    /// cancelled hook leaves the request undecided, which `-p` denies.
    pub timeout_secs: u32,
}

/// How a session is started. Maps 1:1 to Claude Code CLI flags; other
/// adapters translate to their own runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    #[serde(default)]
    pub extra_dirs: Vec<PathBuf>,
    pub mcp_config_path: Option<PathBuf>,
    pub append_system_prompt: Option<String>,
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Resume a previous external session (e.g. Claude `session_id`).
    pub resume_external_session: Option<String>,
    #[serde(default)]
    pub billing: BillingMode,
    /// When set, tool permission prompts are routed to this hook instead of
    /// being denied by the non-interactive CLI.
    #[serde(default)]
    pub approval_hook: Option<ApprovalHookConfig>,
    /// With `resume_external_session`, continue under a new session id
    /// instead of appending to the resumed one (Claude `--fork-session`).
    /// Subthreads use it to inherit their parent's conversation.
    #[serde(default)]
    pub fork_session: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPrompt {
    pub text: String,
}

/// A running agent session, owned by the adapter until shutdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: AgentSessionId,
    /// The agent's own session id (Claude's uuid — required for `--resume`).
    pub external_session_id: Option<String>,
    pub model: Option<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// Normalized agent event stream. Every adapter translates its native
/// protocol into these; the normalized form is what gets persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    SessionStarted {
        external_session_id: Option<String>,
        model: Option<String>,
    },
    /// Session metadata the UI can show next to the model, e.g. "Claude
    /// subscription" vs "API key". Emitted once per session start.
    SessionInfo {
        billing: BillingSource,
        /// Raw runtime value (Claude: `apiKeySource`, `"none"` = login).
        api_key_source: Option<String>,
        agent_version: Option<String>,
        permission_mode: Option<String>,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallStarted {
        tool_use_id: String,
        name: String,
        input_json: serde_json::Value,
    },
    ToolCallFinished {
        tool_use_id: String,
        output_json: serde_json::Value,
        is_error: bool,
    },
    ApprovalRequested {
        request_id: String,
        tool_name: String,
        input_json: serde_json::Value,
    },
    /// A pending approval was answered, by a client or automatically
    /// (timeout, interrupt, session end). `reason` is set for the latter.
    ApprovalResolved {
        request_id: String,
        decision: ApprovalDecision,
        reason: Option<String>,
    },
    SubagentStarted {
        parent_tool_use_id: Option<String>,
        label: Option<String>,
    },
    SubagentFinished {
        parent_tool_use_id: Option<String>,
    },
    UsageReported {
        report: UsageReport,
    },
    TurnCompleted {
        num_turns: u32,
        cost_usd_micros: Option<u64>,
    },
    Failed {
        message: String,
    },
    /// Slash commands and skills the agent reported at session start
    /// (Claude's `system/init` `slash_commands` / `skills`), names without
    /// the leading `/`.
    CommandsReported {
        #[serde(default)]
        slash_commands: Vec<String>,
        #[serde(default)]
        skills: Vec<String>,
    },
    /// Claude Code's transcript uuid of a completed top-level assistant
    /// message (`uuid` on stream-json `assistant` lines). The daemon keeps
    /// the last one of a turn on the stored assistant message so a thread
    /// can later be forked at that point. Not forwarded to clients.
    TranscriptEntry {
        uuid: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

/// Port for agent runtimes. Push-based events via `async-channel` so the
/// trait stays runtime-agnostic (GPUI foreground *or* tokio).
#[async_trait::async_trait]
pub trait AgentAdapter: Send + Sync {
    fn kind(&self) -> AgentKind;
    async fn start(
        &self,
        config: SessionConfig,
        events: Sender<AgentEvent>,
    ) -> Result<AgentSession>;
    async fn send(&self, session: &AgentSession, prompt: AgentPrompt) -> Result<()>;
    async fn respond_approval(
        &self,
        session: &AgentSession,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<()>;
    async fn interrupt(&self, session: &AgentSession) -> Result<()>;
    async fn shutdown(&self, session: &AgentSession) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let json = serde_json::to_string(value).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn assert_stable<T>(value: &T, expected: &str)
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let json = serde_json::to_string(value).unwrap();
        assert_eq!(json, expected, "stable json shape");
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(value).unwrap()
        );
        let again = serde_json::to_string(&roundtrip(value)).unwrap();
        assert_eq!(again, expected);
    }

    #[test]
    fn agent_event_serde_roundtrip_every_variant() {
        assert_stable(
            &AgentEvent::SessionStarted {
                external_session_id: Some("ext-1".into()),
                model: Some("claude".into()),
            },
            r#"{"type":"session_started","external_session_id":"ext-1","model":"claude"}"#,
        );
        assert_stable(
            &AgentEvent::SessionStarted {
                external_session_id: None,
                model: None,
            },
            r#"{"type":"session_started","external_session_id":null,"model":null}"#,
        );
        assert_stable(
            &AgentEvent::TextDelta {
                text: "hello".into(),
            },
            r#"{"type":"text_delta","text":"hello"}"#,
        );
        assert_stable(
            &AgentEvent::ReasoningDelta {
                text: "think".into(),
            },
            r#"{"type":"reasoning_delta","text":"think"}"#,
        );
        assert_stable(
            &AgentEvent::ToolCallStarted {
                tool_use_id: "t1".into(),
                name: "bash".into(),
                input_json: serde_json::json!({"cmd": "ls"}),
            },
            r#"{"type":"tool_call_started","tool_use_id":"t1","name":"bash","input_json":{"cmd":"ls"}}"#,
        );
        assert_stable(
            &AgentEvent::ToolCallFinished {
                tool_use_id: "t1".into(),
                output_json: serde_json::json!("ok"),
                is_error: false,
            },
            r#"{"type":"tool_call_finished","tool_use_id":"t1","output_json":"ok","is_error":false}"#,
        );
        assert_stable(
            &AgentEvent::ApprovalRequested {
                request_id: "r1".into(),
                tool_name: "edit".into(),
                input_json: serde_json::json!({"path": "a.rs"}),
            },
            r#"{"type":"approval_requested","request_id":"r1","tool_name":"edit","input_json":{"path":"a.rs"}}"#,
        );
        assert_stable(
            &AgentEvent::ApprovalResolved {
                request_id: "r1".into(),
                decision: ApprovalDecision::AllowSession,
                reason: None,
            },
            r#"{"type":"approval_resolved","request_id":"r1","decision":"allow_session","reason":null}"#,
        );
        assert_stable(
            &AgentEvent::SessionInfo {
                billing: BillingSource::Subscription,
                api_key_source: Some("none".into()),
                agent_version: Some("2.1.296".into()),
                permission_mode: Some("default".into()),
            },
            r#"{"type":"session_info","billing":"subscription","api_key_source":"none","agent_version":"2.1.296","permission_mode":"default"}"#,
        );
        assert_stable(
            &AgentEvent::SubagentStarted {
                parent_tool_use_id: Some("t1".into()),
                label: Some("explore".into()),
            },
            r#"{"type":"subagent_started","parent_tool_use_id":"t1","label":"explore"}"#,
        );
        assert_stable(
            &AgentEvent::CommandsReported {
                slash_commands: vec!["compact".into()],
                skills: vec!["land".into()],
            },
            r#"{"type":"commands_reported","slash_commands":["compact"],"skills":["land"]}"#,
        );
        assert_stable(
            &AgentEvent::SubagentFinished {
                parent_tool_use_id: None,
            },
            r#"{"type":"subagent_finished","parent_tool_use_id":null}"#,
        );
        assert_stable(
            &AgentEvent::UsageReported {
                report: UsageReport {
                    model: "claude".into(),
                    input_tokens: 10,
                    output_tokens: 4,
                    cache_read_tokens: 1,
                    cache_write_tokens: 2,
                },
            },
            r#"{"type":"usage_reported","report":{"model":"claude","input_tokens":10,"output_tokens":4,"cache_read_tokens":1,"cache_write_tokens":2}}"#,
        );
        assert_stable(
            &AgentEvent::TurnCompleted {
                num_turns: 3,
                cost_usd_micros: Some(42),
            },
            r#"{"type":"turn_completed","num_turns":3,"cost_usd_micros":42}"#,
        );
        assert_stable(
            &AgentEvent::Failed {
                message: "boom".into(),
            },
            r#"{"type":"failed","message":"boom"}"#,
        );
        assert_stable(
            &AgentEvent::TranscriptEntry {
                uuid: "6f1c".into(),
            },
            r#"{"type":"transcript_entry","uuid":"6f1c"}"#,
        );
    }

    #[test]
    fn session_config_and_related_enums_roundtrip() {
        let config = SessionConfig {
            cwd: PathBuf::from("/tmp/ws"),
            model: Some("opus".into()),
            permission_mode: PermissionMode::AcceptEdits,
            allowed_tools: vec!["bash".into()],
            disallowed_tools: vec![],
            extra_dirs: vec![PathBuf::from("/tmp/extra")],
            mcp_config_path: None,
            append_system_prompt: Some("be brief".into()),
            max_turns: Some(8),
            env: BTreeMap::from([("FOO".into(), "bar".into())]),
            resume_external_session: None,
            billing: BillingMode::ApiKey,
            approval_hook: Some(ApprovalHookConfig {
                command: PathBuf::from("/opt/trama/trama-daemon"),
                args: vec!["approval-hook".into()],
                timeout_secs: 60,
            }),
            fork_session: true,
        };
        let back = roundtrip(&config);
        assert!(back.fork_session);
        let mut legacy = serde_json::to_value(&config).unwrap();
        legacy.as_object_mut().unwrap().remove("fork_session");
        let legacy: SessionConfig = serde_json::from_value(legacy).unwrap();
        assert!(!legacy.fork_session);
        assert_eq!(back.cwd, config.cwd);
        assert_eq!(back.billing, BillingMode::ApiKey);
        assert_eq!(back.approval_hook, config.approval_hook);
        assert_eq!(back.permission_mode, PermissionMode::AcceptEdits);
        assert_eq!(roundtrip(&AgentKind::ClaudeCode), AgentKind::ClaudeCode);
        assert_eq!(roundtrip(&AgentKind::Acp), AgentKind::Acp);
        assert_eq!(
            roundtrip(&ApprovalDecision::AllowOnce),
            ApprovalDecision::AllowOnce
        );
        assert_eq!(
            serde_json::to_string(&AgentKind::ClaudeCode).unwrap(),
            r#""claude_code""#
        );
        assert_eq!(
            serde_json::to_string(&PermissionMode::Bypass).unwrap(),
            r#""bypass""#
        );
    }

    #[test]
    fn session_config_without_new_fields_defaults_to_subscription() {
        let config: SessionConfig = serde_json::from_str(
            r#"{"cwd":"/tmp","model":null,"permission_mode":"default","mcp_config_path":null,"append_system_prompt":null,"max_turns":null,"resume_external_session":null}"#,
        )
        .unwrap();
        assert_eq!(config.billing, BillingMode::Subscription);
        assert!(config.approval_hook.is_none());
        assert_eq!(
            serde_json::to_string(&BillingMode::ApiKey).unwrap(),
            r#""api_key""#
        );
    }
}
