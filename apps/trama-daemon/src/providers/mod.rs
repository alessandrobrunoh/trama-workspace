//! Model providers (ADR 0015): the built-in Claude Code CLI plus API
//! providers the user added (OpenAI, any OpenAI-compatible server).
//!
//! Provider settings live in `<app_dir>/providers.json` (no secrets); keys
//! in the OS keychain (see [`keys`]). API providers only run plain chat
//! turns; tools, files and subagents stay Claude Code only. Claude.ai
//! credentials are never read or sent anywhere by this module (ADR 0008):
//! an API provider bills the user's own key, and only once they added it.

mod keys;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use trama_infra::llm::OpenAiCompatClient;
use trama_protocol::{
    ApiKeyUpdate, CLAUDE_CODE_PROVIDER_ID, ModelInfo, ProviderConfig, ProviderKind,
    ProviderSnapshot, ProviderTestReport, qualify_model, split_qualified_model,
};

use crate::error::{DaemonError, Result};

pub(crate) const PROVIDERS_FILE: &str = "providers.json";
const MAX_PROVIDERS: usize = 32;
const MAX_MODELS: usize = 500;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_provider: Option<String>,
    #[serde(default)]
    providers: Vec<ProviderConfig>,
}

/// An API provider resolved for one turn.
pub(crate) struct ChatRoute {
    pub client: OpenAiCompatClient,
    pub model: String,
    pub provider_name: String,
}

pub(crate) struct ProviderStore {
    path: PathBuf,
    keys: keys::KeyStore,
    state: Mutex<StoreFile>,
}

fn claude_code() -> ProviderConfig {
    ProviderConfig {
        id: CLAUDE_CODE_PROVIDER_ID.into(),
        kind: ProviderKind::ClaudeCode,
        name: ProviderKind::ClaudeCode.label().into(),
        base_url: None,
        default_model: None,
        models: Vec::new(),
    }
}

