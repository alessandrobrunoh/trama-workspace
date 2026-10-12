//! What an agent did during a turn besides its final answer: tool calls
//! (input and output), thinking, and the built-in subagent lifecycle, in
//! stream order. Stored per turn next to the turn's answer so a transcript
//! keeps its tool views across restarts.
//!
//! Sizes are bounded: each JSON value keeps at most [`MAX_JSON_BYTES`]
//! (long strings lose their middle, so a command's output keeps its head
//! and its tail), a thinking block at most [`MAX_THINKING_BYTES`], and a
//! turn at most [`MAX_ITEMS`] items. Once a turn has stored
//! [`MAX_TURN_BYTES`], further values keep only [`SPENT_JSON_BYTES`].

use crate::agent::AgentEvent;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Largest serialized size of one tool input or output.
pub const MAX_JSON_BYTES: usize = 32 * 1024;
/// Largest thinking block (UTF-8 bytes).
pub const MAX_THINKING_BYTES: usize = 16 * 1024;
/// A turn's budget for inputs, outputs and thinking together.
pub const MAX_TURN_BYTES: usize = 1024 * 1024;
/// What each value keeps once the turn's budget is spent.
pub const SPENT_JSON_BYTES: usize = 1024;
/// Items kept per turn; later tool calls are counted, not stored.
pub const MAX_ITEMS: usize = 1000;

fn is_false(value: &bool) -> bool {
    !*value
}

/// One step of a turn. Serialized with a `kind` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityItem {
    /// Consecutive reasoning deltas, merged.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
    ToolCall {
        tool_use_id: String,
        name: String,
        input: Value,
        #[serde(default)]
        output: Option<Value>,
        #[serde(default)]
        is_error: bool,
        /// False when the turn ended (or is still running) before the call
        /// reported back.
        finished: bool,
        started_at: DateTime<Utc>,
        #[serde(default)]
        finished_at: Option<DateTime<Utc>>,
        /// The input or the output was cut to fit the size bounds.
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
    /// Claude Code's built-in subagent started (`Task` / `Agent`).
    SubagentStarted {
        parent_tool_use_id: Option<String>,
        label: Option<String>,
    },
    SubagentFinished {
        parent_tool_use_id: Option<String>,
    },
}

/// Accumulates a turn's activity from the normalized event stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivityLog {
    items: Vec<ActivityItem>,
    bytes: usize,
    dropped: u32,
}

impl ActivityLog {
    /// Record `event` (received at `at`). Returns whether anything changed;
    /// events that are not activity (text, usage, approvals) are ignored.
    pub fn apply(&mut self, event: &AgentEvent, at: DateTime<Utc>) -> bool {
        match event {
            AgentEvent::ReasoningDelta { text } => self.push_thinking(text),
            AgentEvent::ToolCallStarted {
                tool_use_id,
                name,
                input_json,
            } => {
                if !self.has_room() {
                    return false;
                }
                let (input, truncated) = bound_json(input_json, self.value_limit());
                self.bytes += json_len(&input);
                self.items.push(ActivityItem::ToolCall {
                    tool_use_id: tool_use_id.clone(),
                    name: name.clone(),
                    input,
                    output: None,
                    is_error: false,
                    finished: false,
                    started_at: at,
                    finished_at: None,
                    truncated,
                });
                true
            }
            AgentEvent::ToolCallFinished {
                tool_use_id,
                output_json,
                is_error,
            } => {
                let limit = self.value_limit();
                let Some(ActivityItem::ToolCall {
                    output,
                    is_error: error,
                    finished,
                    finished_at,
                    truncated,
                    ..
                }) = self.items.iter_mut().rev().find(|item| {
                    matches!(item, ActivityItem::ToolCall { tool_use_id: id, .. } if id == tool_use_id)
                })
                else {
                    return false;
                };
                let (bounded, cut) = bound_json(output_json, limit);
                self.bytes += json_len(&bounded);
                *output = Some(bounded);
                *error = *is_error;
                *finished = true;
                *finished_at = Some(at);
                *truncated |= cut;
                true
            }
            AgentEvent::SubagentStarted {
                parent_tool_use_id,
                label,
            } => {
                if !self.has_room() {
                    return false;
                }
                self.items.push(ActivityItem::SubagentStarted {
                    parent_tool_use_id: parent_tool_use_id.clone(),
                    label: label.as_deref().map(|label| cut_middle(label, 512)),
                });
                true
            }
            AgentEvent::SubagentFinished { parent_tool_use_id } => {
                if !self.has_room() {
                    return false;
                }
                self.items.push(ActivityItem::SubagentFinished {
                    parent_tool_use_id: parent_tool_use_id.clone(),
                });
                true
            }
            _ => false,
        }
    }

