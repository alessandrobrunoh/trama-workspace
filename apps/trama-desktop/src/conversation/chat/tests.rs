use super::{
    ChatEntry, TurnLifecycle, accepts_agent_events, append_stored_assistant, can_submit,
    reconcile_stored_assistant, record_message_id, stop_transition, stored_assistant_transition,
    terminal_transition, unscoped_error_transition,
};
use std::collections::HashSet;
use trama_domain::ids::{AgentSessionId, MessageId, ThreadId};
use trama_domain::message::{ContentBlock, Message};
use trama_protocol::ClientMessage;

fn thread(id: &str) -> ThreadId {
    id.into()
}

#[test]
fn failed_unacknowledged_prompt_is_restored_once_but_accepted_prompt_is_not_retried() {
    let failure = trama_domain::agent::AgentEvent::Failed {
        message: "thread needs migration".into(),
    };
    let mut pending = Some("keep my prompt".into());
    assert_eq!(
        super::take_rejected_prompt(&failure, &mut pending),
        Some("keep my prompt".into())
    );
    assert_eq!(super::take_rejected_prompt(&failure, &mut pending), None);
    let mut accepted = None; // Cleared by MessageAppended(User).
    assert_eq!(super::take_rejected_prompt(&failure, &mut accepted), None);
}

#[test]
fn stopping_keeps_the_composer_busy_until_a_terminal_event() {
    let (state, interrupt) = stop_transition(TurnLifecycle::Running, Some(&thread("thread-1")));
    assert_eq!(
        (state, interrupt),
        (
            TurnLifecycle::Stopping,
            Some(ClientMessage::Interrupt {
                thread_id: thread("thread-1")
            })
        )
    );
    assert_eq!(
        stop_transition(TurnLifecycle::Stopping, Some(&thread("thread-1"))),
        (TurnLifecycle::Stopping, None)
    );
    assert_eq!(
        stop_transition(TurnLifecycle::Running, None),
        (TurnLifecycle::Stopping, None)
    );
    assert!(accepts_agent_events(state));
    assert!(!can_submit(state));
    assert_ne!(state, TurnLifecycle::Idle);
}

#[test]
fn stored_assistant_after_turn_completed_reconciles_only_current_stream() {
    let stored = Message::new_assistant(
        thread("thread-1"),
        AgentSessionId::generate(),
        vec![ContentBlock::Text {
            text: "The answer".into(),
        }],
    );
    let mut entries = vec![
        ChatEntry::User {
            text: "prompt".into(),
            message_id: None,
        },
        ChatEntry::Assistant {
            markdown: "The answer".into(),
            streaming: true,
            message_id: None,
        },
    ];
    let mut seen = HashSet::new();
    assert_eq!(
        terminal_transition(true, false),
        TurnLifecycle::AwaitingAssistant
    );
    assert!(record_message_id(&mut seen, stored.id.clone()));
    assert!(reconcile_stored_assistant(
        &mut entries,
        TurnLifecycle::AwaitingAssistant,
        &stored,
    ));
    assert_eq!(entries.len(), 2);

    assert!(matches!(
        entries.as_slice(),
        [
            ChatEntry::User { .. },
            ChatEntry::Assistant {
                markdown,
                streaming: false,
                message_id: Some(id),
            }
        ] if markdown == "The answer" && id == &stored.id
    ));
    assert!(!record_message_id(&mut seen, stored.id.clone()));
}

#[test]
fn distinct_stored_message_ids_with_identical_text_are_preserved() {
    let first = Message::new_assistant(
        thread("thread-1"),
        AgentSessionId::generate(),
        vec![ContentBlock::Text {
            text: "Same answer".into(),
        }],
    );
    let second = Message::new_assistant(
        thread("thread-1"),
        AgentSessionId::generate(),
        vec![ContentBlock::Text {
            text: "Same answer".into(),
        }],
    );
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for message in [&first, &second, &first] {
        if record_message_id(&mut seen, message.id.clone()) {
            append_stored_assistant(&mut entries, TurnLifecycle::Idle, message);
        }
    }

    assert!(matches!(
        entries.as_slice(),
        [
            ChatEntry::Assistant { markdown: first_text, message_id: Some(first_id), .. },
            ChatEntry::Assistant { markdown: second_text, message_id: Some(second_id), .. }
        ] if first_text == "Same answer"
            && second_text == "Same answer"
            && first_id == &first.id
            && second_id == &second.id
    ));
}

#[test]
fn late_events_are_ignored_until_reconciliation_allows_a_new_turn() {
    let stored = Message::new_assistant(
        thread("thread-1"),
        AgentSessionId::generate(),
        vec![ContentBlock::Text {
            text: "done".into(),
        }],
    );
    let mut entries = vec![ChatEntry::User {
        text: "first".into(),
        message_id: None,
    }];
    let mut lifecycle = terminal_transition(true, false);
    assert_eq!(lifecycle, TurnLifecycle::AwaitingAssistant);
    assert!(!can_submit(lifecycle));
    assert!(!accepts_agent_events(lifecycle));
    append_stored_assistant(&mut entries, lifecycle, &stored);
    lifecycle = stored_assistant_transition(lifecycle);
    assert_eq!(lifecycle, TurnLifecycle::Idle);
    assert!(can_submit(lifecycle));

    entries.push(ChatEntry::User {
        text: "second".into(),
        message_id: None,
    });
    lifecycle = TurnLifecycle::Running;
    assert!(accepts_agent_events(lifecycle));
    assert_eq!(
        record_message_id(&mut HashSet::new(), MessageId::from("same-id")),
        true,
    );
}

