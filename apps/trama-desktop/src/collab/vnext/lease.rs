//! One-click "Take lease" for threads whose turns were not shared: the
//! thread's share comes from `GetThreadShare`, the lease from
//! `AcquireShareLease`.

use std::collections::{HashMap, HashSet};

use trama_domain::ids::ThreadId;
use trama_protocol::{
    ClientMessage, DEFAULT_LEASE_TTL_SECS, ShareLeaseSnapshot, ShareRole, ShareStatus,
    ThreadShareInfo,
};

/// What the transcript offers about the writer lease of one thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaseView {
    /// A "Take lease" button makes sense (writer, open share, not held by me).
    pub can_take: bool,
    pub taking: bool,
    pub holder: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct Leases {
    shares: HashMap<ThreadId, ThreadShareInfo>,
    taking: HashSet<ThreadId>,
    errors: HashMap<ThreadId, String>,
}

impl Leases {
    pub fn apply_thread_share(&mut self, thread: &ThreadId, share: Option<ThreadShareInfo>) {
        match share {
            Some(info) => {
                self.shares.insert(thread.clone(), info);
            }
            None => {
                self.shares.remove(thread);
                self.taking.remove(thread);
            }
        }
    }

    pub fn take(&mut self, thread: &ThreadId) -> Option<ClientMessage> {
        let info = self.shares.get(thread)?;
        if self.taking.contains(thread) {
            return None;
        }
        self.taking.insert(thread.clone());
        self.errors.remove(thread);
        Some(ClientMessage::AcquireShareLease {
            share_id: info.share_id.clone(),
            thread_id: thread.clone(),
            ttl_secs: DEFAULT_LEASE_TTL_SECS,
        })
    }

    /// A lease was granted; returns the thread to refresh.
    pub fn apply_lease(&mut self, lease: &ShareLeaseSnapshot) -> Option<ThreadId> {
        let thread = lease.thread_id.clone()?;
        self.taking.remove(&thread);
        if let Some(info) = self.shares.get_mut(&thread) {
            info.lease_holder = Some(lease.holder.clone());
        }
        Some(thread)
    }

    /// `AcquireShareLease` failed for `share_id`.
    pub fn fail(&mut self, share_id: Option<&str>, message: String) -> bool {
        let threads: Vec<ThreadId> = self
            .taking
            .iter()
            .filter(|t| {
                share_id.is_none_or(|id| self.shares.get(*t).is_some_and(|i| i.share_id == id))
            })
            .cloned()
            .collect();
        for t in &threads {
            self.taking.remove(t);
            self.errors.insert(t.clone(), message.clone());
        }
        !threads.is_empty()
    }

    pub fn view(&self, thread: &ThreadId, me: Option<&str>) -> LeaseView {
        let Some(info) = self.shares.get(thread) else {
            return LeaseView::default();
        };
        let mine = me.is_some_and(|me| {
            info.lease_holder
                .as_deref()
                .is_some_and(|h| h.eq_ignore_ascii_case(me))
        });
        LeaseView {
            can_take: info.role != ShareRole::Reader && info.status != ShareStatus::Closed && !mine,
            taking: self.taking.contains(thread),
            holder: info.lease_holder.clone(),
            error: self.errors.get(thread).cloned(),
        }
    }
}
