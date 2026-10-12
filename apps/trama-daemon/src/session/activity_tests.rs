//! Turn activity: stored with the turn's answer, replayed before it on
//! subscribe (also after a restart), bounded in size, and replayed for the
//! turn in flight without duplicating live events.

use super::agents_tests::{fixture, manager, run_prompt};
use super::*;
use crate::testing::FakeAdapter;
use tokio::time::{Duration, timeout};
use trama_domain::activity::{ActivityItem, MAX_JSON_BYTES, json_len};

fn tool_events(output: serde_json::Value) -> Vec<AgentEvent> {
    vec![
        AgentEvent::SessionStarted {
            external_session_id: Some("fake-ext-1".into()),
            model: Some("fake".into()),
        },
        AgentEvent::ReasoningDelta {
            text: "Let me look.".into(),
        },
        AgentEvent::ToolCallStarted {
            tool_use_id: "toolu_1".into(),
            name: "Bash".into(),
            input_json: serde_json::json!({"command": "ls"}),
        },
        AgentEvent::ToolCallFinished {
            tool_use_id: "toolu_1".into(),
            output_json: output,
            is_error: false,
        },
        AgentEvent::TextDelta {
            text: "Listed.".into(),
        },
        AgentEvent::TurnCompleted {
            num_turns: 1,
            cost_usd_micros: None,
        },
    ]
}

/// Everything a fresh subscriber receives right away.
async fn replay(mgr: &SessionManager, thread_id: &ThreadId) -> Vec<DaemonMessage> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sub = mgr.subscribe(thread_id, tx).await.unwrap();
    mgr.unsubscribe(thread_id, sub).unwrap();
    let mut out = Vec::new();
    while let Ok(message) = rx.try_recv() {
        out.push(message);
    }
    out
}

