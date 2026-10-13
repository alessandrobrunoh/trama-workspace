use super::*;
use chrono::Utc;
use serde_json::json;
use std::path::PathBuf;
use trama_domain::DomainError;
use trama_domain::agent::{AgentSession, UsageReport};
use trama_domain::event::{Aggregate, EVENT_SCHEMA_VERSION, EventRecord, EventStore};
use trama_domain::ids::{
    AgentSessionId, CheckoutId, MessageId, RepositoryId, ThreadId, WorkspaceId,
};
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

#[tokio::test]
async fn concrete_checkout_locations_survive_database_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locations.db");
    let db = TramaDb::open(&path).unwrap();
    let workspace = Workspace::new("locations");
    db.create_workspace(&workspace).await.unwrap();
    let repository = WorkspaceMembership {
        workspace_id: workspace.id.clone(),
        repository_id: RepositoryId::generate(),
        alias: None,
        default_enabled: true,
    };
    db.create_repository(&Repository {
        id: repository.repository_id.clone(),
        canonical_remote: None,
        local_path: Some(PathBuf::from("/src/repo")),
        default_branch: Some("main".into()),
    })
    .await
    .unwrap();
    let thread = Thread {
        id: ThreadId::generate(),
        workspace_id: Some(workspace.id),
        parent_thread_id: None,
        kind: ThreadKind::Top,
        title: Some("location".into()),
        repo_attachments: vec![ThreadRepositoryAttachment {
            repository_id: repository.repository_id.clone(),
            base_revision: Some("abc".into()),
            checkout_id: Some(CheckoutId::from("checkout")),
            attach_mode: AttachMode::Isolated,
        }],
    };
    let location = crate::test_abs_path("/src/repo/.trama/worktrees/t/r/c");
    db.create_thread_with_locations(
        &thread,
        &[CheckoutLocation {
            checkout_id: CheckoutId::from("checkout"),
            repository_id: repository.repository_id.clone(),
            path: location.clone(),
        }],
    )
    .await
    .unwrap();
    drop(db);
    let reopened = TramaDb::open(&path).unwrap();
    assert_eq!(
        reopened
            .checkout_location(
                &thread.id,
                &repository.repository_id,
                &CheckoutId::from("checkout")
            )
            .await
            .unwrap(),
        Some(location.clone())
    );
    assert!(
        reopened
            .checkout_location(
                &ThreadId::generate(),
                &repository.repository_id,
                &CheckoutId::from("checkout"),
            )
            .await
            .is_err()
    );
    assert!(
        reopened
            .checkout_location(
                &thread.id,
                &RepositoryId::generate(),
                &CheckoutId::from("checkout"),
            )
            .await
            .is_err()
    );
    let message = Message::new_user(thread.id.clone(), "user", "must roll back");
    let event = EventRecord::new(
        Aggregate::Thread {
            id: thread.id.clone(),
        },
        Actor::System,
        trama_domain::event::DomainEvent::MessageAdded {
            message: message.clone(),
        },
        vec![],
    )
    .unwrap();
    let retarget = reopened
        .persist_thread_migration_with_locations(
            &thread.id,
            &thread.repo_attachments,
            &message,
            &event,
            &[CheckoutLocation {
                checkout_id: CheckoutId::from("checkout"),
                repository_id: repository.repository_id.clone(),
                path: crate::test_abs_path("/different/checkout"),
            }],
        )
        .await;
    assert!(retarget.is_err());
    assert_eq!(
        reopened
            .checkout_location(
                &thread.id,
                &repository.repository_id,
                &CheckoutId::from("checkout")
            )
            .await
            .unwrap(),
        Some(location.clone())
    );
    assert!(
        reopened
            .list_messages_by_thread(&thread.id)
            .await
            .unwrap()
            .is_empty()
    );
    let repeated_message = Message::new_user(thread.id.clone(), "user", "same migration");
    let repeated_event = EventRecord::new(
        Aggregate::Thread {
            id: thread.id.clone(),
        },
        Actor::System,
        trama_domain::event::DomainEvent::MessageAdded {
            message: repeated_message.clone(),
        },
        vec![],
    )
    .unwrap();
    reopened
        .persist_thread_migration_with_locations(
            &thread.id,
            &thread.repo_attachments,
            &repeated_message,
            &repeated_event,
            &[CheckoutLocation {
                checkout_id: CheckoutId::from("checkout"),
                repository_id: repository.repository_id.clone(),
                path: location.clone(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        reopened
            .checkout_location(
                &thread.id,
                &repository.repository_id,
                &CheckoutId::from("checkout")
            )
            .await
            .unwrap(),
        Some(location)
    );
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
            checkout_id: Some(CheckoutId::from("checkout-1")),
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
    assert_eq!(
        threads[0].repo_attachments[0].checkout_id,
        Some(CheckoutId::from("checkout-1"))
    );
    assert_eq!(
        threads[0].repo_attachments[0].base_revision.as_deref(),
        Some("abc123")
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
        claude_uuid: None,
        annotation: None,
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

#[tokio::test]
async fn workspace_repo_thread_and_session_lookups() {
    let db = TramaDb::open_in_memory().unwrap();
    let workspace = Workspace {
        id: WorkspaceId::generate(),
        name: "LaFraternita".into(),
        settings: WorkspaceSettings::default(),
    };
    db.create_workspace(&workspace).await.unwrap();
    assert_eq!(
        db.get_workspace_by_name("LaFraternita")
            .await
            .unwrap()
            .unwrap()
            .id,
        workspace.id
    );
    db.update_workspace_settings(
        &workspace.id,
        &WorkspaceSettings {
            default_model: Some("opus".into()),
            extra: json!({"permission_mode": "accept_edits"}),
        },
    )
    .await
    .unwrap();
    let loaded = db.get_workspace(&workspace.id).await.unwrap().unwrap();
    assert_eq!(loaded.settings.default_model.as_deref(), Some("opus"));

    let backend = Repository {
        id: RepositoryId::generate(),
        canonical_remote: None,
        local_path: Some(PathBuf::from("/tmp/backend")),
        default_branch: Some("main".into()),
    };
    let frontend = Repository {
        id: RepositoryId::generate(),
        canonical_remote: None,
        local_path: Some(PathBuf::from("/tmp/frontend")),
        default_branch: Some("main".into()),
    };
    db.create_repository(&backend).await.unwrap();
    db.create_repository(&frontend).await.unwrap();
    assert_eq!(
        db.get_repository_by_local_path(PathBuf::from("/tmp/backend").as_path())
            .await
            .unwrap()
            .unwrap()
            .id,
        backend.id
    );

    let membership = WorkspaceMembership {
        workspace_id: workspace.id.clone(),
        repository_id: backend.id.clone(),
        alias: Some("backend".into()),
        default_enabled: true,
    };
    db.upsert_membership(&membership).await.unwrap();
    db.upsert_membership(&WorkspaceMembership {
        alias: Some("api".into()),
        ..membership.clone()
    })
    .await
    .unwrap();
    let memberships = db.list_memberships(&workspace.id).await.unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].alias.as_deref(), Some("api"));

    let thread = Thread {
        id: ThreadId::generate(),
        workspace_id: Some(workspace.id.clone()),
        parent_thread_id: None,
        kind: ThreadKind::Top,
        title: Some("work".into()),
        repo_attachments: vec![
            ThreadRepositoryAttachment {
                repository_id: backend.id.clone(),
                base_revision: None,
                checkout_id: None,
                attach_mode: AttachMode::Isolated,
            },
            ThreadRepositoryAttachment {
                repository_id: frontend.id.clone(),
                base_revision: None,
                checkout_id: None,
                attach_mode: AttachMode::Isolated,
            },
        ],
    };
    db.create_thread(&thread).await.unwrap();
    let got = db.get_thread(&thread.id).await.unwrap().unwrap();
    assert_eq!(got.title.as_deref(), Some("work"));
    let repos = db.list_thread_repositories(&thread.id).await.unwrap();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0].id, backend.id);
    assert_eq!(repos[1].id, frontend.id);

    let session = AgentSession {
        id: AgentSessionId::generate(),
        external_session_id: None,
        model: Some("fake".into()),
        cwd: PathBuf::from("/tmp/backend"),
    };
    db.create_agent_session(&thread.id, &session, "claude_code")
        .await
        .unwrap();
    db.set_agent_session_external(&session.id, Some("ext-9".into()), Some("opus".into()))
        .await
        .unwrap();
    let latest = db
        .latest_agent_session_for_thread(&thread.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.external_session_id.as_deref(), Some("ext-9"));
    assert_eq!(latest.model.as_deref(), Some("opus"));

    db.record_usage(
        &session.id,
        &thread.id,
        &UsageReport {
            model: "opus".into(),
            input_tokens: 3,
            output_tokens: 4,
            cache_read_tokens: 1,
            cache_write_tokens: 2,
        },
    )
    .await
    .unwrap();
    let totals = db.usage_totals_for_thread(&thread.id).await.unwrap();
    assert_eq!(totals.input_tokens, 3);
    assert_eq!(totals.output_tokens, 4);
    assert_eq!(totals.model, "opus");
}

