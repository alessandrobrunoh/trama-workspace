//! The runner socket: claim and run this user's own turns (ADR 0008).

use super::http::CollabError;
use super::{CollabService, Connection, truncate, turn_state};
use ::http::Method;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use trama_collab_protocol::{
    EventEnvelope, EventKind, EventsPage, PendingTurn, RunnerClientFrame, RunnerReportKind,
    RunnerServerFrame, TurnOutcome,
};
use trama_db::{CollabLink, CollabTurn};
use trama_domain::agent::AgentEvent;
use trama_domain::message::Role;
use trama_protocol::{ChangeBase, CollabRunnerState, DaemonMessage};

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_secs(25);
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const IDLE_POLL: Duration = Duration::from_millis(100);
/// Text deltas are coalesced into one report per window (or per 8 KiB) so a
/// streamed answer does not become thousands of durable events.
const DELTA_WINDOW: Duration = Duration::from_millis(250);
const DELTA_FLUSH_BYTES: usize = 8 * 1024;
const MAX_REPORT_TEXT_BYTES: usize = 48 * 1024;

enum Ended {
    /// Reconnect after a backoff.
    Retry,
    /// Do not reconnect; the reason is shown in the status.
    Stop(String),
}

/// Keep the runner connected for this generation's credentials.
pub(super) async fn run(service: CollabService, generation: u64) {
    let mut backoff = Duration::from_millis(250);
    loop {
        let Some(connection) = service.connection(Some(generation)) else {
            return;
        };
        let Some(token) = connection.runner_token.clone() else {
            return;
        };
        service.set_runner_state(generation, CollabRunnerState::Connecting, None);
        let ended = match connection.endpoint.websocket("/runner/ws", &token).await {
            Ok(ws) => {
                let ended = serve(&service, &connection, ws).await;
                if matches!(ended, Ended::Retry) {
                    backoff = Duration::from_millis(250);
                }
                ended
            }
            Err(CollabError::Unauthorized) => Ended::Stop(
                "the runner token was rejected (revoked or unknown); connect again".into(),
            ),
            Err(err) => {
                service.note_error(generation, err.to_string());
                Ended::Retry
            }
        };
        service.set_runner_tx(generation, None);
        match ended {
            Ended::Stop(reason) => {
                crate::log::warn(format!("collab runner stopped: {reason}"));
                service.set_runner_state(generation, CollabRunnerState::Failed, Some(reason));
                return;
            }
            Ended::Retry => {
                service.set_runner_state(generation, CollabRunnerState::Connecting, None);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

fn parse(message: &WsMessage) -> Option<RunnerServerFrame> {
    match message {
        WsMessage::Text(text) => serde_json::from_str(text.as_str()).ok(),
        _ => None,
    }
}

async fn serve(service: &CollabService, connection: &Connection, ws: super::http::Ws) -> Ended {
    let (mut sink, mut stream) = ws.split();
    // The hello names the credential's owner. A token of anyone else is
    // refused before a single turn is looked at.
    let owner = match tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(message))) => match parse(&message) {
            Some(RunnerServerFrame::Hello { owner, .. }) => owner,
            _ => return Ended::Retry,
        },
        _ => return Ended::Retry,
    };
    if !owner.eq_ignore_ascii_case(&connection.username) {
        let _ = sink.close().await;
        return Ended::Stop(format!(
            "the runner token belongs to `{owner}`, not to `{}`; this daemon only runs its own \
             user's turns",
            connection.username
        ));
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<RunnerClientFrame>();
    if !service.set_runner_tx(connection.generation, Some(tx)) {
        return Ended::Stop("disconnected".into());
    }
    service.set_runner_state(connection.generation, CollabRunnerState::Online, None);
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    loop {
        tokio::select! {
            outgoing = rx.recv() => {
                let Some(frame) = outgoing else { return Ended::Retry };
                let Ok(text) = serde_json::to_string(&frame) else { continue };
                if sink.send(WsMessage::Text(text.into())).await.is_err() {
                    return Ended::Retry;
                }
            }
            _ = ping.tick() => {
                let ping = serde_json::to_string(&RunnerClientFrame::Ping).unwrap_or_default();
                if sink.send(WsMessage::Text(ping.into())).await.is_err() {
                    return Ended::Retry;
                }
            }
            incoming = stream.next() => {
                let message = match incoming {
                    Some(Ok(message)) => message,
                    _ => return Ended::Retry,
                };
                if matches!(message, WsMessage::Close(_)) {
                    return Ended::Retry;
                }
                match parse(&message) {
                    Some(RunnerServerFrame::CommandAvailable { turn }) => {
                        let service = service.clone();
                        let connection = connection.clone();
                        tokio::spawn(async move { handle_available(service, connection, turn).await });
                    }
                    Some(RunnerServerFrame::Claimed { turn }) => {
                        service.resolve_claim(&turn.command_id, Ok(()));
                    }
                    Some(RunnerServerFrame::ClaimRejected { command_id, reason }) => {
                        service.resolve_claim(&command_id, Err(reason));
                    }
                    Some(RunnerServerFrame::Finished { command_id, .. }) => {
                        service.resolve_finish(&command_id);
                    }
                    Some(RunnerServerFrame::Error { error }) => {
                        crate::log::warn(format!("collab runner: {error}"));
                    }
                    Some(RunnerServerFrame::Revoked) => {
                        return Ended::Stop("the runner credential was revoked".into());
                    }
                    Some(RunnerServerFrame::Hello { .. })
                    | Some(RunnerServerFrame::Reported { .. })
                    | Some(RunnerServerFrame::Pong)
                    | None => {}
                }
            }
        }
    }
}

/// Why a `command_available` turn is not claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Not a turn this daemon submitted (another machine, client or user).
    NotSubmittedHere,
    /// The local thread is no longer linked to that shared thread.
    NotLinked,
    /// The durable event could not be read yet; try again later.
    Unverified,
    /// The service's view contradicts what this daemon submitted.
    Mismatch(String),
}

/// Client-side half of ADR 0008: claim only a turn this daemon submitted,
/// in the linked thread, with the text it submitted, whose durable
/// `message_appended` event is authored by the signed-in user.
pub(crate) fn check_claim(
    username: &str,
    server_url: &str,
    local: Option<&CollabTurn>,
    link: Option<&CollabLink>,
    turn: &PendingTurn,
    event: Option<&EventEnvelope>,
) -> Result<(), Refusal> {
    let Some(local) = local.filter(|local| local.state == turn_state::SUBMITTED) else {
        return Err(Refusal::NotSubmittedHere);
    };
    if local.collab_thread_id != turn.thread_id {
        return Err(Refusal::Mismatch(
            "offered for another shared thread".into(),
        ));
    }
    if local.text != turn.text {
        return Err(Refusal::Mismatch(
            "its text differs from what was sent".into(),
        ));
    }
    match link {
        Some(link)
            if link.thread_id == local.thread_id
                && link.server_url == server_url
                && link.collab_thread_id == turn.thread_id => {}
        _ => return Err(Refusal::NotLinked),
    }
    let Some(event) = event else {
        return Err(Refusal::Unverified);
    };
    if event.seq != turn.seq
        || event.kind != EventKind::MessageAppended
        || event.command_id.as_deref() != Some(turn.command_id.as_str())
    {
        return Err(Refusal::Mismatch(
            "the shared log does not match the turn".into(),
        ));
    }
    match event.author.as_deref() {
        Some(author) if author.eq_ignore_ascii_case(username) => Ok(()),
        other => Err(Refusal::Mismatch(format!(
            "authored by `{}`, not by `{username}`",
            other.unwrap_or("nobody")
        ))),
    }
}

async fn fetch_event(connection: &Connection, turn: &PendingTurn) -> Option<EventEnvelope> {
    let session = connection.session_token.as_deref()?;
    let after = turn.seq.checked_sub(1)?;
    if after < 0
        || !turn
            .thread_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return None;
    }
    let page: EventsPage = connection
        .endpoint
        .call(
            Method::GET,
            &format!("/threads/{}/events?after={after}&limit=1", turn.thread_id),
            Some(session),
            None::<&()>,
        )
        .await
        .ok()?;
    page.events.into_iter().next()
}

