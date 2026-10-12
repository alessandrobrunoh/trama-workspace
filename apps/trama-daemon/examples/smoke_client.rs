//! Headless smoke client: drives the daemon end-to-end over its UDS socket
//! with a real Claude turn. Usage:
//! `cargo run -p trama-daemon --example smoke_client [-- "<prompt>"]`

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use trama_protocol::{ClientMessage, DaemonMessage, PROTOCOL_VERSION, encode_json_line};

async fn send(wr: &mut OwnedWriteHalf, msg: &ClientMessage) -> std::io::Result<()> {
    wr.write_all(&encode_json_line(msg).expect("encode")).await
}

fn brief(event: &trama_domain::agent::AgentEvent) -> String {
    let d = format!("{event:?}");
    d.chars().take(110).collect()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Reply with exactly: TRAMA-E2E-OK".into());
    let socket = trama_protocol::default_app_dir()
        .expect("TRAMA_DATA_DIR or HOME")
        .join("daemon.sock");

    let stream =
        tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(&socket)).await??;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    send(
        &mut wr,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "smoke-client".into(),
        },
    )
    .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let mut thread_id = None;
    let mut assistant_text: Option<String> = None;

    while tokio::time::Instant::now() < deadline {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line()).await??;
        let Some(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: DaemonMessage =
            serde_json::from_str(&line).map_err(|e| format!("bad line: {e}: {line}"))?;
        match msg {
            DaemonMessage::Welcome {
                accepted,
                protocol_version,
                ..
            } => {
                eprintln!("welcome: accepted={accepted} v{protocol_version}");
                if !accepted {
                    return Err("handshake rejected".into());
                }
                send(&mut wr, &ClientMessage::ListWorkspaces).await?;
            }
            DaemonMessage::Workspaces(list) => {
                let Some(ws) = list.first() else {
                    return Err("no workspaces configured".into());
                };
                eprintln!("workspace: {} ({} repos)", ws.title, ws.repo_count);
                send(
                    &mut wr,
                    &ClientMessage::CreateThread {
                        workspace_id: ws.id.clone(),
                        title: "smoke e2e".into(),
                    },
                )
                .await?;
            }
            DaemonMessage::ThreadCreated(snapshot) => {
                eprintln!("thread: {} ({})", snapshot.title, snapshot.id);
                let tid = snapshot.id.clone();
                thread_id = Some(tid.clone());
                send(
                    &mut wr,
                    &ClientMessage::SubscribeThread {
                        thread_id: tid.clone(),
                    },
                )
                .await?;
                send(
                    &mut wr,
                    &ClientMessage::SendPrompt {
                        thread_id: tid,
                        text: prompt.clone(),
                    },
                )
                .await?;
            }
            DaemonMessage::MessageAppended { message, .. } => {
                eprintln!("message appended: role={:?}", message.role);
                if matches!(message.role, trama_domain::message::Role::Assistant) {
                    assistant_text = Some(message.text());
                    break;
                }
            }
            DaemonMessage::AgentEventEnvelope { event, .. } => {
                eprintln!("agent event: {}", brief(&event));
            }
            DaemonMessage::UsageSnapshot { totals, .. } => {
                eprintln!(
                    "usage: in={} out={} cache_read={} cache_write={}",
                    totals.input_tokens,
                    totals.output_tokens,
                    totals.cache_read_tokens,
                    totals.cache_write_tokens
                );
            }
            DaemonMessage::Error { code, message } => {
                return Err(format!("daemon error [{code}]: {message}").into());
            }
            _ => {}
        }
    }

    let text = assistant_text.ok_or("no assistant message before timeout")?;
    println!("ASSISTANT_TEXT={text}");
    let _ = thread_id;
    Ok(())
}
