//! Env-gated live `claude` CLI test. Skipped unless `TRAMA_CLAUDE_E2E=1`.

use std::time::Duration;

use tokio::time::timeout;
use trama_domain::agent::{AgentAdapter, AgentEvent, AgentPrompt, PermissionMode, SessionConfig};
use trama_infra::ClaudeAdapter;

fn e2e_enabled() -> bool {
    matches!(std::env::var("TRAMA_CLAUDE_E2E"), Ok(v) if v == "1")
}

#[tokio::test]
async fn claude_print_say_hi() {
    if !e2e_enabled() {
        eprintln!("skipping e2e; set TRAMA_CLAUDE_E2E=1 to run `claude -p \"say hi\"`");
        return;
    }

    let adapter = ClaudeAdapter::new().expect("detect claude binary");
    let (tx, rx) = async_channel::unbounded();
    let cwd = std::env::temp_dir();
    let config = SessionConfig {
        cwd,
        model: None,
        permission_mode: PermissionMode::Bypass,
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        extra_dirs: Vec::new(),
        mcp_config_path: None,
        append_system_prompt: Some("Reply with a single word.".into()),
        max_turns: Some(1),
        env: Default::default(),
        resume_external_session: None,
    };

    let session = adapter.start(config, tx).await.expect("start claude");
    adapter
        .send(
            &session,
            AgentPrompt {
                text: "say hi".into(),
            },
        )
        .await
        .expect("send prompt");

    let mut saw_text = false;
    let mut saw_complete = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Ok(AgentEvent::TextDelta { text })) if !text.is_empty() => saw_text = true,
            Ok(Ok(AgentEvent::TurnCompleted { .. })) => {
                saw_complete = true;
                break;
            }
            Ok(Ok(AgentEvent::Failed { message })) => {
                adapter.shutdown(&session).await.ok();
                panic!("claude failed: {message}");
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }

    adapter.shutdown(&session).await.expect("shutdown");
    assert!(saw_text, "expected at least one text delta from claude");
    assert!(saw_complete, "expected turn_completed from claude");
}
