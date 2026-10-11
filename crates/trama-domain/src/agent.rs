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
