//! TramaWorkspace domain: the persistent core. Knows nothing about GPUI,
//! SQLite, Claude Code or SSH — those live behind the traits defined here.

pub mod activity;
pub mod agent;
pub mod checkout_state;
pub mod checkpoint;
pub mod comment;
pub mod error;
pub mod event;
pub mod ids;
pub mod manifest;
pub mod message;
pub mod revision;
pub mod runtime;
pub mod secrets;
pub mod share;
pub mod subagent;
pub mod subthread;
pub mod thread;
pub mod workspace;

pub use error::{DomainError, Result};
