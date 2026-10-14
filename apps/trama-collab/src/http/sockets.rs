//! Client and runner WebSocket endpoints.
//!
//! Client sockets subscribe to the live channel *before* backfilling from the
//! durable log and drop any live event whose `seq` was already delivered, so
//! catch-up joins live delivery without a gap or a duplicate. A live event
//! that skips ahead (or a lagged channel) triggers another membership-checked
//! backfill from the log.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::{
        Path, Query, State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code},
    },
    http::{HeaderMap, header::AUTHORIZATION},
    response::Response,
};
use tokio::sync::broadcast::error::RecvError;
use trama_collab_protocol::{
    COLLAB_PROTOCOL_VERSION, ClientFrame, PresenceState, RunnerClientFrame, RunnerServerFrame,
    ServerFrame,
};

use crate::{
    AppState, AuthError, Identity, bearer_token,
    collab::{
        self, AfterQuery, ClaimOutcome, MAX_PAGE, MAX_WS_MESSAGE_BYTES, RunnerIdentity, read,
        validate_draft, validate_thread_id, write,
    },
    http::live::Live,
};

/// Sessions, memberships and runner credentials are also re-validated on
/// this interval, in addition to the immediate change notifications.
const REVALIDATE_EVERY: Duration = Duration::from_secs(30);
const CLIENT_FRAME_BUDGET: u32 = 100;
const RUNNER_FRAME_BUDGET: u32 = 2_000;
const FRAME_WINDOW: Duration = Duration::from_secs(10);

/// Fixed-window inbound frame budget per connection.
struct FrameBudget {
    limit: u32,
    used: u32,
    window_start: Instant,
}

impl FrameBudget {
    fn new(limit: u32) -> Self {
        Self {
            limit,
            used: 0,
            window_start: Instant::now(),
        }
    }

    fn allow(&mut self) -> bool {
        if self.window_start.elapsed() >= FRAME_WINDOW {
            self.window_start = Instant::now();
            self.used = 0;
        }
        self.used += 1;
        self.used <= self.limit
    }
}

async fn send_json<T: serde::Serialize>(socket: &mut WebSocket, frame: &T) -> Result<(), ()> {
    let text = serde_json::to_string(frame).map_err(|_| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

// ---------------------------------------------------------------------------
// Client socket
// ---------------------------------------------------------------------------

pub(crate) async fn thread_socket(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Query(query): Query<AfterQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, AuthError> {
    let token = bearer_token(headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()))?;
    let identity = state.auth.authenticate(token.clone()).await?;
    validate_thread_id(&thread_id)?;
    if query.after < 0 {
        return Err(AuthError::InvalidInput);
    }
    // Membership check; a cursor beyond the log head is rejected rather than
    // silently skipping the events that will later fill that range.
    let (thread, user_id) = (thread_id.clone(), identity.user_id);
    let head = read(&state, move |c| {
        collab::member_events(c, &thread, user_id, 0, 0)
    })
    .await?;
    if query.after > head.last_seq {
        return Err(AuthError::InvalidInput);
    }
    let slot = state.hub.open_socket(identity.user_id)?;
    Ok(upgrade
        .max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let _slot = slot;
            let hub = state.hub.clone();
            client_session(&state, socket, &identity, token, &thread_id, query.after).await;
            hub.release(&thread_id);
        }))
}

enum Stop {
    /// Access is gone: tell the client, then close.
    Revoked,
    /// Socket closed or failed; nothing more to send.
    Closed,
}

/// Sends every event after `*cursor` from the log, advancing the cursor.
/// Fails with `Revoked` if the user is no longer a member.
async fn backfill(
    state: &AppState,
    socket: &mut WebSocket,
    identity: &Identity,
    thread_id: &str,
    cursor: &mut i64,
) -> Result<(), Stop> {
    loop {
        let (thread, user_id, after) = (thread_id.to_owned(), identity.user_id, *cursor);
        let page = read(state, move |c| {
            collab::member_events(c, &thread, user_id, after, MAX_PAGE)
        })
        .await
        .map_err(|_| Stop::Revoked)?;
        let full = page.events.len() as i64 == MAX_PAGE;
        for event in page.events {
            let seq = event.seq;
            send_json(socket, &ServerFrame::Event { event })
                .await
                .map_err(|_| Stop::Closed)?;
            *cursor = seq;
        }
        if !full {
            return Ok(());
        }
    }
}