async fn handle_available(service: CollabService, connection: Connection, turn: PendingTurn) {
    if !service.begin_turn(&turn.command_id) {
        return;
    }
    claim_and_run(&service, &connection, &turn).await;
    service.end_turn(&turn.command_id);
}

async fn claim_and_run(service: &CollabService, connection: &Connection, turn: &PendingTurn) {
    let db = service.db().clone();
    let local = db.collab_turn(&turn.command_id).await.ok().flatten();
    // Cheap pre-check: most offers of other machines stop here, before any
    // lock or request.
    if local
        .as_ref()
        .is_none_or(|local| local.state != turn_state::SUBMITTED)
    {
        return;
    }
    let Some(local) = local else { return };
    let lock = service.thread_lock(&local.thread_id);
    let _serialized = lock.lock().await;
    // Wait for the thread to be free (an earlier turn, a migration).
    while service.sessions().thread_busy(&local.thread_id) {
        if service.connection(Some(connection.generation)).is_none() {
            return;
        }
        tokio::time::sleep(IDLE_POLL).await;
    }
    let local = db.collab_turn(&turn.command_id).await.ok().flatten();
    let link = match &local {
        Some(local) => db.collab_link(&local.thread_id).await.ok().flatten(),
        None => None,
    };
    let event = fetch_event(connection, turn).await;
    if let Err(refusal) = check_claim(
        &connection.username,
        connection.endpoint.url(),
        local.as_ref(),
        link.as_ref(),
        turn,
        event.as_ref(),
    ) {
        match refusal {
            Refusal::Mismatch(reason) => {
                crate::log::warn(format!(
                    "refusing to run shared turn {}: {reason}",
                    turn.command_id
                ));
                let _ = db
                    .transition_collab_turn(
                        &turn.command_id,
                        &[turn_state::SUBMITTED],
                        turn_state::REJECTED,
                    )
                    .await;
            }
            Refusal::NotSubmittedHere | Refusal::NotLinked | Refusal::Unverified => {}
        }
        return;
    }
    let Some(local) = local else { return };
    // Persisted before the claim frame: if this process dies from here on,
    // the next start reports the turn as ambiguous instead of rerunning it.
    if !db
        .transition_collab_turn(
            &turn.command_id,
            &[turn_state::SUBMITTED],
            turn_state::CLAIMING,
        )
        .await
        .unwrap_or(false)
    {
        return;
    }
    let reply = service.expect_claim(&turn.command_id);
    let sent = service.send_runner(RunnerClientFrame::Claim {
        command_id: turn.command_id.clone(),
    });
    let answer = if sent {
        match tokio::time::timeout(REPLY_TIMEOUT, reply).await {
            Ok(Ok(Ok(()))) => ClaimAnswer::Claimed,
            Ok(Ok(Err(reason))) => ClaimAnswer::Rejected(reason),
            _ => ClaimAnswer::NoAnswer,
        }
    } else {
        ClaimAnswer::NoAnswer
    };
    match answer {
        ClaimAnswer::Claimed => {}
        ClaimAnswer::Rejected(reason) => {
            crate::log::warn(format!(
                "shared turn {} not claimed: {reason}",
                turn.command_id
            ));
            let _ = db
                .transition_collab_turn(
                    &turn.command_id,
                    &[turn_state::CLAIMING],
                    turn_state::REJECTED,
                )
                .await;
            return;
        }
        ClaimAnswer::NoAnswer => {
            // The claim is bound to the lost connection, so the service
            // either still has the turn pending (offered again on
            // reconnect) or marked it ambiguous. Either way nothing ran.
            let _ = db
                .transition_collab_turn(
                    &turn.command_id,
                    &[turn_state::CLAIMING],
                    turn_state::SUBMITTED,
                )
                .await;
            return;
        }
    }
    let _ = db
        .transition_collab_turn(
            &turn.command_id,
            &[turn_state::CLAIMING],
            turn_state::CLAIMED,
        )
        .await;
    run_claimed(service, &local).await;
}

