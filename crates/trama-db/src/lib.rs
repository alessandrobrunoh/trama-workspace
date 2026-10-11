//! TramaDB — append-only event store plus projections on SQLite (WAL, FTS5).
//!
//! # Blocking bridge
//!
//! `rusqlite::Connection` is synchronous. [`TramaDb`] holds it behind a
//! `Mutex` and runs SQL (and disk blob I/O) on `tokio::task::spawn_blocking`.
//! Open/migrate stay synchronous so a runtime is not required to construct
//! the store. Concurrent async calls serialize on the mutex.

mod codec;
mod content;
mod event;
mod migrations;
mod projections;
mod revision;
mod store;

pub use store::TramaDb;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use std::path::PathBuf;
    use trama_domain::DomainError;
    use trama_domain::agent::{AgentSession, UsageReport};
    use trama_domain::event::{Aggregate, EVENT_SCHEMA_VERSION, EventRecord, EventStore};
    use trama_domain::ids::{AgentSessionId, MessageId, RepositoryId, ThreadId, WorkspaceId};
    use trama_domain::message::{Actor, ContentBlock, Message, Role};
    use trama_domain::revision::{ContentStore, FileRevision, RevisionStore};
    use trama_domain::thread::{AttachMode, Thread, ThreadKind, ThreadRepositoryAttachment};
    use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership, WorkspaceSettings};

    fn sample_event(id: &str, aggregate: Aggregate) -> EventRecord {
        EventRecord {
            id: id.to_owned(),
            seq: None,
            aggregate,
            actor: Actor::System,
            schema_version: EVENT_SCHEMA_VERSION,
            kind: "workspace.created".into(),
            payload: json!({"ok": true}),
            parents: vec![],
            created_at: Utc::now(),
        }
    }

    async fn seed_workspace_thread(db: &TramaDb) -> (Workspace, Thread) {
        let workspace = Workspace {
            id: WorkspaceId::generate(),
            name: "LaFraternita".into(),
            settings: WorkspaceSettings::default(),
        };
        db.create_workspace(&workspace).await.unwrap();
        let thread = Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id.clone()),
            parent_thread_id: None,
            kind: ThreadKind::Top,
            title: Some("kickoff".into()),
            repo_attachments: vec![],
        };
        db.create_thread(&thread).await.unwrap();
        (workspace, thread)
    }

    #[test]
    fn open_in_memory_and_migrations_idempotent() {
        crate::migrations::migrations()
            .validate()
            .expect("schema SQL should apply cleanly");
        let db = TramaDb::open_in_memory().unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trama.db");
        let _first = TramaDb::open(&path).unwrap();
        let _second = TramaDb::open(&path).unwrap();
    }

    #[tokio::test]
    async fn workspace_memberships_thread_snapshot() {
        let db = TramaDb::open_in_memory().unwrap();
        let workspace = Workspace {
            id: WorkspaceId::generate(),
            name: "LaFraternita".into(),
            settings: WorkspaceSettings {
                default_model: Some("claude-opus".into()),
                extra: json!({}),
            },
        };
        db.create_workspace(&workspace).await.unwrap();

        let listed = db.list_workspaces().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "LaFraternita");

        let repository = Repository {
            id: RepositoryId::generate(),
            canonical_remote: Some("git@github.com:example/backend.git".into()),
            local_path: Some(PathBuf::from("/tmp/backend")),
            default_branch: Some("main".into()),
        };
        db.create_repository(&repository).await.unwrap();

        let membership = WorkspaceMembership {
            workspace_id: workspace.id.clone(),
            repository_id: repository.id.clone(),
            alias: Some("backend".into()),
            default_enabled: true,
        };
        db.add_membership(&membership).await.unwrap();
        let memberships = db.list_memberships(&workspace.id).await.unwrap();
        assert_eq!(memberships.len(), 1);
        assert_eq!(memberships[0].alias.as_deref(), Some("backend"));
        assert!(memberships[0].default_enabled);

        let thread = Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id.clone()),
            parent_thread_id: None,
            kind: ThreadKind::Top,
            title: Some("auth workstream".into()),
            repo_attachments: vec![ThreadRepositoryAttachment {
                repository_id: repository.id.clone(),
                base_revision: Some("abc123".into()),
                checkout_id: None,
                attach_mode: AttachMode::Isolated,
            }],
        };
        db.create_thread(&thread).await.unwrap();
        let threads = db.list_threads(&workspace.id).await.unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, thread.id);
        assert_eq!(threads[0].title.as_deref(), Some("auth workstream"));
        assert_eq!(threads[0].repo_attachments.len(), 1);
        assert_eq!(threads[0].repo_attachments[0].repository_id, repository.id);
        assert_eq!(
            threads[0].repo_attachments[0].attach_mode,
            AttachMode::Isolated
        );
    }

    #[tokio::test]
    async fn message_append_and_fts_search() {
        let db = TramaDb::open_in_memory().unwrap();
        let (_ws, thread) = seed_workspace_thread(&db).await;
        let message = Message {
            id: MessageId::generate(),
            thread_id: thread.id.clone(),
            parent_id: None,
            actor: Actor::Human { id: "alice".into() },
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "the narwhal baconates at midnight".into(),
            }],
            created_at: Utc::now(),
        };
        db.append_message(&message).await.unwrap();
        let listed = db.list_messages_by_thread(&thread.id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].text(), message.text());

        let hits = db.search_messages("narwhal").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, message.id);
    }

    #[tokio::test]
    async fn event_append_assigns_seq_and_for_aggregate_roundtrip() {
        let db = TramaDb::open_in_memory().unwrap();
        let aggregate = Aggregate::Workspace {
            id: WorkspaceId::from("ws_1"),
        };
        let other = Aggregate::Thread {
            id: ThreadId::from("th_1"),
        };
        db.append(vec![
            sample_event("e1", aggregate.clone()),
            sample_event("e2", aggregate.clone()),
            sample_event("e3", aggregate.clone()),
        ])
        .await
        .unwrap();
        db.append(vec![sample_event("e-other", other)])
            .await
            .unwrap();

        let got = db.for_aggregate(&aggregate, None, 100).await.unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(
            got.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(3)]
        );
        assert_eq!(got[0].id, "e1");
        assert_eq!(got[1].id, "e2");
        assert_eq!(got[2].id, "e3");
        assert_eq!(got[0].aggregate, aggregate);

        let after = db.for_aggregate(&aggregate, Some(1), 10).await.unwrap();
        assert_eq!(
            after.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["e2", "e3"]
        );
    }

    #[tokio::test]
    async fn duplicate_event_id_rejected() {
        let db = TramaDb::open_in_memory().unwrap();
        let aggregate = Aggregate::Workspace {
            id: WorkspaceId::from("ws_dup"),
        };
        db.append(vec![sample_event("same", aggregate.clone())])
            .await
            .unwrap();
        let err = db
            .append(vec![sample_event("same", aggregate)])
            .await
            .expect_err("duplicate id must fail");
        match err {
            DomainError::Invalid(msg) => assert!(msg.contains("duplicate event id"), "{msg}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn content_store_put_get_roundtrip() {
        let mem = TramaDb::open_in_memory().unwrap();
        mem.put("ab" /* prefix == hash */, b"hello blobs")
            .await
            .unwrap();
        assert_eq!(
            mem.get("ab").await.unwrap().as_deref(),
            Some(&b"hello blobs"[..])
        );
        assert_eq!(mem.get("missing").await.unwrap(), None);

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("trama.db");
        let disk = TramaDb::open(&db_path).unwrap();
        let hash = "deadbeefcafebabe";
        disk.put(hash, b"on-disk").await.unwrap();
        assert_eq!(
            disk.get(hash).await.unwrap().as_deref(),
            Some(&b"on-disk"[..])
        );
        let expected = dir.path().join("objects").join("de").join(hash);
        assert_eq!(std::fs::read(expected).unwrap(), b"on-disk");
    }

    #[tokio::test]
    async fn revision_store_and_usage_report() {
        let db = TramaDb::open_in_memory().unwrap();
        let (_ws, thread) = seed_workspace_thread(&db).await;
        let repo = RepositoryId::generate();
        let path = PathBuf::from("src/main.rs");
        let rev1 = FileRevision {
            repository_id: repo.clone(),
            path: path.clone(),
            revision: 1,
            content_hash: "h1".into(),
            size_bytes: 4,
            event_seq: Some(1),
            recorded_at: Utc::now(),
        };
        let rev2 = FileRevision {
            revision: 2,
            content_hash: "h2".into(),
            size_bytes: 8,
            event_seq: Some(2),
            ..rev1.clone()
        };
        db.record(rev1.clone()).await.unwrap();
        db.record(rev2.clone()).await.unwrap();
        let latest = db.latest(&repo, &path).await.unwrap().unwrap();
        assert_eq!(latest.revision, 2);
        assert_eq!(latest.content_hash, "h2");
        let history = db.history(&repo, &path, 10).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].revision, 2);
        assert_eq!(history[1].revision, 1);

        let session = AgentSession {
            id: AgentSessionId::generate(),
            external_session_id: Some("ext-1".into()),
            model: Some("claude-opus".into()),
            cwd: PathBuf::from("/tmp/ws"),
        };
        db.create_agent_session(&thread.id, &session, "claude_code")
            .await
            .unwrap();
        db.record_usage(
            &session.id,
            &thread.id,
            &UsageReport {
                model: "claude-opus".into(),
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 1,
                cache_write_tokens: 2,
            },
        )
        .await
        .unwrap();
    }
}