async fn still_allowed(
    state: &AppState,
    token: &str,
    identity: &Identity,
    thread_id: &str,
) -> bool {
    match state.auth.authenticate(token.to_owned()).await {
        Ok(current) if current.user_id == identity.user_id => {}
        _ => return false,
    }
    let (thread, user_id) = (thread_id.to_owned(), identity.user_id);
    read(state, move |c| collab::is_member(c, &thread, user_id))
        .await
        .unwrap_or(false)
}

async fn client_session(
    state: &AppState,
    mut socket: WebSocket,
    identity: &Identity,
    token: String,
    thread_id: &str,
    after: i64,
) {
    let mut joined = false;
    let stop = client_loop(
        state,
        &mut socket,
        identity,
        &token,
        thread_id,
        after,
        &mut joined,
    )
    .await;
    // Only a connection that joined presence may leave it; otherwise it
    // would decrement the count held by the user's other sockets.
    if joined {
        state.hub.presence_leave(thread_id, &identity.username);
    }
    if let Stop::Revoked = stop {
        let _ = send_json(&mut socket, &ServerFrame::Revoked).await;
        close(&mut socket, close_code::POLICY, "access revoked").await;
    }
}

async fn client_loop(
    state: &AppState,
    socket: &mut WebSocket,
    identity: &Identity,
    token: &str,
    thread_id: &str,
    after: i64,
    joined: &mut bool,
) -> Stop {
    let hub = &state.hub;
    // Subscribe first: anything committed from now on is either in the
    // backfill or on this channel (or both, and then deduplicated by seq).
    let mut live = hub.subscribe(thread_id);
    let mut credentials = hub.credentials();
    let hello = ServerFrame::Hello {
        protocol_version: COLLAB_PROTOCOL_VERSION,
        thread_id: thread_id.to_owned(),
        username: identity.username.clone(),
    };
    if send_json(socket, &hello).await.is_err() {
        return Stop::Closed;
    }
    let mut cursor = after;
    if let Err(stop) = backfill(state, socket, identity, thread_id, &mut cursor).await {
        return stop;
    }
    if send_json(socket, &ServerFrame::CaughtUp { last_seq: cursor })
        .await
        .is_err()
    {
        return Stop::Closed;
    }
    // Snapshot of ephemeral state, limited to current members.
    let (thread, user_id) = (thread_id.to_owned(), identity.user_id);
    let Ok(Some(members)) = read(state, move |c| {
        if !collab::is_member(c, &thread, user_id)? {
            return Ok(None);
        }
        Ok(Some(collab::member_names(c, &thread)?))
    })
    .await
    else {
        return Stop::Revoked;
    };
    hub.presence_join(thread_id, &identity.username);
    *joined = true;
    let snapshot = hub
        .draft_snapshot(thread_id)
        .into_iter()
        .chain(hub.presence_snapshot(thread_id));
    for frame in snapshot {
        let username = match &frame {
            ServerFrame::Draft { username, .. } | ServerFrame::Presence { username, .. } => {
                username
            }
            _ => continue,
        };
        if members.contains(username) && send_json(socket, &frame).await.is_err() {
            return Stop::Closed;
        }
    }

    let mut budget = FrameBudget::new(CLIENT_FRAME_BUDGET);
    let mut revalidate = tokio::time::interval(REVALIDATE_EVERY);
    revalidate.tick().await;
    loop {
        tokio::select! {
            biased;
            item = live.recv() => match item {
                Ok(item) => match &*item {
                    Live::Event(event) if event.seq <= cursor => {}
                    Live::Event(event) if event.seq == cursor + 1 => {
                        let seq = event.seq;
                        let frame = ServerFrame::Event { event: event.clone() };
                        if send_json(socket, &frame).await.is_err() {
                            return Stop::Closed;
                        }
                        cursor = seq;
                    }
                    // Skipped ahead: re-read the gap from the log.
                    Live::Event(_) => {
                        if let Err(stop) = backfill(state, socket, identity, thread_id, &mut cursor).await {
                            return stop;
                        }
                    }
                    Live::Frame(frame) => {
                        if send_json(socket, frame).await.is_err() {
                            return Stop::Closed;
                        }
                    }
                    Live::AccessChanged => {
                        if !still_allowed(state, token, identity, thread_id).await {
                            return Stop::Revoked;
                        }
                    }
                },
                Err(RecvError::Lagged(_)) => {
                    if let Err(stop) = backfill(state, socket, identity, thread_id, &mut cursor).await {
                        return stop;
                    }
                }
                Err(RecvError::Closed) => return Stop::Closed,
            },
            _ = credentials.recv() => {
                if !still_allowed(state, token, identity, thread_id).await {
                    return Stop::Revoked;
                }
            }
            _ = revalidate.tick() => {
                if !still_allowed(state, token, identity, thread_id).await {
                    return Stop::Revoked;
                }
            }
            incoming = socket.recv() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Binary(_))) => {
                        let _ = send_json(socket, &ServerFrame::Error { error: "binary frames are not supported".into() }).await;
                        close(socket, close_code::UNSUPPORTED, "binary frames are not supported").await;
                        return Stop::Closed;
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return Stop::Closed,
                };
                if !budget.allow() {
                    let _ = send_json(socket, &ServerFrame::Error { error: "too many frames".into() }).await;
                    close(socket, close_code::POLICY, "too many frames").await;
                    return Stop::Closed;
                }
                let Ok(frame) = serde_json::from_str::<ClientFrame>(&text) else {
                    let _ = send_json(socket, &ServerFrame::Error { error: "invalid frame".into() }).await;
                    close(socket, close_code::INVALID, "invalid frame").await;
                    return Stop::Closed;
                };
                match frame {
                    ClientFrame::Ping => {
                        if send_json(socket, &ServerFrame::Pong).await.is_err() {
                            return Stop::Closed;
                        }
                    }
                    ClientFrame::Draft { text } => {
                        if validate_draft(&text).is_err() {
                            let _ = send_json(socket, &ServerFrame::Error { error: "invalid draft".into() }).await;
                            close(socket, close_code::POLICY, "invalid draft").await;
                            return Stop::Closed;
                        }
                        hub.set_draft(thread_id, &identity.username, text, state.auth.now());
                    }
                    ClientFrame::Presence { state: presence } => match presence {
                        PresenceState::Active | PresenceState::Idle => {
                            hub.presence_set(thread_id, &identity.username, presence);
                        }
                        PresenceState::Online | PresenceState::Offline => {
                            if send_json(socket, &ServerFrame::Error { error: "presence state is set by the server".into() }).await.is_err() {
                                return Stop::Closed;
                            }
                        }
                    },
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Runner socket
// ---------------------------------------------------------------------------

pub(crate) async fn runner_socket(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, AuthError> {
    let token = collab::runner_token(headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()))?;
    let runner = read(&state, move |c| collab::lookup_runner(c, &token)).await?;
    let slot = state.hub.open_socket(runner.owner.user_id)?;
    // Claims are bound to this connection: when it ends, its open claims
    // become ambiguous instead of being re-offered.
    let connection_id = collab::random_hex(16)?;
    Ok(upgrade
        .max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |mut socket| async move {
            let _slot = slot;
            let revoked = runner_loop(&state, &mut socket, &runner, &connection_id).await;
            if revoked {
                let _ = send_json(&mut socket, &RunnerServerFrame::Revoked).await;
                close(&mut socket, close_code::POLICY, "runner revoked").await;
            }
            let now = state.auth.now();
            let conn = connection_id.clone();
            let _ = write(&state, move |c, outbox| {
                collab::abandon_connection(c, outbox, &conn, now)
            })
            .await;
            state.hub.release_turns(runner.owner.user_id);
        }))
}

/// Returns `true` when the loop ended because the credential was revoked.
async fn runner_loop(
    state: &AppState,
    socket: &mut WebSocket,
    runner: &RunnerIdentity,
    connection_id: &str,
) -> bool {
    let hub = &state.hub;
    let owner_id = runner.owner.user_id;
    // Subscribe before listing so no turn falls between the two.
    let mut turns = hub.subscribe_turns(owner_id);
    let mut credentials = hub.credentials();
    let Ok(pending) = read(state, move |c| collab::pending_turns(c, owner_id)).await else {
        return false;
    };
    let hello = RunnerServerFrame::Hello {
        protocol_version: COLLAB_PROTOCOL_VERSION,
        runner_id: runner.runner_id,
        owner: runner.owner.username.clone(),
    };
    if send_json(socket, &hello).await.is_err() {
        return false;
    }
    for turn in pending {
        if send_json(socket, &RunnerServerFrame::CommandAvailable { turn })
            .await
            .is_err()
        {
            return false;
        }
    }
    let runner_id = runner.runner_id;
    let mut budget = FrameBudget::new(RUNNER_FRAME_BUDGET);
    let mut revalidate = tokio::time::interval(REVALIDATE_EVERY);
    revalidate.tick().await;
    loop {
        tokio::select! {
            biased;
            _ = credentials.recv() => {
                if !read(state, move |c| collab::runner_active(c, runner_id)).await.unwrap_or(false) {
                    return true;
                }
            }
            _ = revalidate.tick() => {
                if !read(state, move |c| collab::runner_active(c, runner_id)).await.unwrap_or(false) {
                    return true;
                }
            }
            turn = turns.recv() => match turn {
                Ok(turn) => {
                    if send_json(socket, &RunnerServerFrame::CommandAvailable { turn }).await.is_err() {
                        return false;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let Ok(pending) = read(state, move |c| collab::pending_turns(c, owner_id)).await else {
                        return false;
                    };
                    for turn in pending {
                        if send_json(socket, &RunnerServerFrame::CommandAvailable { turn }).await.is_err() {
                            return false;
                        }
                    }
                }
                Err(RecvError::Closed) => return false,
            },
            incoming = socket.recv() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Binary(_))) => {
                        close(socket, close_code::UNSUPPORTED, "binary frames are not supported").await;
                        return false;
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return false,
                };
                if !budget.allow() {
                    let _ = send_json(socket, &RunnerServerFrame::Error { error: "too many frames".into() }).await;
                    close(socket, close_code::POLICY, "too many frames").await;
                    return false;
                }
                let Ok(frame) = serde_json::from_str::<RunnerClientFrame>(&text) else {
                    let _ = send_json(socket, &RunnerServerFrame::Error { error: "invalid frame".into() }).await;
                    close(socket, close_code::INVALID, "invalid frame").await;
                    return false;
                };
                let reply = runner_frame(state, runner, connection_id, frame).await;
                if send_json(socket, &reply).await.is_err() {
                    return false;
                }
            }
        }
    }
}

