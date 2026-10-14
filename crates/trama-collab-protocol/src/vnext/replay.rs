//! Replay and duplicate protection for incoming vNext events: idempotent on
//! `event_id`, strict per-device `author_seq` chain.

use std::collections::{HashMap, HashSet};

use super::events::{EventShapeError, ShareEventEnvelope};

/// What to do with an incoming event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// New and in order: apply it.
    Accept,
    /// Already applied (same `event_id`): drop silently, retries are safe.
    Duplicate,
    /// The author's chain has a gap; fetch the missing events first.
    Gap { expected_seq: u64 },
    /// Malformed, or a replay with a reused `author_seq` / forked chain.
    Reject(String),
}

#[derive(Debug, Clone)]
struct DeviceHead {
    seq: u64,
    event_id: String,
}

/// Per-share admission state. Persisting it is the caller's job (the share
/// DB); this type only holds the rules.
#[derive(Debug, Default, Clone)]
pub struct ReplayGuard {
    share_id: Option<String>,
    seen: HashSet<String>,
    heads: HashMap<String, DeviceHead>,
}

impl ReplayGuard {
    pub fn new(share_id: impl Into<String>) -> Self {
        Self {
            share_id: Some(share_id.into()),
            ..Self::default()
        }
    }

    /// Restores a device head loaded from storage.
    pub fn restore_head(&mut self, device: &str, seq: u64, event_id: &str) {
        self.heads.insert(
            device.into(),
            DeviceHead {
                seq,
                event_id: event_id.into(),
            },
        );
        self.seen.insert(event_id.into());
    }

    /// Checks an event without recording it.
    pub fn check(&self, event: &ShareEventEnvelope) -> Admission {
        if let Err(err) = event.validate_shape() {
            return Admission::Reject(shape(err));
        }
        if self
            .share_id
            .as_deref()
            .is_some_and(|id| id != event.share_id)
        {
            return Admission::Reject("event belongs to another share".into());
        }
        if self.seen.contains(&event.event_id) {
            return Admission::Duplicate;
        }
        let (head_seq, head_id) = match self.heads.get(&event.author_device) {
            Some(head) => (head.seq, Some(head.event_id.as_str())),
            None => (0, None),
        };
        if event.author_seq <= head_seq {
            return Admission::Reject("author_seq reused (replay or forked device chain)".into());
        }
        if event.author_seq > head_seq + 1 {
            return Admission::Gap {
                expected_seq: head_seq + 1,
            };
        }
        if event.prev_author_event.as_deref() != head_id {
            return Admission::Reject("prev_author_event does not match the device head".into());
        }
        Admission::Accept
    }

    /// Checks and, on `Accept`, records the event.
    pub fn admit(&mut self, event: &ShareEventEnvelope) -> Admission {
        let admission = self.check(event);
        if admission == Admission::Accept {
            self.restore_head(&event.author_device, event.author_seq, &event.event_id);
        }
        admission
    }
}

fn shape(err: EventShapeError) -> String {
    err.to_string()
}