    fn has_room(&mut self) -> bool {
        if self.items.len() < MAX_ITEMS {
            true
        } else {
            self.dropped = self.dropped.saturating_add(1);
            false
        }
    }

    fn value_limit(&self) -> usize {
        if self.bytes >= MAX_TURN_BYTES {
            SPENT_JSON_BYTES
        } else {
            MAX_JSON_BYTES
        }
    }

    fn push_thinking(&mut self, delta: &str) -> bool {
        if delta.is_empty() {
            return false;
        }
        if !matches!(self.items.last(), Some(ActivityItem::Thinking { .. })) {
            if !self.has_room() {
                return false;
            }
            self.items.push(ActivityItem::Thinking {
                text: String::new(),
                truncated: false,
            });
        }
        let cap = if self.bytes >= MAX_TURN_BYTES {
            SPENT_JSON_BYTES
        } else {
            MAX_THINKING_BYTES
        };
        let Some(ActivityItem::Thinking { text, truncated }) = self.items.last_mut() else {
            return false;
        };
        if *truncated {
            return false;
        }
        let room = cap.saturating_sub(text.len());
        if delta.len() <= room {
            text.push_str(delta);
            self.bytes += delta.len();
        } else {
            let end = floor_boundary(delta, room);
            text.push_str(&delta[..end]);
            self.bytes += end;
            *truncated = true;
        }
        true
    }

    pub fn items(&self) -> &[ActivityItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Tool calls (and subagent events) not stored because the turn had
    /// [`MAX_ITEMS`] items already.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    pub fn into_items(self) -> Vec<ActivityItem> {
        self.items
    }
}

/// Serialized size of `value` in bytes.
pub fn json_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

/// `value` cut to at most about `max` serialized bytes, and whether it was
/// cut. Long strings lose their middle first (halving the per-string cap
/// until it fits); a value that still does not fit (many small fields)
/// becomes a string preview of its JSON text.
pub fn bound_json(value: &Value, max: usize) -> (Value, bool) {
    if json_len(value) <= max {
        return (value.clone(), false);
    }
    let mut cap = max / 2;
    while cap >= 64 {
        let cut = cut_strings(value, cap);
        if json_len(&cut) <= max {
            return (cut, true);
        }
        cap /= 2;
    }
    let text = serde_json::to_string(value).unwrap_or_default();
    // Escaping can grow the text again when it is serialized as a string.
    let mut budget = max.saturating_sub(64);
    loop {
        let preview = Value::String(cut_middle(&text, budget));
        if json_len(&preview) <= max || budget < 64 {
            return (preview, true);
        }
        budget /= 2;
    }
}

