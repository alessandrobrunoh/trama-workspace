//! In-process `AgentAdapter` that emits scripted events. No real `claude`.

use async_channel::Sender;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use trama_domain::agent::{
    AgentAdapter, AgentEvent, AgentKind, AgentPrompt, AgentSession, ApprovalDecision,
    SessionConfig, UsageReport,
};
use trama_domain::ids::AgentSessionId;
use trama_domain::{DomainError, Result};

const DEFAULT_CHUNKS: &[&str] = &["Hello", " from ", "the mock ", "agent."];

#[derive(Clone)]
pub struct FakeAdapter {
    events: Arc<Vec<AgentEvent>>,
    delay: Duration,
    fail_start: bool,
    pub start_configs: Arc<Mutex<Vec<SessionConfig>>>,
    pub prompts: Arc<Mutex<Vec<String>>>,
    interrupt: Arc<AtomicBool>,
    senders: Arc<Mutex<HashMap<AgentSessionId, Sender<AgentEvent>>>>,
}

impl FakeAdapter {
    pub fn new(events: Vec<AgentEvent>, delay: Duration) -> Self {
        Self {
            events: Arc::new(events),
            delay,
            fail_start: false,
            start_configs: Arc::new(Mutex::new(Vec::new())),
            prompts: Arc::new(Mutex::new(Vec::new())),
            interrupt: Arc::new(AtomicBool::new(false)),
            senders: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn scripted() -> Self {
        Self::new(scripted_events(), Duration::from_millis(35))
    }

    pub fn fail_start() -> Self {
        let mut fake = Self::scripted();
        fake.fail_start = true;
        fake
    }

    /// Test hook: drop every event sender so each open pump observes channel
    /// closure, simulating an adapter process that exits mid-turn without a
    /// terminal event.
    pub fn close_all_event_streams(&self) {
        self.senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

impl Default for FakeAdapter {
    fn default() -> Self {
        Self::scripted()
    }
}

pub fn scripted_events() -> Vec<AgentEvent> {
    let mut events = vec![AgentEvent::SessionStarted {
        external_session_id: Some("fake-ext-1".into()),
        model: Some("fake".into()),
    }];
    for chunk in DEFAULT_CHUNKS {
        events.push(AgentEvent::TextDelta {
            text: (*chunk).to_string(),
        });
    }
    events.push(AgentEvent::UsageReported {
        report: UsageReport {
            model: "fake".into(),
            input_tokens: 16,
            output_tokens: 8,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    events.push(AgentEvent::TurnCompleted {
        num_turns: 1,
        cost_usd_micros: Some(0),
    });
    events
}

#[async_trait::async_trait]
impl AgentAdapter for FakeAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Builtin
    }

    async fn start(
        &self,
        config: SessionConfig,
        events: Sender<AgentEvent>,
    ) -> Result<AgentSession> {
        if self.fail_start {
            return Err(DomainError::Agent(
                "Claude Code CLI not found. Set CLAUDE_BIN to the `claude` executable, \
                 or install Claude Code so `claude` is on PATH (https://code.claude.com)."
                    .into(),
            ));
        }
        self.start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(config.clone());
        let id = AgentSessionId::generate();
        self.senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), events);
        Ok(AgentSession {
            id,
            external_session_id: config.resume_external_session.clone(),
            model: config.model.clone(),
            cwd: config.cwd,
        })
    }

    async fn send(&self, session: &AgentSession, prompt: AgentPrompt) -> Result<()> {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(prompt.text);
        self.interrupt.store(false, Ordering::SeqCst);
        let tx = self
            .senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session.id)
            .cloned()
            .ok_or_else(|| DomainError::NotFound(format!("agent session {}", session.id)))?;
        let events = Arc::clone(&self.events);
        let interrupt = Arc::clone(&self.interrupt);
        let delay = self.delay;
        tokio::spawn(async move {
            for event in events.iter() {
                if interrupt.load(Ordering::SeqCst) {
                    return;
                }
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                if interrupt.load(Ordering::SeqCst) {
                    return;
                }
                if tx.send(event.clone()).await.is_err() {
                    return;
                }
            }
        });
        Ok(())
    }

    async fn respond_approval(
        &self,
        _session: &AgentSession,
        _request_id: &str,
        _decision: ApprovalDecision,
    ) -> Result<()> {
        Ok(())
    }

    async fn interrupt(&self, session: &AgentSession) -> Result<()> {
        self.interrupt.store(true, Ordering::SeqCst);
        let tx = self
            .senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session.id)
            .cloned();
        if let Some(tx) = tx {
            let _ = tx
                .send(AgentEvent::Failed {
                    message: "interrupted".into(),
                })
                .await;
        }
        Ok(())
    }

    async fn shutdown(&self, session: &AgentSession) -> Result<()> {
        self.senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&session.id);
        Ok(())
    }
}
