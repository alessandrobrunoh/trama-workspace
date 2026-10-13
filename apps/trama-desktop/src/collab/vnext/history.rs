//! What the transcript shows about vNext sharing: messages never published
//! (written without the writer lease) and shares that ended.

use std::collections::{HashMap, HashSet};

use trama_domain::ids::{MessageId, ThreadId};
use trama_protocol::{ShareHistoryEntry, ShareHistoryState, ShareSnapshot, ShareStatus};

#[derive(Debug, Default)]
pub struct ShareHistory {
    unshared: HashMap<ThreadId, HashSet<MessageId>>,
    /// Root threads of shares that ended, with the reason to show.
    ended: HashMap<ThreadId, String>,
}

impl ShareHistory {
    /// Replaces the thread's state (the daemon sends the full list).
    pub fn apply(&mut self, thread: &ThreadId, entries: &[ShareHistoryEntry]) {
        let set: HashSet<MessageId> = entries
            .iter()
            .filter(|e| e.state == ShareHistoryState::NotSharedNoLease)
            .map(|e| e.message_id.clone())
            .collect();
        if set.is_empty() {
            self.unshared.remove(thread);
        } else {
            self.unshared.insert(thread.clone(), set);
        }
    }

    pub fn apply_shares(&mut self, shares: &[ShareSnapshot]) {
        for share in shares {
            let Some(root) = &share.root_thread_id else {
                continue;
            };
            if share.status == ShareStatus::Closed {
                self.ended.insert(
                    root.clone(),
                    "This share was closed; local history kept.".into(),
                );
            } else {
                self.ended.remove(root);
            }
        }
    }

    pub fn unshared(&self, thread: &ThreadId) -> HashSet<MessageId> {
        self.unshared.get(thread).cloned().unwrap_or_default()
    }

    pub fn ended(&self, thread: &ThreadId) -> Option<String> {
        self.ended.get(thread).cloned()
    }
}
