//! Publishing and listing sequenced share events.
//!
//! Admission per event: shape and replay rules ([`ReplayGuard`], seeded with
//! the stored device heads), the device bound to one user, owner-only kinds
//! from the owner, readers limited to `peer_acknowledged`, and
//! `history_appended` refused while another device holds the thread's
//! writer lease. Accepted events get the next `server_seq` and
//! `received_at`, then fan out to stream sockets.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use trama_collab_protocol::vnext::api::{
    MAX_SHARE_EVENTS_PAGE, MAX_SHARE_EVENTS_PER_PUBLISH, PublishShareEventsRequest,
    PublishShareEventsResponse, PublishedShareEvent, ShareEventsPage, ShareRole, ShareServerFrame,
};
use trama_collab_protocol::vnext::events::{ShareEventEnvelope, ShareEventKind};
use trama_collab_protocol::vnext::replay::{Admission, ReplayGuard};

use super::{Grant, ShareError, grant, read, validate_id, write};
use crate::{AppState, collab};

#[derive(Deserialize)]
pub(crate) struct PageQuery {
    #[serde(default)]
    pub after: u64,
    #[serde(default)]
    pub limit: Option<u32>,
}

fn check_author(
    c: &Connection,
    g: Grant,
    share_id: &str,
    user_id: i64,
    event: &ShareEventEnvelope,
    now: i64,
) -> Result<(), ShareError> {
    if event.kind.owner_only() && g.role != ShareRole::Owner {
        return Err(ShareError::Forbidden(
            "this event kind is reserved to the owner",
        ));
    }
    if !g.can_write() && event.kind != ShareEventKind::PeerAcknowledged {
        return Err(ShareError::Forbidden("read-only member"));
    }
    let bound: Option<i64> = c
        .query_row(
            "SELECT user_id FROM v3_share_devices WHERE share_id = ?1 AND device_id = ?2",
            params![share_id, event.author_device],
            |r| r.get(0),
        )
        .optional()?;
    match bound {
        Some(owner) if owner != user_id => {
            return Err(ShareError::Forbidden("device belongs to another member"));
        }
        Some(_) => {}
        None => {
            c.execute(
                "INSERT INTO v3_share_devices (share_id, device_id, user_id) VALUES (?1, ?2, ?3)",
                params![share_id, event.author_device, user_id],
            )?;
        }
    }
    if event.kind == ShareEventKind::HistoryAppended
        && let Some(gid) = &event.thread_gid
    {
        let holder: Option<String> = c
            .query_row(
                "SELECT device_id FROM v3_share_leases
                 WHERE share_id = ?1 AND thread_gid = ?2 AND expires_at > ?3",
                params![share_id, gid, now],
                |r| r.get(0),
            )
            .optional()?;
        if holder.is_some_and(|d| d != event.author_device) {
            return Err(ShareError::Conflict(
                "another device holds the writer lease",
            ));
        }
    }
    Ok(())
}

