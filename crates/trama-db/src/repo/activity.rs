//! Per-turn agent activity (tool calls, thinking, subagent lifecycle),
//! stored next to the message that ends the turn.

use crate::codec::{from_json, rfc3339, sql_err, to_json};
use crate::projections::insert_event;
use crate::store::TramaDb;
use chrono::Utc;
use rusqlite::params;
use std::collections::HashMap;
use trama_domain::Result;
use trama_domain::activity::ActivityItem;
use trama_domain::event::{Aggregate, DomainEvent, EventRecord};
use trama_domain::ids::{MessageId, ThreadId};
use trama_domain::message::Actor;

impl TramaDb {
    /// Store the activity of the turn `message_id` ends, with its
    /// `TurnActivityRecorded` event, atomically. Empty activity stores
    /// nothing. The message must already be stored.
    pub async fn record_turn_activity(
        &self,
        thread_id: &ThreadId,
        message_id: &MessageId,
        actor: Actor,
        items: Vec<ActivityItem>,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let items_json = to_json(&items)?;
        let count = items.len() as i64;
        let event = EventRecord::new(
            Aggregate::Thread {
                id: thread_id.clone(),
            },
            actor,
            DomainEvent::TurnActivityRecorded {
                thread_id: thread_id.clone(),
                message_id: message_id.clone(),
                items,
            },
            vec![],
        )?;
        let thread_id = thread_id.to_string();
        let message_id = message_id.to_string();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            tx.execute(
                "INSERT INTO turn_activity (message_id, thread_id, items_json, item_count, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![message_id, thread_id, items_json, count, rfc3339(Utc::now())],
            )
            .map_err(sql_err)?;
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// Stored turn activity of the given threads, by the message that ends
    /// each turn. Rows this build cannot read are skipped.
    pub async fn turn_activity_for_threads(
        &self,
        thread_ids: &[ThreadId],
    ) -> Result<HashMap<MessageId, Vec<ActivityItem>>> {
        let thread_ids: Vec<String> = thread_ids.iter().map(ToString::to_string).collect();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT message_id, items_json FROM turn_activity
                     WHERE thread_id = ?1 ORDER BY created_at ASC",
                )
                .map_err(sql_err)?;
            let mut out = HashMap::new();
            for thread_id in &thread_ids {
                let rows = stmt
                    .query_map(params![thread_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(sql_err)?;
                for row in rows {
                    let (message_id, items_json) = row.map_err(sql_err)?;
                    if let Ok(items) = from_json::<Vec<ActivityItem>>(&items_json) {
                        out.insert(MessageId::from(message_id), items);
                    }
                }
            }
            Ok(out)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama_domain::event::EventStore;
    use trama_domain::ids::AgentSessionId;
    use trama_domain::message::{ContentBlock, Message};
    use trama_domain::thread::{Thread, ThreadKind};
    use trama_domain::workspace::Workspace;

    async fn thread_with_answer(db: &TramaDb) -> (ThreadId, Message) {
        let workspace = Workspace::new("activity");
        db.create_workspace(&workspace).await.unwrap();
        let thread = Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id),
            parent_thread_id: None,
            kind: ThreadKind::Top,
            title: Some("t".into()),
            repo_attachments: vec![],
        };
        db.create_thread(&thread).await.unwrap();
        let answer = Message::new_assistant(
            thread.id.clone(),
            AgentSessionId::generate(),
            vec![ContentBlock::Text {
                text: "done".into(),
            }],
        );
        db.append_message(&answer).await.unwrap();
        (thread.id, answer)
    }

    fn items() -> Vec<ActivityItem> {
        let at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        vec![
            ActivityItem::Thinking {
                text: "plan".into(),
                truncated: false,
            },
            ActivityItem::ToolCall {
                tool_use_id: "t1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                output: Some(serde_json::json!("a")),
                is_error: false,
                finished: true,
                started_at: at,
                finished_at: Some(at),
                truncated: false,
            },
        ]
    }

    #[tokio::test]
    async fn activity_survives_reopen_and_records_an_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trama.db");
        let db = TramaDb::open(&path).unwrap();
        let (thread_id, answer) = thread_with_answer(&db).await;
        db.record_turn_activity(&thread_id, &answer.id, Actor::System, items())
            .await
            .unwrap();
        // Empty activity stores nothing (and is not an error).
        db.record_turn_activity(&thread_id, &MessageId::from("none"), Actor::System, vec![])
            .await
            .unwrap();
        let events = db
            .for_aggregate(
                &Aggregate::Thread {
                    id: thread_id.clone(),
                },
                None,
                100,
            )
            .await
            .unwrap();
        assert!(events.iter().any(|e| e.kind == "turn.activity_recorded"));
        drop(db);

        let reopened = TramaDb::open(&path).unwrap();
        let stored = reopened
            .turn_activity_for_threads(&[thread_id.clone(), ThreadId::from("other")])
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[&answer.id], items());
    }

    #[tokio::test]
    async fn activity_needs_a_stored_message_and_is_written_once() {
        let db = TramaDb::open_in_memory().unwrap();
        let (thread_id, answer) = thread_with_answer(&db).await;
        assert!(
            db.record_turn_activity(
                &thread_id,
                &MessageId::from("missing"),
                Actor::System,
                items()
            )
            .await
            .is_err()
        );
        db.record_turn_activity(&thread_id, &answer.id, Actor::System, items())
            .await
            .unwrap();
        assert!(
            db.record_turn_activity(&thread_id, &answer.id, Actor::System, items())
                .await
                .is_err()
        );
    }
}
