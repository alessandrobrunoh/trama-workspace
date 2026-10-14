//! `GET /v3/shares/{id}/stream?after=N`: backfill from the log, then live.
//!
//! The socket subscribes before backfilling and drops live events at or
//! below its cursor; a live event that skips ahead (or a lagged channel)
//! triggers another backfill, so delivery never skips or repeats a
//! `server_seq`. Membership is re-checked on every backfill and every 30 s.

use std::time::Duration;

use axum::{
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::Response,
};
use tokio::sync::broadcast::error::RecvError;
use trama_collab_protocol::vnext::VNEXT_PROTOCOL_VERSION;
use trama_collab_protocol::vnext::api::{
    MAX_SHARE_EVENTS_PAGE, ShareClientFrame, ShareServerFrame,
};

use super::events::{PageQuery, page};
use super::{ShareError, read, validate_id};
use crate::{AppState, collab};

const REVALIDATE_EVERY: Duration = Duration::from_secs(30);
const MAX_CLIENT_FRAME_BYTES: usize = 4 * 1024;

pub(crate) async fn socket(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ShareError> {
    let identity = collab::session(&state, &headers).await?;
    validate_id(&share_id)?;
    let (id, user) = (share_id.clone(), identity.user_id);
    let head = read(&state, move |c| page(c, &id, user, 0, 1)).await?;
    if query.after > head.last_server_seq {
        return Err(ShareError::Invalid("cursor beyond the log head"));
    }
    let slot = state.hub.open_socket(identity.user_id)?;
    Ok(upgrade
        .max_message_size(MAX_CLIENT_FRAME_BYTES)
        .on_upgrade(move |socket| async move {
            let _slot = slot;
            run(&state, socket, &share_id, identity.user_id, query.after).await;
            state.shares.release(&share_id);
        }))
}

async fn send(socket: &mut WebSocket, frame: &ShareServerFrame) -> Result<(), ()> {
    let text = serde_json::to_string(frame).map_err(|_| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

async fn backfill(
    state: &AppState,
    socket: &mut WebSocket,
    share_id: &str,
    user_id: i64,
    cursor: &mut u64,
) -> Result<(), ()> {
    loop {
        let (id, after) = (share_id.to_owned(), *cursor);
        let batch = read(state, move |c| {
            page(c, &id, user_id, after, MAX_SHARE_EVENTS_PAGE)
        })
        .await
        .map_err(|_| ())?;
        let full = batch.events.len() as u32 == MAX_SHARE_EVENTS_PAGE;
        for event in batch.events {
            let seq = event.server_seq.unwrap_or_default();
            send(socket, &ShareServerFrame::Event { event }).await?;
            *cursor = seq;
        }
        if !full {
            return Ok(());
        }
    }
}

async fn run(state: &AppState, mut socket: WebSocket, share_id: &str, user_id: i64, after: u64) {
    let mut live = state.shares.subscribe(share_id);
    let mut cursor = after;
    let (id, user) = (share_id.to_owned(), user_id);
    let Ok(head) = read(state, move |c| page(c, &id, user, 0, 1)).await else {
        return;
    };
    let hello = ShareServerFrame::Hello {
        protocol_version: VNEXT_PROTOCOL_VERSION,
        last_server_seq: head.last_server_seq,
    };
    if send(&mut socket, &hello).await.is_err()
        || backfill(state, &mut socket, share_id, user_id, &mut cursor)
            .await
            .is_err()
    {
        return;
    }
    let mut revalidate = tokio::time::interval(REVALIDATE_EVERY);
    revalidate.tick().await;
    loop {
        let result = tokio::select! {
            item = live.recv() => match item {
                Ok(frame) => match frame.as_ref() {
                    ShareServerFrame::Event { event } => {
                        let seq = event.server_seq.unwrap_or_default();
                        if seq <= cursor {
                            Ok(())
                        } else if seq == cursor + 1 {
                            let r = send(&mut socket, &frame).await;
                            cursor = seq;
                            r
                        } else {
                            backfill(state, &mut socket, share_id, user_id, &mut cursor).await
                        }
                    }
                    other => send(&mut socket, other).await,
                },
                Err(RecvError::Lagged(_)) => {
                    backfill(state, &mut socket, share_id, user_id, &mut cursor).await
                }
                Err(RecvError::Closed) => Err(()),
            },
            _ = revalidate.tick() => {
                let (id, user) = (share_id.to_owned(), user_id);
                read(state, move |c| super::grant(c, &id, user).map(|_| ()))
                    .await
                    .map_err(|_| ())
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ShareClientFrame>(&text) {
                        Ok(ShareClientFrame::Ping) => send(&mut socket, &ShareServerFrame::Pong).await,
                        Err(_) => Err(()),
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => Ok(()),
                _ => Err(()),
            },
        };
        if result.is_err() {
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    }
}