#[test]
fn approval_cards_move_from_pending_to_sending_to_resolved() {
    use super::{
        ApprovalState, answer_approval, expire_open_approvals, push_approval, set_approval_state,
    };
    use trama_domain::agent::ApprovalDecision;

    let mut entries = vec![ChatEntry::User {
        text: "go".into(),
        message_id: None,
    }];
    let input = serde_json::json!({"command": "rm -rf target"});
    push_approval(&mut entries, "req-1".into(), "Bash".into(), input.clone());
    push_approval(&mut entries, "req-1".into(), "Bash".into(), input);
    assert_eq!(entries.len(), 2, "replayed requests are not duplicated");

    let thread_id = thread("t");
    let reply = answer_approval(
        &mut entries,
        &thread_id,
        "req-1",
        ApprovalDecision::AllowOnce,
    );
    assert_eq!(
        reply,
        Some(ClientMessage::RespondApproval {
            thread_id: thread_id.clone(),
            request_id: "req-1".into(),
            decision: ApprovalDecision::AllowOnce,
        })
    );
    assert_eq!(
        answer_approval(&mut entries, &thread_id, "req-1", ApprovalDecision::Deny),
        None,
        "a card in flight cannot be answered twice"
    );
    assert_eq!(
        answer_approval(&mut entries, &thread_id, "unknown", ApprovalDecision::Deny),
        None
    );

    assert!(set_approval_state(
        &mut entries,
        "req-1",
        ApprovalState::Resolved {
            decision: ApprovalDecision::AllowOnce,
            reason: None,
        },
    ));
    let ChatEntry::Approval(card) = &entries[1] else {
        panic!("approval card expected");
    };
    assert!(!card.is_open());

    push_approval(
        &mut entries,
        "req-2".into(),
        "Edit".into(),
        serde_json::json!({}),
    );
    expire_open_approvals(&mut entries);
    let ChatEntry::Approval(card) = &entries[2] else {
        panic!("approval card expected");
    };
    assert_eq!(card.state, ApprovalState::Expired);
}

#[test]
fn idle_view_follows_turns_started_elsewhere() {
    use super::starts_remote_turn;
    use trama_domain::agent::AgentEvent;

    let text = AgentEvent::TextDelta { text: "hi".into() };
    let done = AgentEvent::TurnCompleted {
        num_turns: 1,
        cost_usd_micros: None,
    };
    assert!(starts_remote_turn(TurnLifecycle::Idle, &text));
    assert!(!starts_remote_turn(TurnLifecycle::Idle, &done));
    assert!(!starts_remote_turn(TurnLifecycle::Running, &text));
    assert!(
        !starts_remote_turn(TurnLifecycle::AwaitingAssistant, &text),
        "late events of a finished turn stay fenced"
    );
}

#[test]
fn approval_preview_is_compact_and_capped() {
    use super::approval_preview;
    assert_eq!(approval_preview(&serde_json::json!({})), "");
    assert_eq!(
        approval_preview(&serde_json::json!({"command": "ls"})),
        r#"{"command":"ls"}"#
    );
    let long = approval_preview(&serde_json::json!({"content": "x".repeat(1000)}));
    assert_eq!(long.chars().count(), 281);
    assert!(long.ends_with('…'));
}

#[test]
fn tool_summary_prefers_the_meaningful_field_on_one_line() {
    use super::tool_summary;
    use serde_json::json;
    assert_eq!(
        tool_summary("Read", &json!({"file_path": "src/main.rs", "limit": 20})),
        "src/main.rs"
    );
    assert_eq!(
        tool_summary(
            "Bash",
            &json!({"command": "cargo test\necho done", "description": "Run tests"})
        ),
        "cargo test…"
    );
    assert_eq!(
        tool_summary("Grep", &json!({"pattern": "fn main", "path": "src"})),
        "fn main  in src"
    );
    assert_eq!(
        tool_summary("TodoWrite", &json!({"todos": [1, 2, 3]})),
        "3 items"
    );
    assert_eq!(tool_summary("Custom", &json!({"x": 1, "name": "hi"})), "hi");
    assert_eq!(tool_summary("Custom", &json!({})), "");
    let long = "a".repeat(200);
    assert_eq!(
        tool_summary("Bash", &json!({ "command": long }))
            .chars()
            .count(),
        121
    );
}

#[test]
fn stored_prompt_with_delivered_comments_adopts_the_optimistic_entry() {
    use super::append_stored_user;
    use trama_domain::message::Role;

    let user = |text: &str| {
        let mut message = Message::new_assistant(
            thread("thread-1"),
            AgentSessionId::generate(),
            vec![ContentBlock::Text { text: text.into() }],
        );
        message.role = Role::User;
        message
    };
    let block = "<review-comments>\nintro\n\n[1] On x:\nComment: y\n</review-comments>";
    let mut entries = vec![ChatEntry::User {
        text: "continue".into(),
        message_id: None,
    }];
    let mut pending = Some("continue".to_string());
    let stored = user(&format!("{block}\n\ncontinue"));
    append_stored_user(&mut entries, &mut pending, &stored);
    assert_eq!(pending, None);
    assert!(matches!(
        entries.as_slice(),
        [ChatEntry::User { text, message_id: Some(id) }]
            if text.starts_with("<review-comments>") && id == &stored.id
    ));

    // Comments sent on their own arrive as a new entry.
    let only = user(block);
    append_stored_user(&mut entries, &mut pending, &only);
    assert_eq!(entries.len(), 2);
}

#[test]
fn unscoped_error_from_previous_thread_does_not_end_selected_turn() {
    let (lifecycle, banner) =
        unscoped_error_transition(TurnLifecycle::Running, "old thread error".into());
    assert_eq!(lifecycle, TurnLifecycle::Running);
    assert_eq!(banner, "old thread error");
    assert!(!can_submit(lifecycle));
}