/// `(kind, detail)` per replayed frame: messages by role, activity by the
/// message it belongs to.
fn shape(frames: &[DaemonMessage]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            DaemonMessage::MessageAppended { message, .. } => {
                Some(format!("message:{:?}:{}", message.role, message.text()))
            }
            DaemonMessage::TurnActivity {
                message_id, items, ..
            } => Some(format!("activity:{}:{}", message_id.is_some(), items.len())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn activity_is_replayed_before_its_answer_and_survives_a_restart() {
    let (fixture, db) = fixture().await;
    let fake = FakeAdapter::new(
        tool_events(serde_json::json!("a\nb")),
        Duration::from_millis(1),
    );
    let mgr = manager(&fixture, db, &fake);
    let thread = mgr
        .create_thread(fixture.workspace.clone(), "activity".into())
        .await
        .unwrap();
    run_prompt(&mgr, &thread.id, "list files").await;

    let frames = replay(&mgr, &thread.id).await;
    assert_eq!(
        shape(&frames),
        vec![
            "message:User:list files".to_string(),
            "activity:true:2".to_string(),
            "message:Assistant:Listed.".to_string(),
        ]
    );
    let answer_id = match &frames[2] {
        DaemonMessage::MessageAppended { message, .. } => message.id.clone(),
        other => panic!("{other:?}"),
    };
    match &frames[1] {
        DaemonMessage::TurnActivity {
            message_id, items, ..
        } => {
            assert_eq!(message_id.as_ref(), Some(&answer_id));
            assert_eq!(
                items[0],
                ActivityItem::Thinking {
                    text: "Let me look.".into(),
                    truncated: false
                }
            );
            match &items[1] {
                ActivityItem::ToolCall {
                    tool_use_id,
                    name,
                    output,
                    finished,
                    finished_at,
                    ..
                } => {
                    assert_eq!((tool_use_id.as_str(), name.as_str()), ("toolu_1", "Bash"));
                    assert_eq!(output, &Some(serde_json::json!("a\nb")));
                    assert!(*finished && finished_at.is_some());
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }

    // Restart: a new manager on the same database file.
    drop(mgr);
    let reopened = TramaDb::open(&fixture.dir.path().join("trama.db")).unwrap();
    let restarted = manager(&fixture, reopened, &fake);
    let again = replay(&restarted, &thread.id).await;
    assert_eq!(shape(&again), shape(&frames));
    match (&again[1], &frames[1]) {
        (
            DaemonMessage::TurnActivity { items: a, .. },
            DaemonMessage::TurnActivity { items: b, .. },
        ) => assert_eq!(a, b),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn stored_outputs_are_bounded() {
    let (fixture, db) = fixture().await;
    let output = format!("FIRST{}LAST", "x".repeat(300_000));
    let fake = FakeAdapter::new(
        tool_events(serde_json::json!(output)),
        Duration::from_millis(1),
    );
    let mgr = manager(&fixture, db, &fake);
    let thread = mgr
        .create_thread(fixture.workspace.clone(), "big".into())
        .await
        .unwrap();
    run_prompt(&mgr, &thread.id, "big output").await;
    let stored = mgr
        .db
        .turn_activity_for_threads(std::slice::from_ref(&thread.id))
        .await
        .unwrap();
    let items = stored.values().next().expect("one stored turn");
    let ActivityItem::ToolCall {
        output: Some(output),
        truncated,
        ..
    } = &items[1]
    else {
        panic!("{items:?}");
    };
    assert!(*truncated);
    assert!(json_len(output) <= MAX_JSON_BYTES);
    let text = output.as_str().unwrap();
    assert!(text.starts_with("FIRST") && text.ends_with("LAST"));
}

#[tokio::test]
async fn a_failed_turn_keeps_its_activity_with_the_notice() {
    let (fixture, db) = fixture().await;
    let events = vec![
        AgentEvent::ToolCallStarted {
            tool_use_id: "toolu_9".into(),
            name: "Read".into(),
            input_json: serde_json::json!({"file_path": "a.txt"}),
        },
        AgentEvent::Failed {
            message: "boom".into(),
        },
    ];
    let fake = FakeAdapter::new(events, Duration::from_millis(1));
    let mgr = manager(&fixture, db, &fake);
    let thread = mgr
        .create_thread(fixture.workspace.clone(), "fails".into())
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx).await.unwrap();
    mgr.send_prompt(thread.id.clone(), "read it".into())
        .await
        .unwrap();
    loop {
        let message = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("notice")
            .expect("open");
        if let DaemonMessage::MessageAppended { message, .. } = message
            && message.role == Role::System
        {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let frames = replay(&mgr, &thread.id).await;
    let shapes = shape(&frames);
    assert_eq!(shapes[0], "message:User:read it");
    assert_eq!(shapes[1], "activity:true:1");
    assert!(shapes[2].starts_with("message:System:The agent turn failed: boom"));
    match &frames[1] {
        DaemonMessage::TurnActivity { items, .. } => assert!(matches!(
            &items[0],
            ActivityItem::ToolCall {
                finished: false,
                ..
            }
        )),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_subscriber_joining_mid_turn_gets_each_step_once() {
    let (fixture, db) = fixture().await;
    let mut events = vec![AgentEvent::ToolCallStarted {
        tool_use_id: "toolu_1".into(),
        name: "Grep".into(),
        input_json: serde_json::json!({"pattern": "x"}),
    }];
    for _ in 0..6 {
        events.push(AgentEvent::TextDelta { text: ".".into() });
    }
    events.push(AgentEvent::ToolCallFinished {
        tool_use_id: "toolu_1".into(),
        output_json: serde_json::json!("hit"),
        is_error: false,
    });
    events.push(AgentEvent::TurnCompleted {
        num_turns: 1,
        cost_usd_micros: None,
    });
    let fake = FakeAdapter::new(events, Duration::from_millis(40));
    let mgr = manager(&fixture, db, &fake);
    let thread = mgr
        .create_thread(fixture.workspace.clone(), "live".into())
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx).await.unwrap();
    mgr.send_prompt(thread.id.clone(), "search".into())
        .await
        .unwrap();
    loop {
        let message = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("tool call")
            .expect("open");
        if let DaemonMessage::AgentEventEnvelope {
            event: AgentEvent::ToolCallStarted { .. },
            ..
        } = message
        {
            break;
        }
    }

    let (late_tx, mut late) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, late_tx).await.unwrap();
    let mut frames = Vec::new();
    loop {
        let message = timeout(Duration::from_secs(10), late.recv())
            .await
            .expect("late frames")
            .expect("open");
        let done = matches!(message, DaemonMessage::UsageSnapshot { .. })
            && frames
                .iter()
                .any(|f| matches!(f, DaemonMessage::MessageAppended { message, .. } if message.role == Role::Assistant));
        frames.push(message);
        if done {
            break;
        }
    }
    let in_flight: Vec<&Vec<ActivityItem>> = frames
        .iter()
        .filter_map(|frame| match frame {
            DaemonMessage::TurnActivity {
                message_id: None,
                items,
                ..
            } => Some(items),
            _ => None,
        })
        .collect();
    assert_eq!(in_flight.len(), 1, "one in-flight replay");
    assert!(matches!(
        &in_flight[0][..],
        [ActivityItem::ToolCall {
            finished: false,
            ..
        }]
    ));
    let live_starts = frames
        .iter()
        .filter(|frame| {
            matches!(
                frame,
                DaemonMessage::AgentEventEnvelope {
                    event: AgentEvent::ToolCallStarted { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(live_starts, 0, "the started call came with the replay only");
    assert!(frames.iter().any(|frame| matches!(
        frame,
        DaemonMessage::AgentEventEnvelope {
            event: AgentEvent::ToolCallFinished { .. },
            ..
        }
    )));
    // The order: prompt, in-flight activity, then live events.
    let user = frames
        .iter()
        .position(|f| matches!(f, DaemonMessage::MessageAppended { message, .. } if message.role == Role::User))
        .unwrap();
    let activity = frames
        .iter()
        .position(|f| matches!(f, DaemonMessage::TurnActivity { .. }))
        .unwrap();
    let first_live = frames
        .iter()
        .position(|f| matches!(f, DaemonMessage::AgentEventEnvelope { .. }))
        .unwrap();
    assert!(user < activity && activity < first_live);
}
