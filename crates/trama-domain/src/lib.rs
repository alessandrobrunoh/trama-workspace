//! TramaWorkspace domain: the persistent core. Knows nothing about GPUI,
//! SQLite, Claude Code or SSH — those live behind the traits defined here.

pub mod agent;
pub mod checkpoint;
pub mod comment;
pub mod error;
pub mod event;
pub mod ids;
pub mod message;
pub mod revision;
pub mod runtime;
pub mod subthread;
pub mod thread;
pub mod workspace;

pub use error::{DomainError, Result};
