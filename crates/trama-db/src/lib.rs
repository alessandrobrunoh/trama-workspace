//! TramaDB — append-only event store plus projections on SQLite (WAL, FTS5).
//!
//! # Blocking bridge
//!
//! `rusqlite::Connection` is synchronous. [`TramaDb`] holds it behind a
//! `Mutex` and runs SQL (and disk blob I/O) on `tokio::task::spawn_blocking`.
//! Open/migrate stay synchronous so a runtime is not required to construct
//! the store. Concurrent async calls serialize on the mutex.

mod codec;
mod event;
mod migrations;
mod projections;
mod repo;
mod store;

pub use repo::agents::SubagentRecord;
pub use repo::archive::ThreadCheckoutRow;
pub use repo::checkouts::CheckoutLocation;
pub use repo::collab::{CollabLink, CollabTurn};
pub use repo::collab_history::{
    CollabApplied, CollabApply, FamilyRow, OutboxJob, OutboxRow, RemoteChangesRow,
};
pub use repo::comments::CommentRecord;
pub use repo::composer::AgentCommands;
pub use repo::share::{
    CollaborationServerRow, ObjectState, ShareCheckoutStateRow, ShareDeviceHead, ShareObjectRow,
    SharePeerAck, ShareRow, ShareStatus, ShareThreadRow,
};
pub use repo::subthreads::{ReviewVerdictRecord, SubthreadFork};
pub use store::TramaDb;

/// Absolute test path on every platform: `/a/b` stays as is on Unix and
/// becomes `C:/a/b` on Windows, where a leading `/` is not absolute.
#[cfg(test)]
pub(crate) fn test_abs_path(path: &str) -> std::path::PathBuf {
    if cfg!(windows) {
        std::path::PathBuf::from(format!("C:{path}"))
    } else {
        std::path::PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests;
