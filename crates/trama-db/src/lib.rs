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

// Crate-root aliases keep `crate::<module>` paths stable.
use repo::{agents, archive, checkouts, collab, collab_history, comments, composer, subthreads};

pub use agents::SubagentRecord;
pub use archive::ThreadCheckoutRow;
pub use checkouts::CheckoutLocation;
pub use collab::{CollabLink, CollabTurn};
pub use collab_history::{
    CollabApplied, CollabApply, FamilyRow, OutboxJob, OutboxRow, RemoteChangesRow,
};
pub use comments::CommentRecord;
pub use composer::AgentCommands;
pub use store::TramaDb;
pub use subthreads::{ReviewVerdictRecord, SubthreadFork};

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