fn cut_strings(value: &Value, cap: usize) -> Value {
    match value {
        Value::String(text) => Value::String(cut_middle(text, cap)),
        Value::Array(items) => Value::Array(items.iter().map(|v| cut_strings(v, cap)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, v)| (key.clone(), cut_strings(v, cap)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `text` with its middle replaced by a marker when longer than `max`
/// bytes: the head and the tail survive (a command's output tail matters).
pub fn cut_middle(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let keep = max / 2;
    let head = floor_boundary(text, keep);
    let tail = ceil_boundary(text, text.len() - keep);
    format!(
        "{}\n… [{} bytes omitted] …\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn started(id: &str, name: &str, input: Value) -> AgentEvent {
        AgentEvent::ToolCallStarted {
            tool_use_id: id.into(),
            name: name.into(),
            input_json: input,
        }
    }

    fn finished(id: &str, output: Value, is_error: bool) -> AgentEvent {
        AgentEvent::ToolCallFinished {
            tool_use_id: id.into(),
            output_json: output,
            is_error,
        }
    }

    #[test]
    fn events_become_items_in_stream_order() {
        let mut log = ActivityLog::default();
        let t0 = Utc::now();
        assert!(log.apply(
            &AgentEvent::ReasoningDelta {
                text: "plan ".into()
            },
            t0
        ));
        assert!(log.apply(&AgentEvent::ReasoningDelta { text: "it".into() }, t0));
        assert!(log.apply(&started("t1", "Bash", json!({"command": "ls"})), t0));
        assert!(!log.apply(&AgentEvent::TextDelta { text: "x".into() }, t0));
        let t1 = t0 + chrono::Duration::milliseconds(1500);
        assert!(log.apply(&finished("t1", json!("a\nb"), false), t1));
        assert!(!log.apply(&finished("unknown", json!(null), false), t1));
        assert!(log.apply(
            &AgentEvent::SubagentStarted {
                parent_tool_use_id: Some("t2".into()),
                label: Some("explore".into())
            },
            t1
        ));
        let items = log.into_items();
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0],
            ActivityItem::Thinking {
                text: "plan it".into(),
                truncated: false
            }
        );
        match &items[1] {
            ActivityItem::ToolCall {
                output,
                finished,
                started_at,
                finished_at,
                truncated,
                ..
            } => {
                assert_eq!(output, &Some(json!("a\nb")));
                assert!(*finished && !*truncated);
                assert_eq!(*started_at, t0);
                assert_eq!(*finished_at, Some(t1));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn large_values_keep_head_and_tail_within_the_bound() {
        let output = format!("HEAD{}TAIL", "x".repeat(200_000));
        let (bounded, cut) = bound_json(&json!({"stdout": output, "code": 0}), MAX_JSON_BYTES);
        assert!(cut);
        assert!(json_len(&bounded) <= MAX_JSON_BYTES);
        let stdout = bounded["stdout"].as_str().unwrap();
        assert!(stdout.starts_with("HEAD") && stdout.ends_with("TAIL"));
        assert!(stdout.contains("bytes omitted"));
        assert_eq!(bounded["code"], 0);

        // Many small fields: a JSON preview instead.
        let many: Vec<Value> = (0..20_000).map(|n| json!(n)).collect();
        let (bounded, cut) = bound_json(&Value::Array(many), 2048);
        assert!(cut && bounded.is_string() && json_len(&bounded) <= 2048);

        // Multibyte text is cut on character boundaries.
        let text = "é".repeat(5000);
        let cut = cut_middle(&text, 101);
        assert!(cut.contains("omitted"));
    }

    #[test]
    fn thinking_and_items_are_capped() {
        let mut log = ActivityLog::default();
        let now = Utc::now();
        let chunk = "y".repeat(10_000);
        for _ in 0..3 {
            log.apply(
                &AgentEvent::ReasoningDelta {
                    text: chunk.clone(),
                },
                now,
            );
        }
        match &log.items()[0] {
            ActivityItem::Thinking { text, truncated } => {
                assert_eq!(text.len(), MAX_THINKING_BYTES);
                assert!(truncated);
            }
            other => panic!("{other:?}"),
        }
        for n in 0..MAX_ITEMS + 5 {
            log.apply(&started(&format!("t{n}"), "Read", json!({})), now);
        }
        assert_eq!(log.items().len(), MAX_ITEMS);
        assert_eq!(log.dropped(), 6);
    }

    #[test]
    fn a_spent_budget_shrinks_later_values() {
        let mut log = ActivityLog::default();
        let now = Utc::now();
        let big = json!("z".repeat(MAX_JSON_BYTES * 4));
        let mut n = 0;
        while log.bytes < MAX_TURN_BYTES {
            log.apply(&started(&format!("t{n}"), "Read", big.clone()), now);
            n += 1;
        }
        log.apply(&started("late", "Read", big), now);
        match log.items().last().unwrap() {
            ActivityItem::ToolCall {
                input, truncated, ..
            } => {
                assert!(*truncated);
                assert!(json_len(input) <= SPENT_JSON_BYTES);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn json_shape_is_tagged_and_compact() {
        let item = ActivityItem::Thinking {
            text: "t".into(),
            truncated: false,
        };
        assert_eq!(
            serde_json::to_value(&item).unwrap(),
            json!({"kind": "thinking", "text": "t"})
        );
        let call: ActivityItem = serde_json::from_value(json!({
            "kind": "tool_call",
            "tool_use_id": "t1",
            "name": "Bash",
            "input": {"command": "ls"},
            "finished": false,
            "started_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        assert!(matches!(call, ActivityItem::ToolCall { output: None, .. }));
    }
}