enum ClaimAnswer {
    Claimed,
    Rejected(String),
    NoAnswer,
}

/// Collects a turn's broadcasts into runner reports.
struct Reporter<'a> {
    service: &'a CollabService,
    command_id: String,
    pending_text: String,
    pending_since: Option<Instant>,
    completed: bool,
    failure: Option<String>,
    lost: bool,
}

impl Reporter<'_> {
    fn report(&mut self, kind: RunnerReportKind, payload: Value) {
        if !self.service.send_runner(RunnerClientFrame::Report {
            command_id: self.command_id.clone(),
            kind,
            payload,
        }) {
            self.lost = true;
        }
    }

    fn flush_text(&mut self) {
        if self.pending_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending_text);
        self.pending_since = None;
        self.report(
            RunnerReportKind::AgentEvent,
            json!({ "type": "event", "event": AgentEvent::TextDelta { text } }),
        );
    }

    fn tick(&mut self) {
        if self
            .pending_since
            .is_some_and(|since| since.elapsed() >= DELTA_WINDOW)
        {
            self.flush_text();
        }
    }

    fn on_message(&mut self, message: DaemonMessage) {
        match message {
            DaemonMessage::AgentEventEnvelope { event, .. } => self.on_event(event),
            DaemonMessage::MessageAppended { message, .. } => {
                let text = truncate(&message.text(), MAX_REPORT_TEXT_BYTES);
                let payload = match message.role {
                    Role::Assistant => json!({ "type": "message", "text": text }),
                    Role::System => json!({ "type": "notice", "text": text }),
                    // The turn's own prompt is already in the shared log.
                    Role::User | Role::Tool => return,
                };
                self.flush_text();
                self.report(RunnerReportKind::AgentEvent, payload);
            }
            _ => {}
        }
    }

    /// Only output that is safe to replicate leaves this machine: text,
    /// tool names (not their inputs or outputs) and the turn's end. Session
    /// ids, billing source, usage and approvals stay local.
    fn on_event(&mut self, event: AgentEvent) {
        let shared = match event {
            AgentEvent::TextDelta { text } => {
                self.pending_text.push_str(&text);
                self.pending_since.get_or_insert_with(Instant::now);
                if self.pending_text.len() >= DELTA_FLUSH_BYTES {
                    self.flush_text();
                }
                return;
            }
            AgentEvent::ToolCallStarted {
                tool_use_id, name, ..
            } => AgentEvent::ToolCallStarted {
                tool_use_id,
                name,
                input_json: Value::Null,
            },
            AgentEvent::ToolCallFinished {
                tool_use_id,
                is_error,
                ..
            } => AgentEvent::ToolCallFinished {
                tool_use_id,
                output_json: Value::Null,
                is_error,
            },
            event @ (AgentEvent::SubagentStarted { .. } | AgentEvent::SubagentFinished { .. }) => {
                event
            }
            AgentEvent::TurnCompleted {
                num_turns,
                cost_usd_micros: _,
            } => {
                self.completed = true;
                AgentEvent::TurnCompleted {
                    num_turns,
                    cost_usd_micros: None,
                }
            }
            AgentEvent::Failed { message } => {
                let message = truncate(&message, 2048);
                self.failure = Some(message.clone());
                AgentEvent::Failed { message }
            }
            _ => return,
        };
        self.flush_text();
        self.report(
            RunnerReportKind::AgentEvent,
            json!({ "type": "event", "event": shared }),
        );
    }
}

