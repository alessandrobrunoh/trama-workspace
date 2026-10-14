//! API provider turns (ADR 0015) against a mock OpenAI-compatible server.

use super::*;
use crate::testing::FakeAdapter;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};
use trama_domain::agent::AgentEvent;
use trama_protocol::{ApiKeyUpdate, DaemonMessage, ProviderConfig, ProviderKind};

/// Answers every connection with `parts` as SSE events; requests go to `seen`.
async fn mock_server(
    parts: Vec<&'static str>,
    stall: bool,
) -> (String, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (seen_tx, seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let parts = parts.clone();
            let seen_tx = seen_tx.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let _ = seen_tx.send(String::from_utf8_lossy(&request).to_string());
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n")
                    .await;
                for part in parts {
                    let _ = socket
                        .write_all(format!("{:x}\r\n{part}\r\n", part.len()).as_bytes())
                        .await;
                }
                if stall {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                let _ = socket.write_all(b"0\r\n\r\n").await;
            });
        }
    });
    (format!("http://{addr}/v1"), seen_rx)
}

async fn setup(base_url: String) -> (SessionManager, ThreadId, FakeAdapter, tempfile::TempDir) {
    let db = TramaDb::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let workspace_id = super::tests::seed_workspace(&db, true, dir.path()).await;
    let fake = FakeAdapter::scripted();
    let mgr =
        SessionManager::with_checkout_root(db, Arc::new(fake.clone()), dir.path().join("app"));
    let id = mgr
        .providers()
        .save(
            ProviderConfig {
                id: String::new(),
                kind: ProviderKind::OpenAiCompatible,
                name: "Mock".into(),
                base_url: Some(base_url),
                default_model: Some("mock-model".into()),
                models: Vec::new(),
            },
            ApiKeyUpdate::Keep,
        )
        .unwrap();
    let thread = mgr.create_thread(workspace_id, "t".into()).await.unwrap();
    mgr.set_thread_model(&thread.id, Some(format!("{id}::mock-model")))
        .await
        .unwrap();
    (mgr, thread.id, fake, dir)
}

async fn next_assistant(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> (String, Vec<String>) {
    let mut deltas = Vec::new();
    loop {
        let msg = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("event")
            .expect("open");
        match msg {
            DaemonMessage::AgentEventEnvelope {
                event: AgentEvent::TextDelta { text },
                ..
            } => deltas.push(text),
            DaemonMessage::AgentEventEnvelope {
                event: AgentEvent::Failed { message },
                ..
            } => panic!("turn failed: {message}"),
            DaemonMessage::MessageAppended { message, .. } if message.role == Role::Assistant => {
                let text = match &message.content[0] {
                    trama_domain::message::ContentBlock::Text { text } => text.clone(),
                    other => panic!("{other:?}"),
                };
                return (text, deltas);
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn provider_turn_streams_and_persists_with_history() {
    let (base, mut seen) = mock_server(
        vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"there\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        ],
        false,
    )
    .await;
    let (mgr, thread_id, fake, _dir) = setup(base).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread_id, tx).await.unwrap();

    mgr.send_prompt(thread_id.clone(), "hello".into())
        .await
        .unwrap();
    let (text, deltas) = next_assistant(&mut rx).await;
    assert_eq!(text, "Hi there");
    assert_eq!(deltas, vec!["Hi ", "there"]);
    let first = seen.recv().await.unwrap();
    assert!(first.contains("\"model\":\"mock-model\""));
    assert!(first.contains("\"content\":\"hello\""));

    // Released: a second turn runs and carries the history.
    let mut sent = false;
    for _ in 0..50 {
        if mgr
            .send_prompt(thread_id.clone(), "again".into())
            .await
            .is_ok()
        {
            sent = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(sent, "thread stayed busy");
    next_assistant(&mut rx).await;
    let second = seen.recv().await.unwrap();
    assert!(second.contains("Hi there") && second.contains("again"));
    // The Claude Code adapter never started a session.
    assert!(fake.start_configs.lock().unwrap().is_empty());
    assert!(fake.prompts.lock().unwrap().is_empty());
    let stored = mgr.db.list_messages_by_thread(&thread_id).await.unwrap();
    assert_eq!(stored.len(), 4);
}

#[tokio::test]
async fn interrupt_cancels_a_provider_turn() {
    let (base, _seen) = mock_server(
        vec!["data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"],
        true,
    )
    .await;
    let (mgr, thread_id, _fake, _dir) = setup(base).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread_id, tx).await.unwrap();
    mgr.send_prompt(thread_id.clone(), "hello".into())
        .await
        .unwrap();
    loop {
        let msg = timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            msg,
            DaemonMessage::AgentEventEnvelope {
                event: AgentEvent::TextDelta { .. },
                ..
            }
        ) {
            break;
        }
    }
    mgr.interrupt(&thread_id).await.unwrap();
    loop {
        let msg = timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if let DaemonMessage::MessageAppended { message, .. } = msg
            && message.role == Role::System
        {
            let text = format!("{:?}", message.content);
            assert!(
                text.contains("interrupted") && text.contains("partial"),
                "{text}"
            );
            break;
        }
    }
}

#[tokio::test]
async fn removed_provider_rejects_the_prompt() {
    let (base, _seen) = mock_server(Vec::new(), false).await;
    let (mgr, thread_id, _fake, _dir) = setup(base).await;
    mgr.providers().remove("mock").unwrap();
    let err = mgr
        .send_prompt(thread_id.clone(), "hello".into())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("removed"), "{err}");
    assert!(
        mgr.db
            .list_messages_by_thread(&thread_id)
            .await
            .unwrap()
            .is_empty()
    );
}
