//! Local effects of received share events (docs/plan/03 §4), applied after
//! the event was admitted and recorded, so each runs at most once:
//!
//! - `membership_changed`: the MLS commit is merged into the share group
//!   (skipped when the group is already at or past its epoch, e.g. the
//!   commit that added this device).
//! - `history_appended`: messages are appended to the mapped local thread
//!   as `collab:<author device>`.
//! - `review_verdict_posted`: a system notice on the mapped thread.
//! - `thread_added`: an unmapped perimeter row, mapped on the next join or
//!   snapshot.
//!
//! Every other kind has no local history. Own events and payloads this
//! device cannot open (sealed before it joined) are skipped.

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use trama_collab_protocol::vnext::bodies::{
    HistoryAppendedBody, HistoryRole, MAX_HISTORY_MESSAGES, ReviewVerdictBody, ThreadAddedBody,
};
use trama_collab_protocol::vnext::cipher::{PayloadCipher, PayloadKind};
use trama_collab_protocol::vnext::events::{ShareEventEnvelope, ShareEventKind};
use trama_db::{ShareStatus, ShareThreadRow};
use trama_domain::ids::{MessageId, ThreadId};
use trama_domain::message::{Actor, ContentBlock, Message, Role};
use trama_domain::share::{ShareId, ThreadGid};
use trama_e2ee::Inbound;
use trama_protocol::DaemonMessage;

use super::super::{CollabService, REMOTE_ACTOR_PREFIX};
use crate::error::Result;

const MAX_TEXT_BYTES: usize = 256 * 1024;

pub(super) fn message(
    thread: &ThreadId,
    author: &str,
    role: Role,
    text: String,
    at: DateTime<Utc>,
) -> Message {
    Message {
        id: MessageId::generate(),
        thread_id: thread.clone(),
        parent_id: None,
        actor: Actor::Human {
            id: format!("{REMOTE_ACTOR_PREFIX}{author}"),
        },
        role,
        content: vec![ContentBlock::Text {
            text: super::super::truncate(&text, MAX_TEXT_BYTES),
        }],
        created_at: at,
        claude_uuid: None,
        annotation: None,
    }
}

impl CollabService {
    pub(crate) async fn apply_share_payload(&self, event: &ShareEventEnvelope) -> Result<()> {
        let Ok(e2ee) = self.e2ee() else {
            return Ok(());
        };
        if event.author_device == e2ee.device_id() {
            return Ok(());
        }
        let wanted = matches!(
            event.kind,
            ShareEventKind::MembershipChanged
                | ShareEventKind::HistoryAppended
                | ShareEventKind::ReviewVerdictPosted
                | ShareEventKind::ThreadAdded
        );
        if !wanted {
            return Ok(());
        }
        let Ok(bytes) = STANDARD.decode(&event.cipher_payload) else {
            return Ok(());
        };
        let share_id = event.share_id.clone();
        let (kind, epoch) = (event.kind, event.key_epoch);
        let opened =
            tokio::task::spawn_blocking(move || -> Option<std::result::Result<Vec<u8>, bool>> {
                if kind == ShareEventKind::MembershipChanged {
                    let still = e2ee.cipher.with_group(&share_id, |g, _| {
                        if g.epoch() >= epoch {
                            return Ok(true);
                        }
                        match g.process(&bytes, &[])? {
                            Inbound::Commit { still_member, .. } => Ok(still_member),
                            Inbound::Application { .. } => Ok(true),
                        }
                    });
                    return Some(Err(still.unwrap_or(true)));
                }
                e2ee.cipher
                    .open(&share_id, epoch, PayloadKind::Event, &bytes)
                    .ok()
                    .map(Ok)
            })
            .await
            .ok()
            .flatten();
        let share = ShareId::from(event.share_id.as_str());
        let plain = match opened {
            None => return Ok(()), // sealed before this device joined
            Some(Err(still_member)) => {
                if !still_member {
                    self.db()
                        .set_share_status(&share, ShareStatus::Closed)
                        .await?;
                }
                return Ok(());
            }
            Some(Ok(plain)) => plain,
        };
        let author = event.author_device.as_str();
        match event.kind {
            ShareEventKind::ThreadAdded => {
                let (Ok(body), Some(gid)) = (
                    serde_json::from_slice::<ThreadAddedBody>(&plain),
                    event.thread_gid.clone(),
                ) else {
                    return Ok(());
                };
                let known = self.db().share_threads(&share).await?;
                if known.iter().any(|r| r.thread_gid.0 == gid) {
                    return Ok(());
                }
                self.db()
                    .upsert_share_threads(vec![ShareThreadRow {
                        share_id: share,
                        thread_gid: ThreadGid(gid),
                        local_thread_id: None,
                        parent_gid: body.parent_gid.map(ThreadGid),
                        source_scope_root: false,
                        kind: body.kind,
                    }])
                    .await?;
            }
            ShareEventKind::HistoryAppended | ShareEventKind::ReviewVerdictPosted => {
                let Some(thread) = self
                    .local_thread(&share, event.thread_gid.as_deref())
                    .await?
                else {
                    return Ok(());
                };
                let messages = if event.kind == ShareEventKind::HistoryAppended {
                    let Ok(body) = serde_json::from_slice::<HistoryAppendedBody>(&plain) else {
                        return Ok(());
                    };
                    body.messages
                        .into_iter()
                        .take(MAX_HISTORY_MESSAGES)
                        .map(|m| {
                            let role = match m.role {
                                HistoryRole::User => Role::User,
                                HistoryRole::Assistant => Role::Assistant,
                            };
                            let at = DateTime::parse_from_rfc3339(&m.created_at)
                                .map(|t| t.with_timezone(&Utc))
                                .unwrap_or_else(|_| Utc::now());
                            message(&thread, author, role, m.text, at)
                        })
                        .collect::<Vec<_>>()
                } else {
                    let Ok(body) = serde_json::from_slice::<ReviewVerdictBody>(&plain) else {
                        return Ok(());
                    };
                    let mut text = format!("Review verdict: {}", body.verdict);
                    if !body.body.is_empty() {
                        text.push_str("\n\n");
                        text.push_str(&body.body);
                    }
                    vec![message(&thread, author, Role::System, text, Utc::now())]
                };
                for message in messages {
                    self.db().append_message(&message).await?;
                    self.sessions().notify(
                        &thread,
                        DaemonMessage::MessageAppended {
                            thread_id: thread.clone(),
                            message,
                        },
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }
}
