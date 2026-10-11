//! Production adapter: resolve `claude` lazily so the daemon can still serve
//! lists when the binary is missing. Failure surfaces on `start`.

use async_channel::Sender;
use tokio::sync::Mutex;
use trama_domain::agent::{
    AgentAdapter, AgentEvent, AgentKind, AgentPrompt, AgentSession, ApprovalDecision, SessionConfig,
};
use trama_domain::{DomainError, Result};
use trama_infra::ClaudeAdapter;

const MISSING_CLAUDE: &str = "Claude Code CLI not found. Set CLAUDE_BIN to the `claude` executable, \
     or install Claude Code so `claude` is on PATH (https://code.claude.com).";

#[derive(Default)]
pub struct ProductionAdapter {
    inner: Mutex<Option<ClaudeAdapter>>,
}

impl ProductionAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    async fn ensure(&self) -> Result<tokio::sync::MutexGuard<'_, Option<ClaudeAdapter>>> {
        let mut guard = self.inner.lock().await;
        if guard.is_none() {
            match ClaudeAdapter::new() {
                Ok(adapter) => *guard = Some(adapter),
                Err(err) => return Err(err),
            }
        }
        Ok(guard)
    }
}

fn borrow(guard: &Option<ClaudeAdapter>) -> Result<&ClaudeAdapter> {
    guard
        .as_ref()
        .ok_or_else(|| DomainError::Agent(MISSING_CLAUDE.into()))
}

#[async_trait::async_trait]
impl AgentAdapter for ProductionAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::ClaudeCode
    }

    async fn start(
        &self,
        config: SessionConfig,
        events: Sender<AgentEvent>,
    ) -> Result<AgentSession> {
        let guard = self.ensure().await?;
        borrow(&guard)?.start(config, events).await
    }

    async fn send(&self, session: &AgentSession, prompt: AgentPrompt) -> Result<()> {
        let guard = self.ensure().await?;
        borrow(&guard)?.send(session, prompt).await
    }

    async fn respond_approval(
        &self,
        session: &AgentSession,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<()> {
        let guard = self.ensure().await?;
        borrow(&guard)?
            .respond_approval(session, request_id, decision)
            .await
    }

    async fn interrupt(&self, session: &AgentSession) -> Result<()> {
        let guard = self.ensure().await?;
        borrow(&guard)?.interrupt(session).await
    }

    async fn shutdown(&self, session: &AgentSession) -> Result<()> {
        let guard = self.ensure().await?;
        borrow(&guard)?.shutdown(session).await
    }
}
