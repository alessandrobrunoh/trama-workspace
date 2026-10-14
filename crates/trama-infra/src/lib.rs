//! Infrastructure adapters for TramaWorkspace.
//!
//! This crate owns the **native Claude Code** driver: it spawns the `claude`
//! CLI and speaks NDJSON (`--input-format stream-json` /
//! `--output-format stream-json`). ACP is not used for Claude Code.

pub mod claude;
pub mod fs;
pub mod git;
pub mod llm;
pub mod process;

pub use claude::adapter::ClaudeAdapter;
