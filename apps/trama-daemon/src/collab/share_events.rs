//! vNext share event intake: shape and replay checks, then IPC fan-out as
//! `DaemonMessage::ShareEvent` to the subscribers of the local thread.
//!
//! The transport that receives events (M4) and the share DB that persists
//! device heads are outside this module: callers restore heads with
//! [`ShareEventRouter::restore_head`] and hand every received or locally
//! authored event to [`CollabService::deliver_share_event`].

// Wired to a transport in M4; until then only tests drive it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Mutex;

use trama_collab_protocol::vnext::events::ShareEventEnvelope;
use trama_collab_protocol::vnext::replay::{Admission, ReplayGuard};
use trama_domain::ids::ThreadId;
use trama_protocol::DaemonMessage;

use super::CollabService;

/// Most shares whose replay state is kept in memory.
const MAX_TRACKED_SHARES: usize = 1024;

#[derive(Default)]
pub(crate) struct ShareEventRouter {
    guards: Mutex<HashMap<String, ReplayGuard>>,
}

impl ShareEventRouter {
    fn with_guard<T>(&self, share_id: &str, f: impl FnOnce(&mut ReplayGuard) -> T) -> Option<T> {
        let mut guards = self.guards.lock().unwrap_or_else(|e| e.into_inner());
        if !guards.contains_key(share_id) && guards.len() >= MAX_TRACKED_SHARES {
            return None;
        }
        let guard = guards
            .entry(share_id.to_owned())
            .or_insert_with(|| ReplayGuard::new(share_id));
        Some(f(guard))
    }

    /// Seeds a device head loaded from persistent storage.
    pub(crate) fn restore_head(&self, share_id: &str, device: &str, seq: u64, event_id: &str) {
        self.with_guard(share_id, |guard| guard.restore_head(device, seq, event_id));
    }

    pub(crate) fn admit(&self, event: &ShareEventEnvelope) -> Admission {
        self.with_guard(&event.share_id, |guard| guard.admit(event))
            .unwrap_or_else(|| Admission::Reject("too many shares tracked".into()))
    }

    /// Drops a share's state (share closed or unlinked).
    pub(crate) fn forget(&self, share_id: &str) {
        let mut guards = self.guards.lock().unwrap_or_else(|e| e.into_inner());
        guards.remove(share_id);
    }
}

impl CollabService {
    /// Admits `event` and, when accepted, notifies the subscribers of
    /// `local_thread` (the local thread the share or `thread_gid` maps to).
    /// Duplicates are dropped silently; gaps and rejects are returned for
    /// the caller to backfill or log. Never forwards rejected events.
    pub(crate) fn deliver_share_event(
        &self,
        local_thread: &ThreadId,
        event: ShareEventEnvelope,
    ) -> Admission {
        let admission = self.share_events().admit(&event);
        if admission == Admission::Accept {
            self.sessions().notify(
                local_thread,
                DaemonMessage::ShareEvent {
                    thread_id: Some(local_thread.clone()),
                    event,
                },
            );
        }
        admission
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama_collab_protocol::vnext::events::ShareEventKind;

    fn event(share: &str, id: &str, seq: u64, prev: Option<&str>) -> ShareEventEnvelope {
        ShareEventEnvelope {
            event_id: id.into(),
            share_id: share.into(),
            kind: ShareEventKind::ShareCreated,
            thread_gid: None,
            author_device: "d1".into(),
            author_seq: seq,
            prev_author_event: prev.map(Into::into),
            parents: vec![],
            server_seq: None,
            key_epoch: 0,
            cipher_payload: "AA==".into(),
            signature: "AA==".into(),
            created_at: 0,
            received_at: None,
        }
    }

    #[test]
    fn router_keeps_per_share_heads_and_honours_restored_state() {
        let router = ShareEventRouter::default();
        router.restore_head("s1", "d1", 4, "e4");
        assert_eq!(
            router.admit(&event("s1", "e4", 4, Some("e3"))),
            Admission::Duplicate
        );
        assert_eq!(
            router.admit(&event("s1", "e5", 5, Some("e4"))),
            Admission::Accept
        );
        assert_eq!(router.admit(&event("s2", "x1", 1, None)), Admission::Accept);
        router.forget("s2");
        assert_eq!(router.admit(&event("s2", "x1", 1, None)), Admission::Accept);
    }
}
