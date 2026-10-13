//! One resumable stream task per share: connect with the persisted cursor,
//! apply events in order, reconnect with backoff, stop when the connection
//! generation changes. A gap or reject is logged and the stream resumes
//! from the stored cursor (the server never skips a `server_seq`).

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use trama_collab_protocol::vnext::api::{ShareClientFrame, ShareServerFrame};
use trama_collab_protocol::vnext::replay::Admission;
use trama_db::ShareStatus;
use trama_domain::share::ShareId;

use super::super::CollabService;
use super::ShareClient;

const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const PING_EVERY: Duration = Duration::from_secs(25);

enum End {
    /// Reconnect after a backoff.
    Retry,
    /// Generation changed or access lost.
    Stop,
}

impl CollabService {
    /// Starts streams for every open share bound to the connected server.
    pub(crate) async fn start_share_streams(&self, generation: u64) {
        let Some(connection) = self.connection(Some(generation)) else {
            return;
        };
        if connection.session_token.is_none() {
            return;
        }
        let shares = match self.db().shares().await {
            Ok(shares) => shares,
            Err(err) => {
                crate::log::warn(format!("could not list shares: {err}"));
                return;
            }
        };
        let servers = self.db().collaboration_servers().await.unwrap_or_default();
        for share in shares {
            let on_this_server = share.server_id.as_ref().is_some_and(|id| {
                servers
                    .iter()
                    .any(|s| &s.server_id == id && s.base_url == connection.endpoint.url())
            });
            if on_this_server && !matches!(share.status, ShareStatus::Closed | ShareStatus::Paused)
            {
                self.ensure_share_stream(&share.share_id.0, generation);
            }
        }
    }

    pub(crate) fn ensure_share_stream(&self, share_id: &str, generation: u64) {
        let router = self.share_events();
        let mut streams = router.streams.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((g, task)) = streams.get(share_id)
            && *g == generation
            && !task.is_finished()
        {
            return;
        }
        let service = self.clone();
        let id = share_id.to_owned();
        let task = tokio::spawn(async move { service.run_share_stream(id, generation).await });
        if let Some((_, old)) = streams.insert(share_id.to_owned(), (generation, task)) {
            old.abort();
        }
    }

    async fn run_share_stream(self, share_id: String, generation: u64) {
        let mut backoff = BACKOFF_START;
        loop {
            let Some(connection) = self.connection(Some(generation)) else {
                return;
            };
            let Ok(client) = ShareClient::from_connection(&connection) else {
                return;
            };
            match self.stream_once(&client, &share_id, generation).await {
                End::Stop => return,
                End::Retry => {}
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    async fn stream_once(&self, client: &ShareClient, share_id: &str, generation: u64) -> End {
        let cursor = match self
            .db()
            .share_stream_cursor(&ShareId::from(share_id))
            .await
        {
            Ok(cursor) => cursor,
            Err(_) => return End::Retry,
        };
        let mut ws = match client.stream(share_id, cursor).await {
            Ok(ws) => ws,
            Err(crate::error::DaemonError::NotFound(_)) => return End::Stop,
            Err(err) => {
                crate::log::warn(format!("share {share_id}: stream unavailable: {err}"));
                return End::Retry;
            }
        };
        let mut ping = tokio::time::interval(PING_EVERY);
        loop {
            if self.connection(Some(generation)).is_none() {
                return End::Stop;
            }
            let message = tokio::select! {
                message = ws.next() => message,
                _ = ping.tick() => {
                    let frame = serde_json::to_string(&ShareClientFrame::Ping).unwrap_or_default();
                    if ws.send(Message::Text(frame.into())).await.is_err() {
                        return End::Retry;
                    }
                    continue;
                }
            };
            let text = match message {
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                _ => return End::Retry,
            };
            let Ok(frame) = serde_json::from_str::<ShareServerFrame>(&text) else {
                return End::Retry;
            };
            if let ShareServerFrame::Event { event } = frame {
                match self.apply_share_event(event).await {
                    Ok(Admission::Accept | Admission::Duplicate) => {}
                    Ok(other) => {
                        crate::log::warn(format!("share {share_id}: event not applied: {other:?}"));
                        return End::Retry;
                    }
                    Err(err) => {
                        crate::log::warn(format!("share {share_id}: {err}"));
                        return End::Retry;
                    }
                }
            }
        }
    }
}
