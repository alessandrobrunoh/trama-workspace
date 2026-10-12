//! Turn checkpoints and forking at a message, with the fake adapter and fake
//! Claude Code transcripts.

use super::agents_tests::{checkout, configs, fixture, git, manager, run_prompt};
use super::*;
use crate::testing::{FakeAdapter, scripted_events};
use std::path::Path;
use tokio::time::Duration;
use trama_infra::claude::transcript::project_dir_name;
use trama_protocol::SubthreadKind;

/// The fake agent writes `same.txt` = `<word>` for a prompt `write <word>`
/// and reports a transcript uuid `a-<turn>` per turn.
fn writing_fake() -> FakeAdapter {
    let mut events = scripted_events();
    events.insert(
        events.len() - 1,
        AgentEvent::TranscriptEntry { uuid: "a".into() },
    );
    FakeAdapter::new(events, Duration::from_millis(1)).with_on_send(|cwd, prompt| {
        if let Some(word) = prompt.strip_prefix("write ") {
            std::fs::write(cwd.join("same.txt"), format!("{word}\n")).unwrap();
        }
    })
}

fn line(uuid: &str, parent: Option<&str>, kind: &str, text: &str) -> String {
    serde_json::json!({
        "parentUuid": parent,
        "isSidechain": false,
        "type": kind,
        "uuid": uuid,
        "sessionId": "fake-ext-1",
        "message": {"role": kind, "content": text},
    })
    .to_string()
}

/// Write the transcript Claude Code would keep for `thread`'s session.
async fn write_transcript(
    mgr: &SessionManager,
    projects: &Path,
    thread: &ThreadId,
    lines: &[String],
) {
    let session = mgr
        .db
        .latest_agent_session_for_thread(thread)
        .await
        .unwrap()
        .unwrap();
    let dir = projects.join(project_dir_name(&session.cwd));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!(
            "{}.jsonl",
            session.external_session_id.as_deref().unwrap()
        )),
        lines.join("\n") + "\n",
    )
    .unwrap();
}

