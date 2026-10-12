//! Pure translation from [`ClaudeLine`](super::protocol::ClaudeLine) to
//! [`AgentEvent`].
//!
//! Stateful only where the wire protocol is redundant: `--include-partial-messages`
//! emits both `stream_event` deltas and a completed `assistant` message. Text
//! and thinking already streamed are not replayed from the buffered block.
//! Tool calls are taken from the completed `tool_use` block (not from
//! `input_json_delta` fragments).

use std::collections::HashSet;

use trama_domain::agent::{AgentEvent, BillingSource, UsageReport};

use super::protocol::{
    AssistantLine, ClaudeLine, ClaudeUsage, ContentBlock, MessageContent, ResultLine,
    StreamEventLine, SystemLine, UserLine, usd_to_micros,
};

/// Per-session translator state (still unit-testable: feed lines in order).
#[derive(Debug, Default)]
pub struct TranslateState {
    /// Last known model id, used to populate [`UsageReport::model`].
    pub model: Option<String>,
    streamed_text: bool,
    streamed_thinking: bool,
    active_subagents: HashSet<String>,
}

/// Translate a single line with a fresh state. Prefer [`translate_lines`] for
/// a real stream (subagent + stream/buffer dedup need order).
pub fn translate_line(line: &ClaudeLine) -> Vec<AgentEvent> {
    let mut state = TranslateState::default();
    translate_line_in(line, &mut state)
}

/// Translate an ordered NDJSON stream into normalized events.
pub fn translate_lines<'a, I>(lines: I) -> Vec<AgentEvent>
where
    I: IntoIterator<Item = &'a ClaudeLine>,
{
    let mut state = TranslateState::default();
    let mut out = Vec::new();
    for line in lines {
        out.extend(translate_line_in(line, &mut state));
    }
    out
}

pub fn translate_line_in(line: &ClaudeLine, state: &mut TranslateState) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    note_parent(line.parent_tool_use_id(), state, &mut events);

    match line {
        ClaudeLine::System(sys) => translate_system(sys, state, &mut events),
        ClaudeLine::Assistant(asst) => translate_assistant(asst, state, &mut events),
        ClaudeLine::User(user) => translate_user(user, state, &mut events),
        ClaudeLine::StreamEvent(stream) => translate_stream(stream, &mut events, state),
        ClaudeLine::Result(result) => translate_result(result, state, &mut events),
        ClaudeLine::Unknown => {}
    }

    events
}

fn note_parent(parent: Option<&str>, state: &mut TranslateState, events: &mut Vec<AgentEvent>) {
    let Some(id) = parent else {
        return;
    };
    if id.is_empty() {
        return;
    }
    if state.active_subagents.insert(id.to_string()) {
        events.push(AgentEvent::SubagentStarted {
            parent_tool_use_id: Some(id.to_string()),
            label: None,
        });
    }
}

fn translate_system(sys: &SystemLine, state: &mut TranslateState, events: &mut Vec<AgentEvent>) {
    if sys.subtype != "init" {
        return;
    }
    if sys.model.is_some() {
        state.model = sys.model.clone();
    }
    events.push(AgentEvent::SessionStarted {
        external_session_id: sys.session_id.clone(),
        model: sys.model.clone(),
    });
    events.push(AgentEvent::SessionInfo {
        billing: billing_source(sys.api_key_source.as_deref()),
        api_key_source: sys.api_key_source.clone(),
        agent_version: sys.claude_code_version.clone(),
        permission_mode: sys.permission_mode.clone(),
    });
}

/// Map Claude Code's `apiKeySource` to what the session bills against.
/// `"none"` means no API key is in use, so requests go through the
/// subscription login. Anything else that names a key source is API billing.
/// A missing or unrecognized value is reported as unknown rather than
/// guessed.
pub fn billing_source(api_key_source: Option<&str>) -> BillingSource {
    match api_key_source.map(str::trim) {
        Some("none") => BillingSource::Subscription,
        Some("ANTHROPIC_API_KEY" | "apiKeyHelper" | "/login managed key") => BillingSource::ApiKey,
        _ => BillingSource::Unknown,
    }
}