async fn runner_frame(
    state: &AppState,
    runner: &RunnerIdentity,
    connection_id: &str,
    frame: RunnerClientFrame,
) -> RunnerServerFrame {
    let now = state.auth.now();
    let runner = Arc::new(runner.clone());
    let conn = connection_id.to_owned();
    let internal = || RunnerServerFrame::Error {
        error: "internal error".into(),
    };
    match frame {
        RunnerClientFrame::Ping => RunnerServerFrame::Pong,
        RunnerClientFrame::Claim { command_id } => {
            let id = command_id.clone();
            match write(state, move |c, outbox| {
                collab::claim_turn(c, outbox, &runner, &conn, &id, now)
            })
            .await
            {
                Ok(ClaimOutcome::Claimed(turn)) => RunnerServerFrame::Claimed { turn },
                Ok(ClaimOutcome::Rejected(reason)) => RunnerServerFrame::ClaimRejected {
                    command_id,
                    reason: reason.into(),
                },
                Err(_) => internal(),
            }
        }
        RunnerClientFrame::Report {
            command_id,
            kind,
            payload,
        } => {
            let id = command_id.clone();
            match write(state, move |c, outbox| {
                collab::report_turn(c, outbox, &runner, &conn, &id, kind, payload, now)
            })
            .await
            {
                Ok(Ok(seq)) => RunnerServerFrame::Reported { command_id, seq },
                Ok(Err(reason)) => RunnerServerFrame::Error {
                    error: reason.into(),
                },
                Err(_) => internal(),
            }
        }
        RunnerClientFrame::Finish {
            command_id,
            outcome,
            result,
        } => {
            let id = command_id.clone();
            match write(state, move |c, outbox| {
                collab::finish_turn(c, outbox, &runner, &conn, &id, outcome, result, now)
            })
            .await
            {
                Ok(Ok((state, seq))) => RunnerServerFrame::Finished {
                    command_id,
                    state,
                    seq,
                },
                Ok(Err(reason)) => RunnerServerFrame::Error {
                    error: reason.into(),
                },
                Err(_) => internal(),
            }
        }
    }
}
