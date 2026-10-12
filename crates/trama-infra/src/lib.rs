//! Infrastructure adapters for TramaWorkspace.
//!
//! This crate owns the **native Claude Code** driver: it spawns the `claude`
//! CLI and speaks NDJSON (`--input-format stream-json` /
//! `--output-format stream-json`). ACP is not used for Claude Code.

pub mod changes;
pub mod checkout;
pub mod claude;

pub use claude::adapter::ClaudeAdapter;
