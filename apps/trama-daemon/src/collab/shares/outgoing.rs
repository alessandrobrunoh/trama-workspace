//! `history_appended` from local turns (docs/plan/03 §4–§5). On every
//! change signal for a thread mapped in a ready share, a writer/owner
//! device that currently holds the thread's writer lease publishes the
//! thread's new local user/assistant messages (sealed with MLS), in
//! batches of [`MAX_HISTORY_MESSAGES`].
//!
//! A per-thread watermark (last published message id) lives in
//! `collab_settings`, set on create/join to the thread's current end (older
//! history is not replayed as events; an empty thread publishes from its
//! first message). Messages written while this device
//! did not hold the lease are skipped for good (the watermark still moves),
//! so nothing authored without the lease is ever published. Mirrored
//! messages (`collab:` actors) are never re-published.

use chrono::Utc;
use trama_collab_protocol::vnext::bodies::{
    HistoryAppendedBody, HistoryMessage, HistoryRole, MAX_HISTORY_MESSAGES,
};
use trama_collab_protocol::vnext::events::ShareEventKind;
use trama_db::ShareStatus;
use trama_domain::ids::ThreadId;
use trama_domain::message::{Actor, ContentBlock, Message, Role};
use trama_protocol::ShareRole;

use super::super::{CollabService, REMOTE_ACTOR_PREFIX};
use super::flow::role_key;
use super::outbound::Payload;
use crate::error::Result;

fn watermark_key(share_id: &str, gid: &str) -> String {
    format!("share_hist:{share_id}:{gid}")
}

fn shareable(message: &Message) -> Option<HistoryMessage> {
    let role = match message.role {
        Role::User => HistoryRole::User,
        Role::Assistant => HistoryRole::Assistant,
        _ => return None,
    };
    if matches!(&message.actor, Actor::Human { id } if id.starts_with(REMOTE_ACTOR_PREFIX)) {
        return None;
    }
    let text: Vec<&str> = message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if text.is_empty() {
        return None;
    }
    Some(HistoryMessage {
        origin: message.id.to_string(),
        role,
        text: text.join("\n"),
        created_at: message.created_at.to_rfc3339(),
    })
}

impl CollabService {
    /// Starts publishing after the current history of every mapped thread
    /// (called on create and join).
    pub(crate) async fn init_history_marks(&self, share_id: &str) -> Result<()> {
        let share = trama_domain::share::ShareId::from(share_id);
        for row in self.db().share_threads(&share).await? {
            let Some(local) = row.local_thread_id else {
                continue;
            };
            let messages = self.db().list_messages_by_thread(&local.into()).await?;
            let last = messages
                .last()
                .map(|m| m.id.to_string())
                .unwrap_or_default();
            self.db()
                .set_collab_setting(&watermark_key(share_id, &row.thread_gid.0), &last)
                .await?;
        }
        Ok(())
    }

    /// Publishes new local history of `thread` to every share mapping it.
    pub(crate) async fn publish_local_history(&self, thread: &ThreadId) -> Result<usize> {
        if self.e2ee().is_err() || self.connection(None).is_none() {
            return Ok(0);
        }
        let mut published = 0;
        let local = thread.to_string();
        for share in self.db().shares().await? {
            if share.status != ShareStatus::Ready {
                continue;
            }
            let share_id = share.share_id.0.clone();
            let Some(row) = self
                .db()
                .share_threads(&share.share_id)
                .await?
                .into_iter()
                .find(|r| r.local_thread_id.as_deref() == Some(local.as_str()))
            else {
                continue;
            };
            let role = self
                .db()
                .collab_setting_or_insert(&role_key(&share_id), "reader")
                .await?;
            let role: ShareRole = serde_json::from_value(serde_json::Value::String(role))
                .unwrap_or(ShareRole::Reader);
            if role == ShareRole::Reader {
                continue;
            }
            published += self
                .publish_thread_history(&share_id, &row.thread_gid.0, thread)
                .await?;
        }
        Ok(published)
    }

    async fn publish_thread_history(
        &self,
        share_id: &str,
        gid: &str,
        thread: &ThreadId,
    ) -> Result<usize> {
        let messages = self.db().list_messages_by_thread(thread).await?;
        let key = watermark_key(share_id, gid);
        let last_id = messages
            .last()
            .map(|m| m.id.to_string())
            .unwrap_or_default();
        let mark = self.db().collab_setting_or_insert(&key, &last_id).await?;
        let start = messages
            .iter()
            .position(|m| m.id.to_string() == mark)
            .map_or(0, |i| i + 1);
        let fresh: Vec<HistoryMessage> = messages[start..].iter().filter_map(shareable).collect();
        if start >= messages.len() {
            return Ok(0);
        }
        let device = self.e2ee()?.device_id().to_owned();
        let (_, client) = self.share_client()?;
        let now = Utc::now().timestamp();
        let holds_lease = client
            .leases(share_id)
            .await?
            .iter()
            .any(|l| l.thread_gid == gid && l.device_id == device && l.expires_at > now);
        if holds_lease {
            for chunk in fresh.chunks(MAX_HISTORY_MESSAGES) {
                let body = HistoryAppendedBody {
                    messages: chunk.to_vec(),
                };
                let plain = serde_json::to_vec(&body)
                    .map_err(|e| crate::error::DaemonError::Invalid(e.to_string()))?;
                self.emit_share_event(
                    &client,
                    share_id,
                    ShareEventKind::HistoryAppended,
                    Some(gid.to_owned()),
                    Payload::Plain(plain),
                )
                .await?;
            }
        }
        self.db().set_collab_setting(&key, &last_id).await?;
        Ok(if holds_lease { fresh.len() } else { 0 })
    }
}