fn seed_head(
    c: &Connection,
    guard: &mut ReplayGuard,
    share_id: &str,
    device: &str,
) -> Result<(), ShareError> {
    let head: Option<(i64, String)> = c
        .query_row(
            "SELECT author_seq, event_id FROM v3_share_events
             WHERE share_id = ?1 AND author_device = ?2 ORDER BY author_seq DESC LIMIT 1",
            params![share_id, device],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((seq, id)) = head {
        guard.restore_head(device, seq as u64, &id);
    }
    Ok(())
}

pub(crate) fn append(
    c: &Connection,
    share_id: &str,
    user_id: i64,
    events: Vec<ShareEventEnvelope>,
    now: i64,
) -> Result<(Vec<PublishedShareEvent>, Vec<ShareEventEnvelope>), ShareError> {
    let g = grant(c, share_id, user_id)?;
    let mut guard = ReplayGuard::new(share_id);
    let mut seeded = std::collections::HashSet::new();
    let mut last_seq: i64 = c.query_row(
        "SELECT last_seq FROM v3_shares WHERE share_id = ?1",
        params![share_id],
        |r| r.get(0),
    )?;
    let (mut results, mut stored) = (Vec::new(), Vec::new());
    for mut event in events {
        let existing: Option<i64> = c
            .query_row(
                "SELECT server_seq FROM v3_share_events WHERE share_id = ?1 AND event_id = ?2",
                params![share_id, event.event_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(seq) = existing {
            results.push(PublishedShareEvent {
                event_id: event.event_id,
                server_seq: seq as u64,
                duplicate: true,
            });
            continue;
        }
        if seeded.insert(event.author_device.clone()) {
            seed_head(c, &mut guard, share_id, &event.author_device)?;
        }
        match guard.admit(&event) {
            Admission::Accept => {}
            Admission::Duplicate => return Err(ShareError::Rejected("duplicate event".into())),
            Admission::Gap { expected_seq } => {
                return Err(ShareError::Rejected(format!(
                    "author_seq gap: expected {expected_seq}"
                )));
            }
            Admission::Reject(why) => return Err(ShareError::Rejected(why)),
        }
        check_author(c, g, share_id, user_id, &event, now)?;
        last_seq += 1;
        event.server_seq = Some(last_seq as u64);
        event.received_at = Some(now.saturating_mul(1000));
        let envelope = serde_json::to_string(&event).map_err(|_| ShareError::Database)?;
        c.execute(
            "INSERT INTO v3_share_events
             (share_id, server_seq, event_id, author_device, author_seq, envelope)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                share_id,
                last_seq,
                event.event_id,
                event.author_device,
                event.author_seq as i64,
                envelope
            ],
        )?;
        results.push(PublishedShareEvent {
            event_id: event.event_id.clone(),
            server_seq: last_seq as u64,
            duplicate: false,
        });
        stored.push(event);
    }
    c.execute(
        "UPDATE v3_shares SET last_seq = ?2 WHERE share_id = ?1",
        params![share_id, last_seq],
    )?;
    Ok((results, stored))
}

pub(crate) async fn publish(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<PublishShareEventsRequest>,
) -> Result<Json<PublishShareEventsResponse>, ShareError> {
    let identity = collab::mutating_session(&state, &headers).await?;
    validate_id(&share_id)?;
    if input.events.is_empty() || input.events.len() > MAX_SHARE_EVENTS_PER_PUBLISH {
        return Err(ShareError::Invalid("1 to 64 events per publish"));
    }
    let now = state.auth.now();
    let id = share_id.clone();
    let (events, stored) = write(&state, move |c| {
        append(c, &id, identity.user_id, input.events, now)
    })
    .await?;
    state.shares.send(
        &share_id,
        stored
            .into_iter()
            .map(|event| ShareServerFrame::Event { event })
            .collect(),
    );
    Ok(Json(PublishShareEventsResponse { events }))
}

pub(crate) fn page(
    c: &Connection,
    share_id: &str,
    user_id: i64,
    after: u64,
    limit: u32,
) -> Result<ShareEventsPage, ShareError> {
    grant(c, share_id, user_id)?;
    let last: i64 = c.query_row(
        "SELECT last_seq FROM v3_shares WHERE share_id = ?1",
        params![share_id],
        |r| r.get(0),
    )?;
    let mut statement = c.prepare(
        "SELECT envelope FROM v3_share_events WHERE share_id = ?1 AND server_seq > ?2
         ORDER BY server_seq LIMIT ?3",
    )?;
    let events = statement
        .query_map(params![share_id, after as i64, limit as i64], |r| {
            r.get::<_, String>(0)
        })?
        .map(|row| {
            row.map_err(ShareError::from)
                .and_then(|text| serde_json::from_str(&text).map_err(|_| ShareError::Database))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ShareEventsPage {
        events,
        last_server_seq: last as u64,
    })
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<ShareEventsPage>, ShareError> {
    let identity = collab::session(&state, &headers).await?;
    validate_id(&share_id)?;
    let limit = query
        .limit
        .unwrap_or(MAX_SHARE_EVENTS_PAGE)
        .clamp(1, MAX_SHARE_EVENTS_PAGE);
    Ok(Json(
        read(&state, move |c| {
            page(c, &share_id, identity.user_id, query.after, limit)
        })
        .await?,
    ))
}
