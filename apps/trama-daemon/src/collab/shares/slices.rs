//! Pre-share history in the snapshot (docs/plan/03 §3): each mapped
//! thread's local text messages up to its share watermark (the history
//! that existed when the share was created or the thread was mapped) are
//! uploaded once as an encrypted `history` object; the manifest's
//! `message_slice` is `v1:<content_id>:<parts>:<ciphertext_hash>:<key>`.
//! Later history travels as `history_appended` events. A joiner imports
//! the slice as `collab:<publisher device>` messages.

use trama_collab_protocol::vnext::api::ObjectClass;
use trama_collab_protocol::vnext::bodies::{HistoryMessage, HistoryRole};
use trama_domain::ids::ThreadId;
use trama_domain::message::Role;

use super::super::CollabService;
use super::super::transport::ShareClient;
use super::inbound::message;
use super::objects::{download, open_object, seal_object, upload};
use super::outgoing::{shareable, watermark_key};
use crate::error::{DaemonError, Result};

const MAX_SLICE_MESSAGES: usize = 10_000;

fn slice_key(share_id: &str, gid: &str) -> String {
    format!("share_slice:{share_id}:{gid}")
}

impl CollabService {
    /// The slice reference of a thread, uploading it on first use.
    pub(crate) async fn thread_slice(
        &self,
        client: &ShareClient,
        share_id: &str,
        gid: &str,
        local: &ThreadId,
    ) -> Result<Option<String>> {
        let key = slice_key(share_id, gid);
        let cached = self.db().collab_setting_or_insert(&key, "?").await?;
        if cached != "?" {
            return Ok(Some(cached).filter(|c| !c.is_empty()));
        }
        let messages = self.db().list_messages_by_thread(local).await?;
        let mark = self
            .db()
            .collab_setting_or_insert(&watermark_key(share_id, gid), "")
            .await?;
        let end = messages
            .iter()
            .position(|m| m.id.to_string() == mark)
            .map_or(0, |i| i + 1);
        let slice: Vec<HistoryMessage> = messages[..end]
            .iter()
            .filter_map(shareable)
            .take(MAX_SLICE_MESSAGES)
            .collect();
        let reference = if slice.is_empty() {
            String::new()
        } else {
            let plain =
                serde_json::to_vec(&slice).map_err(|e| DaemonError::Invalid(e.to_string()))?;
            let object = tokio::task::spawn_blocking(move || seal_object(&plain))
                .await
                .map_err(|e| DaemonError::Collab(e.to_string()))??;
            upload(client, share_id, ObjectClass::History, &object).await?;
            format!(
                "v1:{}:{}:{}:{}",
                object.content_id,
                object.parts.len(),
                object.ciphertext_hash,
                object.key
            )
        };
        self.db().set_collab_setting(&key, &reference).await?;
        Ok(Some(reference).filter(|r| !r.is_empty()))
    }

    /// Joiner: appends a thread's pre-share history. Returns the count.
    pub(crate) async fn import_slice(
        &self,
        client: &ShareClient,
        share_id: &str,
        reference: &str,
        local: &ThreadId,
        publisher: &str,
    ) -> Result<usize> {
        let fields: Vec<&str> = reference.split(':').collect();
        let ["v1", content_id, parts, hash, key] = fields.as_slice() else {
            return Err(DaemonError::Collab(
                "unknown message slice reference".into(),
            ));
        };
        let parts: u32 = parts
            .parse()
            .map_err(|_| DaemonError::Collab("bad slice part count".into()))?;
        let blobs = download(client, share_id, content_id, parts).await?;
        let (id, key, hash) = (content_id.to_string(), key.to_string(), hash.to_string());
        let plain = tokio::task::spawn_blocking(move || open_object(&id, &key, &blobs, &hash))
            .await
            .map_err(|e| DaemonError::Collab(e.to_string()))??;
        let slice: Vec<HistoryMessage> = serde_json::from_slice(&plain)
            .map_err(|_| DaemonError::Collab("message slice is malformed".into()))?;
        let count = slice.len();
        for m in slice.into_iter().take(MAX_SLICE_MESSAGES) {
            let at = chrono::DateTime::parse_from_rfc3339(&m.created_at)
                .map(|t| t.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());
            let role = match m.role {
                HistoryRole::User => Role::User,
                HistoryRole::Assistant => Role::Assistant,
            };
            let msg = message(local, publisher, role, m.text, at);
            self.db().append_message(&msg).await?;
        }
        Ok(count)
    }
}
