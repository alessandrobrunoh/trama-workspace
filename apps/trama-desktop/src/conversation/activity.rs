//! Replayed turn activity: the daemon stores each turn's tool calls and
//! thinking (`DaemonMessage::TurnActivity`) and replays them on subscribe,
//! so the tool views of [`crate::conversation::tools`] come back after a restart. This
//! module turns the stored items into transcript entries.

use std::collections::HashSet;

use trama_protocol::ActivityItem;

use crate::conversation::chat::ChatEntry;
use crate::conversation::tools::ToolTiming;

/// Entries (and tool timings by `tool_use_id`) for one replayed turn.
#[derive(Default)]
pub struct Replayed {
    pub entries: Vec<ChatEntry>,
    pub timings: Vec<(String, ToolTiming)>,
}

/// Transcript entries for `items`, skipping tool calls already shown
/// (`shown` holds their ids). A stored turn is over: a call that never
/// reported back is shown as cut off (finished, failed, no output). The
/// turn in flight (`in_flight`) keeps such calls running so live events can
/// finish them. Subagent lifecycle items have no row of their own: the
/// `Task` / `Agent` call that started the subagent is the row.
pub fn replay_entries(
    items: &[ActivityItem],
    in_flight: bool,
    shown: &HashSet<String>,
) -> Replayed {
    let mut out = Replayed::default();
    for item in items {
        match item {
            ActivityItem::Thinking { text, truncated } => {
                if text.is_empty() {
                    continue;
                }
                let mut text = text.clone();
                if *truncated {
                    text.push_str(" …");
                }
                match out.entries.last_mut() {
                    Some(ChatEntry::Thinking { text: existing }) => existing.push_str(&text),
                    _ => out.entries.push(ChatEntry::Thinking { text }),
                }
            }
            ActivityItem::ToolCall {
                tool_use_id,
                name,
                input,
                output,
                is_error,
                finished,
                started_at,
                finished_at,
                ..
            } => {
                if shown.contains(tool_use_id) {
                    continue;
                }
                let cut_off = !in_flight && !*finished;
                out.entries.push(ChatEntry::ToolCall {
                    tool_use_id: tool_use_id.clone(),
                    name: name.clone(),
                    input_json: input.clone(),
                    finished: *finished || cut_off,
                    is_error: *is_error || (cut_off && output.is_none()),
                    expanded: false,
                    output: output.clone(),
                });
                out.timings.push((
                    tool_use_id.clone(),
                    ToolTiming {
                        started_ms: started_at.timestamp_millis(),
                        finished_ms: finished_at.map(|at| at.timestamp_millis()),
                    },
                ));
            }
            ActivityItem::SubagentStarted { .. } | ActivityItem::SubagentFinished { .. } => {}
        }
    }
    out
}

/// Ids of the tool calls already in `entries`.
pub fn shown_tool_ids(entries: &[ChatEntry]) -> HashSet<String> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            ChatEntry::ToolCall { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::tools::{self, Item, ToolKind};
    use chrono::{DateTime, Utc};
    use serde_json::json;

    fn at(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).unwrap()
    }

    fn call(id: &str, name: &str, input: serde_json::Value, finished: bool) -> ActivityItem {
        ActivityItem::ToolCall {
            tool_use_id: id.into(),
            name: name.into(),
            input,
            output: finished.then(|| json!("ok")),
            is_error: false,
            finished,
            started_at: at(1_000),
            finished_at: finished.then(|| at(3_500)),
            truncated: false,
        }
    }

    fn items() -> Vec<ActivityItem> {
        vec![
            ActivityItem::Thinking {
                text: "plan".into(),
                truncated: false,
            },
            call("r1", "Read", json!({"file_path": "a.rs"}), true),
            call(
                "e1",
                "Edit",
                json!({"file_path": "a.rs", "old_string": "a", "new_string": "b"}),
                true,
            ),
            call("b1", "Bash", json!({"command": "cargo test"}), false),
            ActivityItem::SubagentStarted {
                parent_tool_use_id: None,
                label: None,
            },
        ]
    }

    #[test]
    fn stored_activity_rebuilds_the_tool_views() {
        let replayed = replay_entries(&items(), false, &HashSet::new());
        assert_eq!(replayed.entries.len(), 4, "subagent items have no row");
        // Thinking and the read fold into a summary; the edit and the
        // command get their own views, as when they streamed.
        assert_eq!(
            tools::layout_items(&replayed.entries),
            vec![Item::Group(0..2), Item::Edits(2..3), Item::Single(3)]
        );
        assert_eq!(
            tools::group_summary(&replayed.entries[0..2]),
            "Thought once, read a file"
        );
        match &replayed.entries[3] {
            ChatEntry::ToolCall {
                name,
                finished,
                is_error,
                ..
            } => {
                assert_eq!(tools::tool_kind(name), ToolKind::Command);
                assert!(
                    *finished && *is_error,
                    "a stored unfinished call was cut off"
                );
            }
            _ => panic!("expected the command"),
        }
        let (id, timing) = &replayed.timings[0];
        assert_eq!(id, "r1");
        assert_eq!(timing.duration_ms(), Some(2_500));
    }

    #[test]
    fn the_turn_in_flight_keeps_running_calls_and_skips_shown_ones() {
        let shown = HashSet::from(["r1".to_string()]);
        let replayed = replay_entries(&items(), true, &shown);
        assert_eq!(replayed.entries.len(), 3);
        assert!(matches!(
            replayed.entries.last(),
            Some(ChatEntry::ToolCall {
                finished: false,
                is_error: false,
                ..
            })
        ));
        assert_eq!(shown_tool_ids(&replayed.entries).len(), 2);
    }

    #[test]
    fn truncated_thinking_is_marked() {
        let replayed = replay_entries(
            &[ActivityItem::Thinking {
                text: "long".into(),
                truncated: true,
            }],
            false,
            &HashSet::new(),
        );
        assert!(matches!(
            &replayed.entries[0],
            ChatEntry::Thinking { text } if text == "long …"
        ));
    }
}
