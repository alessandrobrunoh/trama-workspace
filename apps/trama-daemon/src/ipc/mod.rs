//! Per-connection JSON-lines handler.

mod dispatch;
mod hooks;
#[cfg(test)]
mod tests;
#[allow(unused_imports)]
pub(crate) use self::dispatch::*;
#[allow(unused_imports)]
pub(crate) use self::hooks::*;

use crate::approval::HookDecision;
use crate::collab::CollabService;
use crate::error::{DaemonError, Result};
use crate::session::{ApprovalWait, SessionManager, SubscriptionId};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};
use trama_domain::ids::ThreadId;
use trama_protocol::{
    ClientMessage, DaemonMessage, PROTOCOL_VERSION, decode_json_line, encode_json_line,
};

const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn handle_connection<S>(
    stream: S,
    sessions: SessionManager,
    collab: CollabService,
    shutdown: watch::Sender<bool>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();

    let Some(first) = lines.next_line().await? else {
        return Ok(());
    };
    if first.trim().is_empty() {
        write_msg(&mut writer, &welcome(false)).await?;
        return Ok(());
    }

    let accepted = match decode_json_line::<ClientMessage>(&first) {
        Ok(ClientMessage::Hello {
            protocol_version,
            client_name: _,
        }) => protocol_version == PROTOCOL_VERSION,
        _ => false,
    };
    write_msg(&mut writer, &welcome(accepted)).await?;
    if !accepted {
        return Ok(());
    }

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<DaemonMessage>();
    let mut subscriptions: HashMap<ThreadId, SubscriptionId> = HashMap::new();

    loop {
        tokio::select! {
            incoming = lines.next_line() => {
                match incoming {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        if let Ok(ClientMessage::ApprovalHook {
                            token,
                            tool_name,
                            tool_input,
                        }) = decode_json_line::<ClientMessage>(&line)
                        {
                            // A hook connection carries exactly one request.
                            serve_approval_hook(
                                &sessions,
                                &mut lines,
                                &mut writer,
                                &token,
                                tool_name,
                                tool_input,
                            )
                            .await?;
                            break;
                        }
                        if let Ok(ClientMessage::McpToolCall {
                            token,
                            tool,
                            arguments,
                            tool_use_id,
                        }) = decode_json_line::<ClientMessage>(&line)
                        {
                            // A helper connection carries exactly one call.
                            serve_mcp_tool(
                                &sessions,
                                &mut lines,
                                &mut writer,
                                &token,
                                &tool,
                                arguments,
                                tool_use_id,
                            )
                            .await?;
                            break;
                        }
                        match handle_line(
                            &line,
                            &sessions,
                            &collab,
                            &out_tx,
                            &mut subscriptions,
                            &shutdown,
                        ).await {
                            Ok(ConnectionControl::Continue) => {}
                            Ok(ConnectionControl::Close) => break,
                            Err(err) => {
                                let _ = out_tx.send(DaemonMessage::Error {
                                    code: err.code().into(),
                                    message: err.to_string(),
                                });
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(err) => return Err(err.into()),
                }
            }
            outbound = out_rx.recv() => {
                match outbound {
                    Some(msg) => write_msg(&mut writer, &msg).await?,
                    None => break,
                }
            }
        }
    }

    let leftover: Vec<_> = subscriptions.into_iter().collect();
    sessions.unsubscribe_all(&leftover);
    Ok(())
}

enum ConnectionControl {
    Continue,
    Close,
}

fn welcome(accepted: bool) -> DaemonMessage {
    DaemonMessage::Welcome {
        protocol_version: PROTOCOL_VERSION,
        daemon_version: DAEMON_VERSION.to_string(),
        accepted,
    }
}

async fn write_msg<W: AsyncWrite + Unpin>(writer: &mut W, msg: &DaemonMessage) -> Result<()> {
    let bytes = encode_json_line(msg)?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
