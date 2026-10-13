//! UI-facing agent used by the desktop chat panel.
//!
//! This is not [`trama_domain::agent::AgentAdapter`]. The domain trait is a
//! session-oriented port for adapters; the shell only needs a prompt in and a
//! stream of [`AgentEvent`]s out.

use std::thread;
use std::time::Duration;

use async_channel::Receiver;
use trama_domain::agent::{AgentEvent, UsageReport};

/// Prompt → event stream. Implemented by [`FakeAgent`] when the daemon is
/// offline or no thread is selected.
pub trait ChatAgent {
    fn send(&mut self, prompt: String) -> Receiver<AgentEvent>;
}

/// Scripted agent that emits a realistic turn so the UI can exercise streaming
/// markdown, thinking, folded tool calls, an edit diff, a command block,
/// usage, and completion.
pub struct FakeAgent;

impl ChatAgent for FakeAgent {
    fn send(&mut self, prompt: String) -> Receiver<AgentEvent> {
        let (tx, rx) = async_channel::unbounded();
        thread::spawn(move || {
            let emit = |event: AgentEvent| {
                let _ = tx.send_blocking(event);
            };

            emit(AgentEvent::SessionStarted {
                external_session_id: Some("fake-session".into()),
                model: Some("claude-sonnet-5-5".into()),
            });
            thread::sleep(Duration::from_millis(80));

            let answer = scripted_markdown(&prompt);
            for chunk in chunk_text(&answer, 28) {
                emit(AgentEvent::TextDelta { text: chunk });
                thread::sleep(Duration::from_millis(28));
            }

            emit(AgentEvent::ReasoningDelta {
                text: "The session path lives in auth.rs; check how tokens are issued first."
                    .into(),
            });
            let tool = |id: &str,
                        name: &str,
                        input: serde_json::Value,
                        output: serde_json::Value,
                        is_error: bool,
                        ms: u64| {
                emit(AgentEvent::ToolCallStarted {
                    tool_use_id: id.into(),
                    name: name.into(),
                    input_json: input,
                });
                thread::sleep(Duration::from_millis(ms));
                emit(AgentEvent::ToolCallFinished {
                    tool_use_id: id.into(),
                    output_json: output,
                    is_error,
                });
                thread::sleep(Duration::from_millis(40));
            };
            tool(
                "toolu_grep_1",
                "Grep",
                serde_json::json!({"pattern": "fn login", "path": "backend/src"}),
                serde_json::json!("backend/src/auth.rs:42:fn login(user: &User) -> Session {"),
                false,
                120,
            );
            tool(
                "toolu_read_1",
                "Read",
                serde_json::json!({"file_path": "backend/src/auth.rs"}),
                serde_json::json!(
                    "    42→fn login(user: &User) -> Session {\n    43→    todo!()\n    44→}"
                ),
                false,
                90,
            );
            tool(
                "toolu_edit_1",
                "Edit",
                serde_json::json!({
                    "file_path": "backend/src/auth.rs",
                    "old_string": "fn login(user: &User) -> Session {\n    todo!()\n}",
                    "new_string": "fn login(user: &User) -> Session {\n    let token = issue_token(user.id);\n    Session::new(token)\n}"
                }),
                serde_json::json!(
                    "The file backend/src/auth.rs has been updated. Here's the result of running `cat -n` on a snippet of the edited file:\n    42→fn login(user: &User) -> Session {\n    43→    let token = issue_token(user.id);"
                ),
                false,
                220,
            );
            tool(
                "toolu_bash_1",
                "Bash",
                serde_json::json!({"command": "cargo test -p backend auth", "description": "Run the auth tests"}),
                serde_json::json!(format!(
                    "{}\n\u{1b}[32mtest result: ok.\u{1b}[0m 14 passed; 0 failed",
                    (1..=14)
                        .map(|n| format!("test auth::tests::case_{n} ... \u{1b}[32mok\u{1b}[0m"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )),
                false,
                640,
            );
            for chunk in chunk_text(
                "\n\nThe `Edit` landed. Next we can wire the frontend login form to this endpoint.\n",
                32,
            ) {
                emit(AgentEvent::TextDelta { text: chunk });
                thread::sleep(Duration::from_millis(24));
            }

            emit(AgentEvent::UsageReported {
                report: UsageReport {
                    model: "claude-sonnet-5-5".into(),
                    input_tokens: 1_240,
                    output_tokens: 386,
                    cache_read_tokens: 8_192,
                    cache_write_tokens: 256,
                },
            });
            thread::sleep(Duration::from_millis(40));

            emit(AgentEvent::TurnCompleted {
                num_turns: 1,
                cost_usd_micros: Some(12_400),
            });
        });
        rx
    }
}

fn scripted_markdown(prompt: &str) -> String {
    let preview = prompt.trim();
    let preview = if preview.is_empty() {
        "your request".to_string()
    } else if preview.len() > 80 {
        format!("{}…", &preview[..80])
    } else {
        preview.to_string()
    };

    format!(
        "\
## Authentication plan

I read **{preview}** and would start on the backend session path, then the frontend login form.

### Token helper

```rust
fn issue_token(user_id: Uuid) -> Session {{
    Session {{
        user_id,
        expires_at: Utc::now() + Duration::hours(24),
    }}
}}
```

### Checklist

- Validate credentials against the user store
- Issue a signed session token
- Persist a rotating refresh token
- Return the session payload to the frontend
"
    )
}

fn chunk_text(text: &str, size: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = size.min(rest.len());
        while end < rest.len() && !rest.is_char_boundary(end) {
            end += 1;
        }
        chunks.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    chunks
}