fn uuids(path: &str) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["uuid"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

async fn answers(mgr: &SessionManager, thread: &ThreadId) -> Vec<Message> {
    mgr.db
        .list_messages_by_thread(thread)
        .await
        .unwrap()
        .into_iter()
        .filter(|message| message.role == Role::Assistant)
        .collect()
}

#[tokio::test]
async fn turns_record_checkpoints_and_forks_cut_conversation_and_files_at_a_message() {
    let (fixture, db) = fixture().await;
    let projects = fixture.dir.path().join("claude-projects");
    let fake = writing_fake();
    let mgr = manager(&fixture, db, &fake);
    let parent = mgr
        .create_thread(fixture.workspace.clone(), "main".into())
        .await
        .unwrap()
        .id;
    run_prompt(&mgr, &parent, "write v1").await;
    run_prompt(&mgr, &parent, "write v2").await;
    let parent_path = checkout(&mgr, &parent).await;
    assert_eq!(
        std::fs::read_to_string(parent_path.join("same.txt")).unwrap(),
        "v2\n"
    );

    // Each turn's answer keeps its transcript uuid and a file checkpoint.
    let turn_answers = answers(&mgr, &parent).await;
    assert_eq!(turn_answers.len(), 2);
    assert_eq!(turn_answers[0].claude_uuid.as_deref(), Some("a-1"));
    assert_eq!(turn_answers[1].claude_uuid.as_deref(), Some("a-2"));
    let checkpoint = mgr
        .db
        .checkpoint_for_message(&parent, &turn_answers[0].id)
        .await
        .unwrap()
        .expect("turn checkpoint");
    let commit = checkpoint.repo_states[0].commit.clone().unwrap();
    assert_eq!(
        git(
            &parent_path,
            &[
                "rev-parse",
                &format!("refs/trama/checkpoints/{}", checkpoint.id)
            ]
        ),
        commit
    );
    assert_eq!(
        git(&parent_path, &["show", &format!("{commit}:same.txt")]),
        "v1"
    );
    // The checkout's branch, index and stash are untouched.
    assert_eq!(git(&parent_path, &["status", "--porcelain"]), "M same.txt");
    assert!(git(&parent_path, &["stash", "list"]).is_empty());

    let user_messages: Vec<_> = mgr
        .db
        .list_messages_by_thread(&parent)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.role == Role::User)
        .collect();
    write_transcript(
        &mgr,
        &projects,
        &parent,
        &[
            line("u-1", None, "user", "write v1"),
            line("a-1", Some("u-1"), "assistant", "done 1"),
            line("side", Some("a-1"), "assistant", "sidechain"),
            line("s-1", Some("a-1"), "system", ""),
            line("u-2", Some("s-1"), "user", "write v2"),
            line("a-2", Some("u-2"), "assistant", "done 2"),
        ],
    )
    .await;

    // Fork at the first turn's answer.
    let child = mgr
        .fork_thread(
            parent.clone(),
            Some(turn_answers[0].id.clone()),
            Some("Try another way".into()),
            None,
            SubthreadKind::Plain,
        )
        .await
        .unwrap();
    assert_eq!(child.parent_thread_id.as_ref(), Some(&parent));
    assert_eq!(child.depth, 1);
    let child_path = checkout(&mgr, &child.id).await;
    assert_eq!(
        std::fs::read_to_string(child_path.join("same.txt")).unwrap(),
        "v1\n"
    );
    assert_eq!(
        std::fs::read_to_string(parent_path.join("same.txt")).unwrap(),
        "v2\n"
    );
    let fork = mgr.db.get_subthread_fork(&child.id).await.unwrap().unwrap();
    assert_eq!(fork.fork_message_id.as_ref(), Some(&turn_answers[0].id));
    let copy = fork.parent_external_session_id.clone().unwrap();
    assert!(
        copy.starts_with(
            &fixture
                .dir
                .path()
                .join("managed/forks")
                .to_string_lossy()
                .into_owned()
        )
    );
    assert_eq!(uuids(&copy), ["u-1", "a-1"]);
    let events = mgr
        .db
        .for_aggregate(
            &Aggregate::Thread {
                id: child.id.clone(),
            },
            None,
            20,
        )
        .await
        .unwrap();
    assert!(events.iter().any(|e| e.kind == "thread.forked_at_message"));

    // The child shows the parent's conversation up to that message.
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&child.id, tx).await.unwrap();
    let mut inherited = Vec::new();
    while let Ok(DaemonMessage::MessageAppended { message, .. }) = rx.try_recv() {
        inherited.push(message.id);
    }
    assert_eq!(
        inherited,
        [user_messages[0].id.clone(), turn_answers[0].id.clone()]
    );
    // Its first session resumes the truncated copy as a fork.
    run_prompt(&mgr, &child.id, "write v3").await;
    let config = configs(&fake)
        .into_iter()
        .rev()
        .find(|config| same_directory(&config.cwd, &child_path))
        .unwrap();
    assert_eq!(
        config.resume_external_session.as_deref(),
        Some(copy.as_str())
    );
    assert!(config.fork_session);

    // A user message forks at the last turn completed before it.
    let before_second = mgr
        .fork_thread(
            parent.clone(),
            Some(user_messages[1].id.clone()),
            None,
            None,
            SubthreadKind::Plain,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(checkout(&mgr, &before_second.id).await.join("same.txt")).unwrap(),
        "v1\n"
    );
    // Without a message: the current state.
    let current = mgr
        .fork_thread(parent.clone(), None, None, None, SubthreadKind::Plain)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(checkout(&mgr, &current.id).await.join("same.txt")).unwrap(),
        "v2\n"
    );
    // Nothing to fork before the first turn ended, and only own messages.
    let err = mgr
        .fork_thread(
            parent.clone(),
            Some(user_messages[0].id.clone()),
            None,
            None,
            SubthreadKind::Plain,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid");
    let err = mgr
        .fork_thread(
            child.id.clone(),
            Some(turn_answers[0].id.clone()),
            None,
            None,
            SubthreadKind::Plain,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("own messages"), "{err}");

    // Forks nest: fork the child at its own answer. Its session file only
    // holds its own entries (they continue the parent's chain).
    let child_answer = answers(&mgr, &child.id).await.remove(0);
    let child_uuid = child_answer.claude_uuid.clone().unwrap();
    write_transcript(
        &mgr,
        &projects,
        &child.id,
        &[
            line("u-c", Some("a-1"), "user", "write v3"),
            line(&child_uuid, Some("u-c"), "assistant", "done 3"),
        ],
    )
    .await;
    let grandchild = mgr
        .fork_thread(
            child.id.clone(),
            Some(child_answer.id.clone()),
            None,
            None,
            SubthreadKind::Plain,
        )
        .await
        .unwrap();
    assert_eq!(grandchild.depth, 2);
    assert_eq!(
        std::fs::read_to_string(checkout(&mgr, &grandchild.id).await.join("same.txt")).unwrap(),
        "v3\n"
    );
    let fork = mgr
        .db
        .get_subthread_fork(&grandchild.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        uuids(&fork.parent_external_session_id.unwrap()),
        ["u-1", "a-1", "u-c", child_uuid.as_str()]
    );
    // The grandchild sees the whole inherited conversation.
    let inherited = mgr.inherited_messages(&grandchild.id).await.unwrap();
    assert_eq!(
        inherited.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
        [
            user_messages[0].id.clone(),
            turn_answers[0].id.clone(),
            mgr.db.list_messages_by_thread(&child.id).await.unwrap()[0]
                .id
                .clone(),
            child_answer.id.clone(),
        ]
    );
}

#[tokio::test]
async fn fork_without_a_transcript_is_refused_and_leaves_nothing_behind() {
    let (fixture, db) = fixture().await;
    let fake = writing_fake();
    let mgr = manager(&fixture, db, &fake);
    let parent = mgr
        .create_thread(fixture.workspace.clone(), "main".into())
        .await
        .unwrap()
        .id;
    run_prompt(&mgr, &parent, "write v1").await;
    let answer = answers(&mgr, &parent).await.remove(0);
    let err = mgr
        .fork_thread(
            parent.clone(),
            Some(answer.id),
            None,
            None,
            SubthreadKind::Plain,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("transcript"), "{err}");
    assert!(mgr.list_subthreads(&parent).await.unwrap().is_empty());
}
