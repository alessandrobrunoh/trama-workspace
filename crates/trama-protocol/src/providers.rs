//! Model providers (ADR 0015): the Claude Code CLI (built in, agentic) and
//! user-added API providers that run plain chat turns.
//!
//! A thread selects an API provider through its model id: a *qualified*
//! model `"<provider_id>::<model>"` (see [`qualify_model`]). A model id
//! without the separator is a Claude Code model, so existing threads and
//! `SetThreadModel` callers keep working unchanged.

use serde::{Deserialize, Serialize};

/// Id of the built-in Claude Code CLI provider. It cannot be removed.
pub const CLAUDE_CODE_PROVIDER_ID: &str = "claude-code";
/// Separator between provider id and model in a qualified model id.
pub const MODEL_PROVIDER_SEPARATOR: &str = "::";

/// `"<provider_id>::<model>"`.
pub fn qualify_model(provider_id: &str, model: &str) -> String {
    format!("{provider_id}{MODEL_PROVIDER_SEPARATOR}{model}")
}

/// Split a qualified model id; `None` for a plain (Claude Code) model id.
pub fn split_qualified_model(model: &str) -> Option<(&str, &str)> {
    let (provider, model) = model.split_once(MODEL_PROVIDER_SEPARATOR)?;
    (!provider.is_empty() && !model.is_empty()).then_some((provider, model))
}

/// Which wire protocol a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// The local `claude` CLI (subscription or its own API billing, ADR
    /// 0008). Tools, files, subagents: the full agent.
    ClaudeCode,
    /// `https://api.openai.com/v1`, API key required.
    OpenAi,
    /// Any OpenAI-compatible server: OpenRouter, Groq, Together, Ollama,
    /// LM Studio, vLLM... Base URL required, key optional.
    OpenAiCompatible,
}

impl ProviderKind {
    /// Whether turns can use tools and edit files. Only Claude Code can.
    pub fn is_agentic(self) -> bool {
        matches!(self, Self::ClaudeCode)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code CLI",
            Self::OpenAi => "OpenAI",
            Self::OpenAiCompatible => "OpenAI-compatible",
        }
    }

    /// Base URL used when the provider does not set one.
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("https://api.openai.com/v1"),
            _ => None,
        }
    }
}

/// A provider as stored and edited. Never carries an API key: keys live in
/// the OS keychain (file fallback) and travel only in [`ApiKeyUpdate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Stable id (`[a-z0-9-]`, 1-40 chars). Empty in `SaveProvider` asks the
    /// daemon to derive one from `name`.
    pub id: String,
    pub kind: ProviderKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Model used when a thread picks this provider without a model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Models offered in the picker (filled by a successful test, editable).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

/// What `SaveProvider` does with the stored key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub enum ApiKeyUpdate {
    #[default]
    Keep,
    Set(String),
    Clear,
}

/// A provider as listed by `Providers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    #[serde(flatten)]
    pub config: ProviderConfig,
    /// A key is stored (its value is never sent to clients).
    pub has_api_key: bool,
    pub is_default: bool,
    /// Built in (Claude Code): cannot be removed or edited.
    pub builtin: bool,
}

/// Outcome of `TestProvider`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTestReport {
    pub ok: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_models_round_trip() {
        let id = qualify_model("ollama", "llama3.1:8b");
        assert_eq!(split_qualified_model(&id), Some(("ollama", "llama3.1:8b")));
        assert_eq!(split_qualified_model("claude-sonnet-5-5"), None);
        assert_eq!(split_qualified_model("::x"), None);
    }

    #[test]
    fn key_update_wire_format() {
        let json = serde_json::to_string(&ApiKeyUpdate::Set("k".into())).unwrap();
        assert_eq!(json, r#"{"action":"set","value":"k"}"#);
        let keep: ApiKeyUpdate = serde_json::from_str(r#"{"action":"keep"}"#).unwrap();
        assert_eq!(keep, ApiKeyUpdate::Keep);
    }
}
