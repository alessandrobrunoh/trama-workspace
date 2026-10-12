//! Parse Claude Code NDJSON fixtures and assert the translated `AgentEvent` stream.

use trama_infra::claude::protocol::parse_ndjson;
use trama_infra::claude::translate::translate_lines;

fn load(name: &str) -> &'static str {
    match name {
        "simple_text.ndjson" => include_str!("fixtures/simple_text.ndjson"),
        "tool_use.ndjson" => include_str!("fixtures/tool_use.ndjson"),
        "stream_events.ndjson" => include_str!("fixtures/stream_events.ndjson"),
        "init_subscription.ndjson" => include_str!("fixtures/init_subscription.ndjson"),
        "init_api_key.ndjson" => include_str!("fixtures/init_api_key.ndjson"),
        other => panic!("unknown fixture {other}"),
    }
}

fn events_json(name: &str) -> serde_json::Value {
    let lines = parse_ndjson(load(name)).expect("parse fixture");
    serde_json::to_value(translate_lines(&lines)).unwrap()
}

#[test]
fn simple_text_turn() {
    assert_eq!(
        events_json("simple_text.ndjson"),
        serde_json::json!([
            {
                "type": "session_started",
                "external_session_id": "ses_simple",
                "model": "claude-opus-4-6"
            },
            {
                "type": "session_info",
                "billing": "subscription",
                "api_key_source": "none",
                "agent_version": "2.1.296",
                "permission_mode": "default"
            },
            {
                "type": "commands_reported",
                "slash_commands": ["compact"],
                "skills": []
            },
            { "type": "text_delta", "text": "Hello from Claude." },
            {
                "type": "usage_reported",
                "report": {
                    "model": "claude-opus-4-6",
                    "input_tokens": 12,
                    "output_tokens": 8,
                    "cache_read_tokens": 3,
                    "cache_write_tokens": 0
                }
            },
            {
                "type": "usage_reported",
                "report": {
                    "model": "claude-opus-4-6",
                    "input_tokens": 12,
                    "output_tokens": 8,
                    "cache_read_tokens": 3,
                    "cache_write_tokens": 0
                }
            },
            {
                "type": "turn_completed",
                "num_turns": 1,
                "cost_usd_micros": 2500
            }
        ])
    );
}

#[test]
fn tool_use_and_result_turn() {
    assert_eq!(
        events_json("tool_use.ndjson"),
        serde_json::json!([
            {
                "type": "session_started",
                "external_session_id": "ses_tools",
                "model": "claude-sonnet-4-6"
            },
            {
                "type": "session_info",
                "billing": "unknown",
                "api_key_source": null,
                "agent_version": null,
                "permission_mode": "acceptEdits"
            },
            {
                "type": "tool_call_started",
                "tool_use_id": "toolu_01Read",
                "name": "Read",
                "input_json": { "file_path": "/repo/README.md" }
            },
            {
                "type": "usage_reported",
                "report": {
                    "model": "claude-sonnet-4-6",
                    "input_tokens": 40,
                    "output_tokens": 16,
                    "cache_read_tokens": 2,
                    "cache_write_tokens": 10
                }
            },
            {
                "type": "tool_call_finished",
                "tool_use_id": "toolu_01Read",
                "output_json": "# TramaWorkspace\nScaffold.",
                "is_error": false
            },
            { "type": "text_delta", "text": "The README is a scaffold." },
            {
                "type": "usage_reported",
                "report": {
                    "model": "claude-sonnet-4-6",
                    "input_tokens": 55,
                    "output_tokens": 12,
                    "cache_read_tokens": 20,
                    "cache_write_tokens": 10
                }
            },
            {
                "type": "usage_reported",
                "report": {
                    "model": "claude-sonnet-4-6",
                    "input_tokens": 95,
                    "output_tokens": 28,
                    "cache_read_tokens": 22,
                    "cache_write_tokens": 10
                }
            },
            {
                "type": "turn_completed",
                "num_turns": 2,
                "cost_usd_micros": 12300
            }
        ])
    );
}

#[test]
fn stream_event_partial_deltas() {
    assert_eq!(
        events_json("stream_events.ndjson"),
        serde_json::json!([
            {
                "type": "session_started",
                "external_session_id": "ses_stream",
                "model": "claude-opus-4-6"
            },
            {
                "type": "session_info",
                "billing": "unknown",
                "api_key_source": null,
                "agent_version": null,
                "permission_mode": "plan"
            },
            { "type": "reasoning_delta", "text": "Considering the answer." },
            { "type": "text_delta", "text": "Hel" },
            { "type": "text_delta", "text": "lo" },
            {
                "type": "usage_reported",
                "report": {
                    "model": "claude-opus-4-6",
                    "input_tokens": 8,
                    "output_tokens": 9,
                    "cache_read_tokens": 0,
                    "cache_write_tokens": 0
                }
            },
            {
                "type": "usage_reported",
                "report": {
                    "model": "claude-opus-4-6",
                    "input_tokens": 8,
                    "output_tokens": 9,
                    "cache_read_tokens": 0,
                    "cache_write_tokens": 0
                }
            },
            {
                "type": "turn_completed",
                "num_turns": 1,
                "cost_usd_micros": 1000
            }
        ])
    );
}

/// Captured from Claude Code 2.1.296 logged in with a subscription (paths
/// and ids redacted). Hook lifecycle and `permission_denied` system lines
/// precede/follow `init` and must not produce events.
#[test]
fn subscription_init_reports_subscription_billing() {
    assert_eq!(
        events_json("init_subscription.ndjson"),
        serde_json::json!([
            {
                "type": "session_started",
                "external_session_id": "ses_sub",
                "model": "claude-haiku-5-5"
            },
            {
                "type": "session_info",
                "billing": "subscription",
                "api_key_source": "none",
                "agent_version": "2.1.296",
                "permission_mode": "default"
            },
            {
                "type": "commands_reported",
                "slash_commands": ["compact"],
                "skills": []
            }
        ])
    );
}

#[test]
fn api_key_init_reports_api_billing() {
    assert_eq!(
        events_json("init_api_key.ndjson"),
        serde_json::json!([
            {
                "type": "session_started",
                "external_session_id": "ses_key",
                "model": "claude-sonnet-4-6"
            },
            {
                "type": "session_info",
                "billing": "api_key",
                "api_key_source": "ANTHROPIC_API_KEY",
                "agent_version": "2.1.296",
                "permission_mode": "acceptEdits"
            }
        ])
    );
}

#[test]
fn fixtures_parse_every_line() {
    for name in [
        "simple_text.ndjson",
        "tool_use.ndjson",
        "stream_events.ndjson",
        "init_subscription.ndjson",
        "init_api_key.ndjson",
    ] {
        let raw = load(name);
        let expected = raw.lines().filter(|l| !l.trim().is_empty()).count();
        let parsed = parse_ndjson(&raw).unwrap();
        assert_eq!(parsed.len(), expected, "{name}");
    }
}