fn translate_assistant(
    asst: &AssistantLine,
    state: &mut TranslateState,
    events: &mut Vec<AgentEvent>,
) {
    if asst.message.model.is_some() {
        state.model = asst.message.model.clone();
    }

    match &asst.message.content {
        MessageContent::Text(text) if !text.is_empty() && !state.streamed_text => {
            events.push(AgentEvent::TextDelta { text: text.clone() });
        }
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text { text } if !text.is_empty() && !state.streamed_text => {
                        events.push(AgentEvent::TextDelta { text: text.clone() });
                    }
                    ContentBlock::Thinking { thinking, text } if !state.streamed_thinking => {
                        let body = if !thinking.is_empty() {
                            thinking.as_str()
                        } else {
                            text.as_str()
                        };
                        if !body.is_empty() {
                            events.push(AgentEvent::ReasoningDelta {
                                text: body.to_string(),
                            });
                        }
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        if id.is_empty() && name.is_empty() {
                            continue;
                        }
                        events.push(AgentEvent::ToolCallStarted {
                            tool_use_id: id.clone(),
                            name: name.clone(),
                            input_json: input.clone(),
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    if let Some(usage) = asst.message.usage.as_ref() {
        events.push(usage_event(usage, state));
    }

    state.streamed_text = false;
    state.streamed_thinking = false;
}

fn translate_user(user: &UserLine, state: &mut TranslateState, events: &mut Vec<AgentEvent>) {
    for block in user.message.content.blocks() {
        if let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = block
        {
            if tool_use_id.is_empty() {
                continue;
            }
            if state.active_subagents.remove(tool_use_id) {
                events.push(AgentEvent::SubagentFinished {
                    parent_tool_use_id: Some(tool_use_id.clone()),
                });
            }
            events.push(AgentEvent::ToolCallFinished {
                tool_use_id: tool_use_id.clone(),
                output_json: content.clone(),
                is_error: *is_error,
            });
        }
    }
}

fn translate_stream(
    stream: &StreamEventLine,
    events: &mut Vec<AgentEvent>,
    state: &mut TranslateState,
) {
    if stream.event.event_type != "content_block_delta" {
        return;
    }
    let Some(delta) = stream.event.delta.as_ref() else {
        return;
    };
    match delta.delta_type.as_str() {
        "text_delta" => {
            if let Some(text) = delta.text.as_ref().filter(|t| !t.is_empty()) {
                state.streamed_text = true;
                events.push(AgentEvent::TextDelta { text: text.clone() });
            }
        }
        "thinking_delta" => {
            if let Some(text) = delta.thinking.as_ref().filter(|t| !t.is_empty()) {
                state.streamed_thinking = true;
                events.push(AgentEvent::ReasoningDelta { text: text.clone() });
            }
        }
        // Parsed for protocol coverage; tool calls are emitted from the
        // completed `assistant` `tool_use` block so the input is whole JSON.
        "input_json_delta" => {}
        _ => {}
    }
}

const NOT_LOGGED_IN_HINT: &str = "Claude Code is not logged in for the user running the \
     Trama daemon. Run `claude` in a terminal and use /login (or `claude auth login`), then \
     retry. If the daemon was started with a different HOME, start it with your real HOME and \
     use TRAMA_DATA_DIR to isolate Trama's data instead.";

/// Replace the CLI's terse login error with an actionable one; keep others as-is.
fn explain_failure(message: String) -> String {
    if message.contains("Not logged in") {
        format!("{NOT_LOGGED_IN_HINT}\n\nClaude Code said: {message}")
    } else {
        message
    }
}

fn translate_result(result: &ResultLine, state: &mut TranslateState, events: &mut Vec<AgentEvent>) {
    if let Some(usage) = result.usage.as_ref() {
        events.push(usage_event(usage, state));
    }

    let failed = result.is_error || result.subtype == "error";
    if failed {
        let message = result
            .error
            .clone()
            .or_else(|| result.result.clone())
            .unwrap_or_else(|| "claude result error".to_string());
        events.push(AgentEvent::Failed {
            message: explain_failure(message),
        });
    }

    events.push(AgentEvent::TurnCompleted {
        num_turns: result.num_turns,
        cost_usd_micros: result.total_cost_usd.map(usd_to_micros),
    });

    state.streamed_text = false;
    state.streamed_thinking = false;
}

fn usage_event(usage: &ClaudeUsage, state: &TranslateState) -> AgentEvent {
    AgentEvent::UsageReported {
        report: UsageReport {
            model: state.model.clone().unwrap_or_else(|| "unknown".to_string()),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_input_tokens,
            cache_write_tokens: usage.cache_creation_input_tokens,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::protocol::parse_ndjson;

    #[test]
    fn billing_source_maps_known_api_key_sources() {
        assert_eq!(billing_source(Some("none")), BillingSource::Subscription);
        assert_eq!(
            billing_source(Some("ANTHROPIC_API_KEY")),
            BillingSource::ApiKey
        );
        assert_eq!(billing_source(Some("apiKeyHelper")), BillingSource::ApiKey);
        assert_eq!(
            billing_source(Some("/login managed key")),
            BillingSource::ApiKey
        );
        assert_eq!(billing_source(Some("future")), BillingSource::Unknown);
        assert_eq!(billing_source(None), BillingSource::Unknown);
    }

    #[test]
    fn not_logged_in_result_explains_how_to_log_in() {
        let ndjson = r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login","num_turns":1}"#;
        let lines = parse_ndjson(ndjson).unwrap();
        let events = translate_lines(&lines);
        let Some(AgentEvent::Failed { message }) = events.first() else {
            panic!("expected Failed, got {events:?}");
        };
        assert!(message.starts_with("Claude Code is not logged in"));
        assert!(message.contains("TRAMA_DATA_DIR"));
        assert!(message.ends_with("Claude Code said: Not logged in · Please run /login"));
    }

    #[test]
    fn parent_tool_use_id_emits_subagent_lifecycle() {
        let ndjson = r#"
{"type":"assistant","parent_tool_use_id":"toolu_task","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_task","content":"done"}]}}
"#;
        let lines = parse_ndjson(ndjson).unwrap();
        let events = translate_lines(&lines);
        assert_eq!(
            serde_json::to_value(&events).unwrap(),
            serde_json::json!([
                {
                    "type": "subagent_started",
                    "parent_tool_use_id": "toolu_task",
                    "label": null
                },
                { "type": "text_delta", "text": "working" },
                {
                    "type": "subagent_finished",
                    "parent_tool_use_id": "toolu_task"
                },
                {
                    "type": "tool_call_finished",
                    "tool_use_id": "toolu_task",
                    "output_json": "done",
                    "is_error": false
                }
            ])
        );
    }
}