async fn run_claimed(service: &CollabService, local: &CollabTurn) {
    let sessions = service.sessions().clone();
    let thread_id = local.thread_id.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let watch = sessions.watch_thread(&thread_id, tx);
    let mut reporter = Reporter {
        service,
        command_id: local.command_id.clone(),
        pending_text: String::new(),
        pending_since: None,
        completed: false,
        failure: None,
        lost: false,
    };
    match sessions
        .send_prompt(thread_id.clone(), local.text.clone())
        .await
    {
        Err(err) => {
            let message = format!("The shared turn could not start here: {err}");
            reporter.failure = Some(message.clone());
            sessions.append_system_notice(&thread_id, message).await;
        }
        Ok(_) => loop {
            tokio::select! {
                message = rx.recv() => match message {
                    Some(message) => reporter.on_message(message),
                    None => break,
                },
                _ = tokio::time::sleep(IDLE_POLL) => {}
            }
            reporter.tick();
            if !sessions.turn_running(&thread_id) {
                // Terminal broadcasts are queued before the reservation is
                // released, so draining now sees all of them.
                while let Ok(message) = rx.try_recv() {
                    reporter.on_message(message);
                }
                break;
            }
        },
    }
    // Also drains the failure notice of a turn that never started.
    while let Ok(message) = rx.try_recv() {
        reporter.on_message(message);
    }
    let _ = sessions.unsubscribe(&thread_id, watch);
    reporter.flush_text();
    if reporter.completed {
        if let Some(summary) = changes_summary(service, &thread_id).await {
            reporter.report(RunnerReportKind::ChangesSummary, summary);
        }
    }
    let (outcome, result, state) = if reporter.completed && reporter.failure.is_none() {
        (TurnOutcome::Completed, Value::Null, turn_state::COMPLETED)
    } else {
        let error = reporter
            .failure
            .clone()
            .unwrap_or_else(|| "the turn ended without completing".into());
        (
            TurnOutcome::Failed,
            json!({ "error": error }),
            turn_state::FAILED,
        )
    };
    let finished = if reporter.lost {
        false
    } else {
        let reply = service.expect_finish(&local.command_id);
        service.send_runner(RunnerClientFrame::Finish {
            command_id: local.command_id.clone(),
            outcome,
            result,
        }) && matches!(tokio::time::timeout(REPLY_TIMEOUT, reply).await, Ok(Ok(())))
    };
    if finished {
        let _ = service
            .db()
            .transition_collab_turn(&local.command_id, &[turn_state::CLAIMED], state)
            .await;
    } else {
        service.mark_ambiguous(&local.command_id, &thread_id).await;
    }
}