/// `[a-z0-9-]` slug of `name`, 1-40 chars.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(40).collect();
    if out.is_empty() {
        "provider".into()
    } else {
        out.trim_end_matches('-').to_owned()
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl ProviderStore {
    pub(crate) fn open(app_dir: &Path) -> Self {
        let path = app_dir.join(PROVIDERS_FILE);
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
                crate::log::warn(format!("ignoring unreadable {PROVIDERS_FILE}: {err}"));
                StoreFile::default()
            }),
            Err(_) => StoreFile::default(),
        };
        Self {
            path,
            keys: keys::KeyStore::new(app_dir),
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoreFile> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn persist(&self, state: &StoreFile) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(state)
            .map_err(|err| DaemonError::Invalid(err.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn default_id(state: &StoreFile) -> String {
        state
            .default_provider
            .clone()
            .filter(|id| state.providers.iter().any(|p| &p.id == id))
            .unwrap_or_else(|| CLAUDE_CODE_PROVIDER_ID.into())
    }

    pub(crate) fn snapshots(&self) -> Vec<ProviderSnapshot> {
        let state = self.lock().clone();
        let default = Self::default_id(&state);
        std::iter::once(claude_code())
            .chain(state.providers)
            .map(|config| ProviderSnapshot {
                has_api_key: config.kind != ProviderKind::ClaudeCode
                    && self.keys.get(&config.id).is_some(),
                is_default: config.id == default,
                builtin: config.kind == ProviderKind::ClaudeCode,
                config,
            })
            .collect()
    }

    fn validate(config: &mut ProviderConfig) -> Result<()> {
        let invalid = |m: &str| Err(DaemonError::Invalid(m.into()));
        config.name = config.name.trim().chars().take(80).collect();
        if config.name.is_empty() {
            return invalid("the provider needs a name");
        }
        match config.kind {
            ProviderKind::ClaudeCode => {
                return invalid("Claude Code is built in and cannot be added or edited");
            }
            ProviderKind::OpenAi => {
                config.base_url = config
                    .base_url
                    .take()
                    .map(|url| url.trim().to_owned())
                    .filter(|url| !url.is_empty());
            }
            ProviderKind::OpenAiCompatible => {
                let url = config.base_url.as_deref().unwrap_or("").trim().to_owned();
                if url.is_empty() {
                    return invalid("an OpenAI-compatible provider needs a base URL");
                }
                config.base_url = Some(url);
            }
        }
        config.default_model = config
            .default_model
            .take()
            .map(|m| m.trim().to_owned())
            .filter(|m| !m.is_empty());
        config.models.retain(|m| !m.trim().is_empty());
        config.models.truncate(MAX_MODELS);
        Ok(())
    }

    fn base_url(config: &ProviderConfig) -> String {
        config
            .base_url
            .clone()
            .or_else(|| config.kind.default_base_url().map(str::to_owned))
            .unwrap_or_default()
    }

    fn client(config: &ProviderConfig, key: Option<String>) -> Result<OpenAiCompatClient> {
        if config.kind == ProviderKind::OpenAi && key.is_none() {
            return Err(DaemonError::Invalid("OpenAI needs an API key".into()));
        }
        OpenAiCompatClient::new(&Self::base_url(config), key)
            .map_err(|err| DaemonError::Invalid(err.to_string()))
    }

    pub(crate) fn save(&self, mut config: ProviderConfig, key: ApiKeyUpdate) -> Result<String> {
        Self::validate(&mut config)?;
        let mut state = self.lock().clone();
        if config.id.is_empty() {
            let base = slug(&config.name);
            let mut id = base.clone();
            let mut n = 2;
            while id == CLAUDE_CODE_PROVIDER_ID || state.providers.iter().any(|p| p.id == id) {
                id = format!("{base}-{n}");
                n += 1;
            }
            config.id = id;
        } else if !valid_id(&config.id) || config.id == CLAUDE_CODE_PROVIDER_ID {
            return Err(DaemonError::Invalid("invalid provider id".into()));
        }
        // Validate the URL against the transport policy with the key it will
        // be used with.
        let effective_key = match &key {
            ApiKeyUpdate::Set(k) => Some(k.clone()).filter(|k| !k.trim().is_empty()),
            ApiKeyUpdate::Clear => None,
            ApiKeyUpdate::Keep => self.keys.get(&config.id),
        };
        OpenAiCompatClient::new(&Self::base_url(&config), effective_key)
            .map_err(|err| DaemonError::Invalid(err.to_string()))?;
        match state.providers.iter_mut().find(|p| p.id == config.id) {
            Some(existing) => *existing = config.clone(),
            None => {
                if state.providers.len() >= MAX_PROVIDERS {
                    return Err(DaemonError::Invalid("too many providers".into()));
                }
                state.providers.push(config.clone());
            }
        }
        match key {
            ApiKeyUpdate::Keep => {}
            ApiKeyUpdate::Clear => self.keys.delete(&config.id),
            ApiKeyUpdate::Set(k) if k.trim().is_empty() => self.keys.delete(&config.id),
            ApiKeyUpdate::Set(k) => self
                .keys
                .set(&config.id, k.trim())
                .map_err(|err| DaemonError::Invalid(format!("could not store the key: {err}")))?,
        }
        self.persist(&state)?;
        *self.lock() = state;
        Ok(config.id)
    }

    pub(crate) fn remove(&self, id: &str) -> Result<()> {
        if id == CLAUDE_CODE_PROVIDER_ID {
            return Err(DaemonError::Invalid("Claude Code cannot be removed".into()));
        }
        let mut state = self.lock().clone();
        let before = state.providers.len();
        state.providers.retain(|p| p.id != id);
        if state.providers.len() == before {
            return Err(DaemonError::NotFound(format!("provider {id}")));
        }
        if state.default_provider.as_deref() == Some(id) {
            state.default_provider = None;
        }
        self.persist(&state)?;
        *self.lock() = state;
        self.keys.delete(id);
        Ok(())
    }

    pub(crate) fn set_default(&self, id: &str) -> Result<()> {
        let mut state = self.lock().clone();
        if id == CLAUDE_CODE_PROVIDER_ID {
            state.default_provider = None;
        } else {
            let provider = state
                .providers
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| DaemonError::NotFound(format!("provider {id}")))?;
            if provider.default_model.is_none() && provider.models.is_empty() {
                return Err(DaemonError::Invalid(
                    "set a default model for this provider first".into(),
                ));
            }
            state.default_provider = Some(id.to_owned());
        }
        self.persist(&state)?;
        *self.lock() = state;
        Ok(())
    }

    /// Test `id` (stored) or `draft` (unsaved settings). A stored provider's
    /// model list is refreshed on success.
    pub(crate) async fn test(
        &self,
        id: &str,
        draft: Option<ProviderConfig>,
        key: ApiKeyUpdate,
    ) -> ProviderTestReport {
        let failed = |message: String| ProviderTestReport {
            ok: false,
            message,
            models: Vec::new(),
        };
        let stored = self.lock().providers.iter().find(|p| p.id == id).cloned();
        let mut config = match draft.or_else(|| stored.clone()) {
            Some(config) => config,
            None if id == CLAUDE_CODE_PROVIDER_ID => {
                return ProviderTestReport {
                    ok: true,
                    message: "Claude Code runs through the local `claude` CLI and its own login."
                        .into(),
                    models: Vec::new(),
                };
            }
            None => return failed(format!("no provider {id}")),
        };
        if let Err(err) = Self::validate(&mut config) {
            return failed(err.to_string());
        }
        let key = match key {
            ApiKeyUpdate::Set(k) => Some(k).filter(|k| !k.trim().is_empty()),
            ApiKeyUpdate::Clear => None,
            ApiKeyUpdate::Keep => self.keys.get(id),
        };
        let client = match Self::client(&config, key) {
            Ok(client) => client,
            Err(err) => return failed(err.to_string()),
        };
        match client.list_models().await {
            Ok(models) => {
                if stored.is_some() && !models.is_empty() {
                    let mut state = self.lock().clone();
                    if let Some(p) = state.providers.iter_mut().find(|p| p.id == id) {
                        p.models = models.iter().take(MAX_MODELS).cloned().collect();
                        if self.persist(&state).is_ok() {
                            *self.lock() = state;
                        }
                    }
                }
                ProviderTestReport {
                    ok: true,
                    message: format!("Connected: {} models available.", models.len()),
                    models,
                }
            }
            Err(err) => failed(err.to_string()),
        }
    }

    /// Picker entries for every API provider's models (qualified ids).
    pub(crate) fn model_choices(&self) -> Vec<ModelInfo> {
        let state = self.lock().clone();
        let mut out = Vec::new();
        for provider in &state.providers {
            let mut models = provider.models.clone();
            if let Some(default) = &provider.default_model
                && !models.contains(default)
            {
                models.insert(0, default.clone());
            }
            for model in models {
                out.push(ModelInfo {
                    id: qualify_model(&provider.id, &model),
                    label: model,
                    provider: Some(provider.name.clone()),
                });
            }
        }
        out
    }

    /// Qualified model for threads without an override, when the default
    /// provider is an API provider.
    pub(crate) fn default_model(&self) -> Option<String> {
        let state = self.lock();
        let id = state.default_provider.as_deref()?;
        let provider = state.providers.iter().find(|p| p.id == id)?;
        let model = provider
            .default_model
            .clone()
            .or_else(|| provider.models.first().cloned())?;
        Some(qualify_model(&provider.id, &model))
    }

    /// The API route for a thread's effective model, `None` for Claude Code.
    /// An unknown provider is an error, not a silent fallback to Claude.
    pub(crate) fn route(&self, model: Option<&str>) -> Result<Option<ChatRoute>> {
        let qualified = match model {
            Some(model) => model.to_owned(),
            None => match self.default_model() {
                Some(model) => model,
                None => return Ok(None),
            },
        };
        let Some((provider_id, model)) = split_qualified_model(&qualified) else {
            return Ok(None);
        };
        if provider_id == CLAUDE_CODE_PROVIDER_ID {
            return Ok(None);
        }
        let config = self
            .lock()
            .providers
            .iter()
            .find(|p| p.id == provider_id)
            .cloned()
            .ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "the thread's provider `{provider_id}` was removed; pick another model"
                ))
            })?;
        let client = Self::client(&config, self.keys.get(provider_id))?;
        Ok(Some(ChatRoute {
            client,
            model: model.to_owned(),
            provider_name: config.name,
        }))
    }
}