#[tokio::test]
async fn workspace_event_membership_and_migration_are_atomic() {
    let db = TramaDb::open_in_memory().unwrap();
    let workspace = Workspace::new("ui");
    let event = EventRecord::new(
        Aggregate::Workspace {
            id: workspace.id.clone(),
        },
        Actor::System,
        trama_domain::event::DomainEvent::WorkspaceCreated {
            workspace: workspace.clone(),
        },
        vec![],
    )
    .unwrap();
    db.create_workspace_with_event(&workspace, &event)
        .await
        .unwrap();
    let other = Workspace::new("other");
    let mut duplicate = EventRecord::new(
        Aggregate::Workspace {
            id: other.id.clone(),
        },
        Actor::System,
        trama_domain::event::DomainEvent::WorkspaceCreated {
            workspace: other.clone(),
        },
        vec![],
    )
    .unwrap();
    duplicate.id = event.id.clone();
    assert!(
        db.create_workspace_with_event(&other, &duplicate)
            .await
            .is_err()
    );
    assert!(db.get_workspace(&other.id).await.unwrap().is_none());

    let path = PathBuf::from("/tmp/trama-upsert-app");
    let repo = Repository::new(None, Some(path.clone()), Some("main".into()));
    let stored = db
        .upsert_repository_membership(
            &repo,
            &WorkspaceMembership {
                workspace_id: workspace.id.clone(),
                repository_id: repo.id.clone(),
                alias: Some("app".into()),
                default_enabled: true,
            },
        )
        .await
        .unwrap();
    let again = db
        .upsert_repository_membership(
            &Repository::new(
                Some("git@example.com:org/app.git".into()),
                Some(path),
                Some("main".into()),
            ),
            &WorkspaceMembership {
                workspace_id: workspace.id.clone(),
                repository_id: RepositoryId::generate(),
                alias: Some("app".into()),
                default_enabled: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(stored.id, again.id);
    assert_eq!(
        again.canonical_remote.as_deref(),
        Some("git@example.com:org/app.git")
    );
    assert_eq!(db.list_all_repositories().await.unwrap().len(), 1);
    assert_eq!(db.list_memberships(&workspace.id).await.unwrap().len(), 1);

    let thread = Thread {
        id: ThreadId::generate(),
        workspace_id: Some(workspace.id.clone()),
        parent_thread_id: None,
        kind: ThreadKind::Top,
        title: Some("legacy".into()),
        repo_attachments: vec![ThreadRepositoryAttachment {
            repository_id: stored.id.clone(),
            base_revision: None,
            checkout_id: None,
            attach_mode: AttachMode::Isolated,
        }],
    };
    db.create_thread(&thread).await.unwrap();
    db.append_message(&Message::new_user(thread.id.clone(), "user", "keep me"))
        .await
        .unwrap();
    let failure = Message {
        id: MessageId::generate(),
        thread_id: thread.id.clone(),
        parent_id: None,
        actor: Actor::System,
        role: Role::System,
        content: vec![ContentBlock::Text {
            text: "should roll back".into(),
        }],
        created_at: Utc::now(),
        claude_uuid: None,
        annotation: None,
    };
    let failure_event = EventRecord::new(
        Aggregate::Thread {
            id: thread.id.clone(),
        },
        Actor::System,
        trama_domain::event::DomainEvent::MessageAdded {
            message: failure.clone(),
        },
        vec![],
    )
    .unwrap();
    let rolled_back = db
        .persist_thread_migration(
            &thread.id,
            &[
                ThreadRepositoryAttachment {
                    repository_id: stored.id.clone(),
                    base_revision: Some("abc".into()),
                    checkout_id: Some(CheckoutId::from("checkout-ok")),
                    attach_mode: AttachMode::Isolated,
                },
                ThreadRepositoryAttachment {
                    repository_id: RepositoryId::from("missing-repo"),
                    base_revision: Some("abc".into()),
                    checkout_id: Some(CheckoutId::from("checkout-missing")),
                    attach_mode: AttachMode::Isolated,
                },
            ],
            &failure,
            &failure_event,
        )
        .await;
    assert!(rolled_back.is_err());
    let unchanged = db.get_thread(&thread.id).await.unwrap().unwrap();
    assert!(unchanged.repo_attachments[0].checkout_id.is_none());
    assert_eq!(
        db.list_messages_by_thread(&thread.id).await.unwrap().len(),
        1
    );

    let notice = Message {
        id: MessageId::generate(),
        thread_id: thread.id.clone(),
        parent_id: None,
        actor: Actor::System,
        role: Role::System,
        content: vec![ContentBlock::Text {
            text: "migrated".into(),
        }],
        created_at: Utc::now(),
        claude_uuid: None,
        annotation: None,
    };
    let notice_event = EventRecord::new(
        Aggregate::Thread {
            id: thread.id.clone(),
        },
        Actor::System,
        trama_domain::event::DomainEvent::MessageAdded {
            message: notice.clone(),
        },
        vec![],
    )
    .unwrap();
    db.persist_thread_migration(
        &thread.id,
        &[ThreadRepositoryAttachment {
            repository_id: stored.id.clone(),
            base_revision: Some("abc".into()),
            checkout_id: Some(CheckoutId::from("checkout-ok")),
            attach_mode: AttachMode::Isolated,
        }],
        &notice,
        &notice_event,
    )
    .await
    .unwrap();
    let migrated = db.get_thread(&thread.id).await.unwrap().unwrap();
    assert_eq!(
        migrated.repo_attachments[0].checkout_id,
        Some(CheckoutId::from("checkout-ok"))
    );
    assert_eq!(
        migrated.repo_attachments[0].base_revision.as_deref(),
        Some("abc")
    );
    let messages = db.list_messages_by_thread(&thread.id).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().any(|message| message.text() == "keep me"));
    assert!(messages.iter().any(|message| message.text() == "migrated"));
}

#[tokio::test]
async fn workspace_name_guard_rejects_duplicates_across_connections() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("trama.db");
    let first = TramaDb::open(&db_path).unwrap();
    let second = TramaDb::open(&db_path).unwrap();

    async fn create_named(db: &TramaDb, name: &str) -> trama_domain::Result<()> {
        let workspace = Workspace::new(name.to_owned());
        let event = EventRecord::new(
            Aggregate::Workspace {
                id: workspace.id.clone(),
            },
            Actor::System,
            trama_domain::event::DomainEvent::WorkspaceCreated {
                workspace: workspace.clone(),
            },
            vec![],
        )
        .unwrap();
        db.create_workspace_with_event(&workspace, &event).await
    }

    // Sequential duplicate on a second connection is a typed error.
    create_named(&first, "Race").await.unwrap();
    let err = create_named(&second, "Race").await.unwrap_err();
    match err {
        DomainError::Invalid(msg) => assert!(msg.contains("already exists"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
    assert_eq!(first.list_workspaces().await.unwrap().len(), 1);

    // Concurrent creators on both connections: exactly one row survives.
    let a = create_named(&first, "Concurrent");
    let b = create_named(&second, "Concurrent");
    let c = create_named(&first, "Concurrent");
    let d = create_named(&second, "Concurrent");
    let (a, b, c, d) = tokio::join!(a, b, c, d);
    let outcomes = [a.is_ok(), b.is_ok(), c.is_ok(), d.is_ok()];
    assert_eq!(
        outcomes.iter().filter(|ok| **ok).count(),
        1,
        "exactly one concurrent creator wins: {outcomes:?}"
    );
    let names: Vec<_> = first
        .list_workspaces()
        .await
        .unwrap()
        .into_iter()
        .map(|workspace| workspace.name)
        .collect();
    assert_eq!(names.iter().filter(|name| *name == "Concurrent").count(), 1);
}

#[tokio::test]
async fn migration_clears_external_session_ids_in_the_same_transaction() {
    let db = TramaDb::open_in_memory().unwrap();
    let workspace = Workspace::new("invalidation");
    db.create_workspace(&workspace).await.unwrap();
    let repository = Repository::new(None, Some(PathBuf::from("/source")), Some("main".into()));
    db.create_repository(&repository).await.unwrap();
    let checkout_path = PathBuf::from("/managed/checkouts/t/r/c");
    let thread = Thread {
        id: ThreadId::generate(),
        workspace_id: Some(workspace.id.clone()),
        parent_thread_id: None,
        kind: ThreadKind::Top,
        title: Some("mixed".into()),
        repo_attachments: vec![ThreadRepositoryAttachment {
            repository_id: repository.id,
            base_revision: None,
            checkout_id: None,
            attach_mode: AttachMode::Adopted,
        }],
    };
    db.create_thread(&thread).await.unwrap();
    db.create_agent_session(
        &thread.id,
        &AgentSession {
            id: AgentSessionId::generate(),
            external_session_id: Some("pre-migration-session".into()),
            model: Some("old-model".into()),
            // Same cwd as the checkout that stays managed: without the
            // invalidation the daemon would resume this session.
            cwd: checkout_path.clone(),
        },
        "claude_code",
    )
    .await
    .unwrap();

    let notice = Message {
        id: MessageId::generate(),
        thread_id: thread.id.clone(),
        parent_id: None,
        actor: Actor::System,
        role: Role::System,
        content: vec![ContentBlock::Text {
            text: "migrated".into(),
        }],
        created_at: Utc::now(),
        claude_uuid: None,
        annotation: None,
    };
    let event = EventRecord::new(
        Aggregate::Thread {
            id: thread.id.clone(),
        },
        Actor::System,
        trama_domain::event::DomainEvent::MessageAdded {
            message: notice.clone(),
        },
        vec![],
    )
    .unwrap();
    db.persist_thread_migration(
        &thread.id,
        &[ThreadRepositoryAttachment {
            repository_id: thread.repo_attachments[0].repository_id.clone(),
            base_revision: Some("abc".into()),
            checkout_id: Some(CheckoutId::from("checkout-new")),
            attach_mode: AttachMode::Isolated,
        }],
        &notice,
        &event,
    )
    .await
    .unwrap();

    let latest = db
        .latest_agent_session_for_thread(&thread.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.external_session_id, None);
    assert_eq!(latest.cwd, checkout_path);
}