/// Files touched by the last turn, without contents.
async fn changes_summary(
    service: &CollabService,
    thread_id: &trama_domain::ids::ThreadId,
) -> Option<Value> {
    let repos = service
        .sessions()
        .get_changes(thread_id, ChangeBase::LastTurn)
        .await
        .ok()?;
    let repos: Vec<Value> = repos
        .into_iter()
        .filter(|repo| !repo.files.is_empty())
        .map(|repo| {
            let files: Vec<Value> = repo
                .files
                .iter()
                .take(200)
                .map(|file| {
                    json!({
                        "path": truncate(&file.path, 512),
                        "status": file.status,
                        "additions": file.additions,
                        "deletions": file.deletions,
                    })
                })
                .collect();
            json!({ "title": truncate(&repo.title, 120), "files": files, "file_count": repo.files.len() })
        })
        .collect();
    if repos.is_empty() {
        None
    } else {
        Some(json!({ "repos": repos }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use trama_domain::ids::ThreadId;

    fn turn() -> PendingTurn {
        PendingTurn {
            command_id: "trd.me.1".into(),
            thread_id: "ct1".into(),
            seq: 4,
            text: "run tests".into(),
        }
    }

    fn local(state: &str) -> CollabTurn {
        CollabTurn {
            command_id: "trd.me.1".into(),
            thread_id: ThreadId::from("local"),
            collab_thread_id: "ct1".into(),
            text: "run tests".into(),
            state: state.into(),
            created_at: Utc::now(),
        }
    }

    fn link() -> CollabLink {
        CollabLink {
            thread_id: ThreadId::from("local"),
            server_url: "http://127.0.0.1:1".into(),
            collab_thread_id: "ct1".into(),
            cursor: 0,
            linked_at: Utc::now(),
        }
    }

    fn event(author: &str) -> EventEnvelope {
        EventEnvelope {
            thread_id: "ct1".into(),
            seq: 4,
            kind: EventKind::MessageAppended,
            author: Some(author.into()),
            command_id: Some("trd.me.1".into()),
            created_at: 0,
            payload: json!({"text": "run tests"}),
        }
    }

    #[test]
    fn claims_only_own_submitted_and_authored_turns() {
        let url = "http://127.0.0.1:1";
        let check = |local: Option<&CollabTurn>,
                     link: Option<&CollabLink>,
                     event: Option<&EventEnvelope>| {
            check_claim("alice", url, local, link, &turn(), event)
        };
        let ok_local = local(turn_state::SUBMITTED);
        assert_eq!(
            check(Some(&ok_local), Some(&link()), Some(&event("Alice"))),
            Ok(())
        );

        // Someone else's turn (or another machine's): never submitted here.
        assert_eq!(
            check(None, Some(&link()), Some(&event("alice"))),
            Err(Refusal::NotSubmittedHere)
        );
        // Already handled locally (claimed, ambiguous, ...): never rerun.
        for state in [
            turn_state::CLAIMED,
            turn_state::AMBIGUOUS,
            turn_state::SEND_FAILED,
        ] {
            assert_eq!(
                check(Some(&local(state)), Some(&link()), Some(&event("alice"))),
                Err(Refusal::NotSubmittedHere)
            );
        }
        // The durable event is authored by another user.
        assert!(matches!(
            check(Some(&ok_local), Some(&link()), Some(&event("bob"))),
            Err(Refusal::Mismatch(_))
        ));
        let mut other_kind = event("alice");
        other_kind.kind = EventKind::CommentCreated;
        assert!(matches!(
            check(Some(&ok_local), Some(&link()), Some(&other_kind)),
            Err(Refusal::Mismatch(_))
        ));
        let mut other_command = event("alice");
        other_command.command_id = Some("trd.me.2".into());
        assert!(matches!(
            check(Some(&ok_local), Some(&link()), Some(&other_command)),
            Err(Refusal::Mismatch(_))
        ));
        // Text substituted by the service.
        let mut changed = turn();
        changed.text = "rm -rf /".into();
        assert!(matches!(
            check_claim(
                "alice",
                url,
                Some(&ok_local),
                Some(&link()),
                &changed,
                Some(&event("alice"))
            ),
            Err(Refusal::Mismatch(_))
        ));
        // Unlinked, or linked elsewhere.
        assert_eq!(
            check(Some(&ok_local), None, Some(&event("alice"))),
            Err(Refusal::NotLinked)
        );
        let mut elsewhere = link();
        elsewhere.server_url = "http://127.0.0.1:2".into();
        assert_eq!(
            check(Some(&ok_local), Some(&elsewhere), Some(&event("alice"))),
            Err(Refusal::NotLinked)
        );
        // Cannot verify the author: do not claim.
        assert_eq!(
            check(Some(&ok_local), Some(&link()), None),
            Err(Refusal::Unverified)
        );
    }
}
