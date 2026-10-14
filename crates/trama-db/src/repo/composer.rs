//! Composer state per thread: model override and the agent's reported
//! slash commands and skills.

use crate::codec::{from_json, rfc3339, sql_err, to_json};
use crate::store::TramaDb;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use trama_domain::Result;
use trama_domain::ids::ThreadId;

/// What the agent reported at its latest session start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentCommands {
    pub slash_commands: Vec<String>,
    pub skills: Vec<String>,
}

impl TramaDb {
    /// Set (or clear with `None`) the thread's model override.
    pub async fn set_thread_model(
        &self,
        thread_id: &ThreadId,
        model: Option<String>,
    ) -> Result<()> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO thread_settings (thread_id, model, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(thread_id) DO UPDATE SET model = excluded.model,
                     updated_at = excluded.updated_at",
                params![thread_id, model, rfc3339(Utc::now())],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn thread_model(&self, thread_id: &ThreadId) -> Result<Option<String>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let model: Option<Option<String>> = conn
                .query_row(
                    "SELECT model FROM thread_settings WHERE thread_id = ?1",
                    [thread_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            Ok(model.flatten())
        })
        .await
    }

    pub async fn record_agent_commands(
        &self,
        thread_id: &ThreadId,
        commands: &AgentCommands,
    ) -> Result<()> {
        let thread_id = thread_id.to_string();
        let slash = to_json(&commands.slash_commands)?;
        let skills = to_json(&commands.skills)?;
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO thread_agent_commands
                    (thread_id, slash_commands_json, skills_json, reported_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(thread_id) DO UPDATE SET
                    slash_commands_json = excluded.slash_commands_json,
                    skills_json = excluded.skills_json,
                    reported_at = excluded.reported_at",
                params![thread_id, slash, skills, rfc3339(Utc::now())],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn agent_commands(&self, thread_id: &ThreadId) -> Result<Option<AgentCommands>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT slash_commands_json, skills_json FROM thread_agent_commands
                     WHERE thread_id = ?1",
                    [thread_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sql_err)?;
            row.map(|(slash, skills)| {
                Ok(AgentCommands {
                    slash_commands: from_json(&slash)?,
                    skills: from_json(&skills)?,
                })
            })
            .transpose()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama_domain::thread::Thread;
    use trama_domain::workspace::Workspace;

    #[tokio::test]
    async fn model_override_and_reported_commands_round_trip() {
        let db = TramaDb::open_in_memory().unwrap();
        let workspace = Workspace::new("W");
        db.create_workspace(&workspace).await.unwrap();
        let thread = Thread::new_for_workspace(&workspace, &[], Some("t".into()));
        db.create_thread(&thread).await.unwrap();

        assert_eq!(db.thread_model(&thread.id).await.unwrap(), None);
        db.set_thread_model(&thread.id, Some("claude-opus-5-5".into()))
            .await
            .unwrap();
        assert_eq!(
            db.thread_model(&thread.id).await.unwrap().as_deref(),
            Some("claude-opus-5-5")
        );
        db.set_thread_model(&thread.id, None).await.unwrap();
        assert_eq!(db.thread_model(&thread.id).await.unwrap(), None);

        assert_eq!(db.agent_commands(&thread.id).await.unwrap(), None);
        let first = AgentCommands {
            slash_commands: vec!["compact".into()],
            skills: vec![],
        };
        db.record_agent_commands(&thread.id, &first).await.unwrap();
        let latest = AgentCommands {
            slash_commands: vec!["compact".into(), "land".into()],
            skills: vec!["land".into()],
        };
        db.record_agent_commands(&thread.id, &latest).await.unwrap();
        assert_eq!(db.agent_commands(&thread.id).await.unwrap(), Some(latest));
    }
}
