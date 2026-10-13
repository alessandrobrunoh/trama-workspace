//! Versioned JSON-lines IPC between the desktop UI, the local daemon, and (later) runners.
//!
//! # Framing
//!
//! Each message is a single JSON object encoded as UTF-8 and terminated by a newline
//! (`\n`). There is no length prefix and no embedded raw newlines inside a value
//! (`serde_json::to_string` emits compact JSON). A transport is any reliable byte
//! stream (Unix domain socket today; TCP/SSH for remote runners later).
//!
//! Decode one message per line; ignore empty lines. A line that is not valid JSON
//! or does not match a known schema is a protocol error.
//!
//! # Tagging
//!
//! [`ClientMessage`] and [`DaemonMessage`] are **adjacently tagged** serde enums:
//! `{"type":"<snake_case_variant>","payload": ... }`. Unit variants omit `payload`.
//!
//! # Unknown-tag policy
//!
//! Unknown `type` tags are **denied**. Serde rejects them at deserialize time; the
//! daemon must not silently ignore a variant it does not understand. Because of that,
//! a new variant is a breaking handshake change: an older peer would accept `Hello`
//! and then fail the command. [`PROTOCOL_VERSION`] **2** added workspace creation,
//! repository membership, and legacy-thread migration. **3** added thread
//! checkout discovery. **4** added the approval channel
//! ([`ClientMessage::ApprovalHook`], [`DaemonMessage::ApprovalHookDecision`])
//! and the `session_info` / `approval_resolved` agent events. **5** added
//! subthreads, changes, pull changes, review verdicts and land results. **6**
//! added anchored comments (create, list, reply, resolve, reopen, delete, send)
//! and delivers pending comments with `SendPrompt`. **7** added the composer
//! backend: `@` mentionables, `/` skills, per-thread model selection, inline
//! terminals, and `SendPrompt.mentions` / `SendPrompt.terminals`. **8** added
//! shared threads through a `trama-collab` service (connect, share, join, list,
//! drafts, presence, mirrored remote activity). **9** adds Trama subagents (the
//! MCP helper's [`ClientMessage::McpToolCall`] /
//! [`DaemonMessage::McpToolResult`], [`DaemonMessage::SubagentStatusChanged`],
//! `depth` and `subagent` in thread snapshots, message annotations), nesting
//! at any depth, and [`ClientMessage::ForkThread`] at a message. **10** adds
//! [`DaemonMessage::TurnActivity`]: a turn's stored tool calls and thinking,
//! replayed on subscribe before the message that ends the turn. **11** adds
//! checkout disk accounting and archiving ([`ClientMessage::GetDiskUsage`],
//! [`ClientMessage::ArchiveThread`], [`ClientMessage::UnarchiveThread`],
//! [`ClientMessage::DeleteThreadCheckouts`], their replies,
//! `ThreadSnapshot.archived` and [`CheckoutStatus::Removed`]). **12** shares a
//! thread's whole family and history. **13** adds collab server discovery
//! ([`ClientMessage::DiscoverCollabServer`], [`DaemonMessage::CollabServerInfo`])
//! and vNext share events ([`DaemonMessage::ShareEvent`]). **14** adds the
//! Share/Join flow (see the `shares` payloads); **15** adds
//! `RemoveShareMember`; **16** adds share members, per-message share state
//! and `ShareFailed`. Each bump is
//! transport-only. Persisted domain ids and SQLite rows are not rewritten.
//!
//! A version mismatch fails the handshake (`Welcome.accepted = false`). Extra object
//! fields inside a known payload are ignored (serde default). Bump [`PROTOCOL_VERSION`]
//! again for further tag or payload shape changes.

mod client;
mod daemon;
mod payloads;
mod shares;
mod snapshots;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_vnext;
#[allow(unused_imports)]
pub use self::client::*;
#[allow(unused_imports)]
pub use self::daemon::*;
#[allow(unused_imports)]
pub use self::payloads::*;
#[allow(unused_imports)]
pub use self::shares::*;
#[allow(unused_imports)]
pub use self::snapshots::*;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::PathBuf;

pub use trama_domain::activity::ActivityItem;
pub use trama_domain::agent::{AgentEvent, ApprovalDecision, BillingSource, UsageReport};
pub use trama_domain::comment::{AnchorStatus, CodeAnchor, CommentDelivery, CommentTarget};
pub use trama_domain::ids::{CommentId, MessageId, RepositoryId, ThreadId, WorkspaceId};
pub use trama_domain::message::{Actor, Message, MessageAnnotation};
pub use trama_domain::subagent::{SpawnedBy, SubagentProfile, SubagentStatus};
pub use trama_domain::subthread::{
    FileChange, FileChangeStatus, LandOutcome, PullFileOutcome, PullFileResult, RepoPullResult,
    ReviewVerdict,
};
pub use trama_domain::thread::ThreadKind;

/// vNext discovery and share event types (collab protocol v3).
pub use trama_collab_protocol::vnext::discovery::{
    Capability as CollabCapability, NegotiatedProfile, ServerDiscovery,
};
pub use trama_collab_protocol::vnext::events::{ShareEventEnvelope, ShareEventKind};
/// Shared-thread wire types reused from the collab service.
pub use trama_collab_protocol::{
    AccessLevel as CollabAccess, EventEnvelope as CollabEventEnvelope,
    EventKind as CollabEventKind, MemberInfo as CollabMember, MemberVia as CollabMemberVia,
    PresenceState as CollabPresenceState,
};

/// Wire protocol version carried in `Hello` / `Welcome`.
///
/// Version 17 adds `GetThreadShare` / `ThreadShare` (share id, role and
/// lease per thread) and the `access_revoked` share error code.
/// Version 16 adds `ListShareMembers`, `GetShareHistoryStatus`,
/// `ShareHistoryStatus` and `ShareFailed` (share error correlation ids).
/// Version 15 adds `RemoveShareMember` / `ShareMemberRemoved`.
/// Version 14 adds the Share/Join flow (`SharePreflight`, `CreateShare`,
/// `ListShares`, join requests and approval, `CompleteShareJoin`,
/// `PublishShareSnapshot`, writer leases and their replies).
/// Version 13 adds collab server discovery (`DiscoverCollabServer` /
/// `CollabServerInfo`) and vNext share events (`ShareEvent`).
/// Version 12 (11 is checkout provisioning) shares a thread's whole family
/// and history (ADR 0013): `GetRemoteChanges` / `RemoteChanges` (the changes
/// snapshots other participants published), and `SharedThreadSnapshot`
/// carries the outbox state. Subthreads and subagents of a shared thread are
/// shared with it and mirrored as local children.
/// Version 11 adds checkout disk usage (`GetDiskUsage` / `DiskUsage`: apparent
/// vs. private bytes of every copy-on-write checkout), archiving
/// (`ArchiveThread`, `UnarchiveThread`, `ThreadArchived`, `ThreadUnarchived`,
/// `ThreadSnapshot.archived`) and removing a thread's checkouts to free disk
/// (`DeleteThreadCheckouts`, `ThreadCheckoutsDeleted`,
/// `CheckoutRemovalRefused`, `CheckoutStatus::Removed`); removed checkouts are
/// recreated on demand (ADR 0012).
/// Version 10 adds `TurnActivity`: the bounded tool calls, thinking and
/// subagent lifecycle of each stored turn (and of the turn in flight), sent
/// on subscribe so tool views survive restarts.
/// Version 9 added subagents spawned through Trama's MCP tools
/// (`McpToolCall`, `McpToolResult`, `SubagentStatusChanged`, `depth` and
/// `subagent` in `ThreadSnapshot`, `Message::annotation`), subthreads of
/// subthreads at any depth, and `ForkThread` (fork at a message: the
/// conversation up to it and the files of that turn's checkpoint).
/// Version 8 added shared threads: `ConnectCollab`, `DisconnectCollab`,
/// `GetCollabStatus`, `ShareThread`, `GetSharedThread`,
/// `RemoveSharedThreadMember`, `JoinSharedThread`, `ListSharedThreads`,
/// `UnlinkSharedThread`, `SetCollabDraft`, `SetCollabPresence`, and the
/// matching `Collab*` / `SharedThread*` daemon messages. `SendPrompt` on a
/// linked thread submits the turn to the collab service; this daemon's runner
/// then runs it locally (ADR 0008).
/// Version 7 added the composer backend: `ListMentionables`, `ListSkills`,
/// `SetThreadModel`, `ListModels`, inline terminals (`OpenTerminal`,
/// `TerminalInput`, `ResizeTerminal`, `CloseTerminal`, `ListTerminals`,
/// `ReplayTerminal`), `ThreadSnapshot.model`, and `SendPrompt` mentions and
/// terminal attachments.
/// Version 6 added anchored comments: `CreateComment`, `ListComments`,
/// `ResolveComment`, `ReopenComment`, `DeleteComment`, `SendComments`, and
/// `SendPrompt` now delivers the thread's pending comments with the prompt.
/// Version 5 added subthreads (create/list, parent and kind in thread
/// snapshots), changes, pull changes, review verdicts and land results.
/// Version 4 added the Claude approval channel and 3 thread checkout
/// discovery. It does not migrate stored data; peers with a different version
/// are rejected at handshake time.
pub const PROTOCOL_VERSION: u32 = 17;

/// Default writer lease length for `AcquireShareLease`.
pub const DEFAULT_LEASE_TTL_SECS: u64 = 120;

fn default_lease_ttl() -> u64 {
    DEFAULT_LEASE_TTL_SECS
}

/// Environment variable that relocates the whole app-data directory (socket,
/// database, config, legacy checkouts). Use it for isolated test instances
/// instead of overriding `HOME`: the `claude` child still needs the real
/// `HOME` to find the user's Claude Code login.
pub const DATA_DIR_ENV: &str = "TRAMA_DATA_DIR";

/// `$TRAMA_DATA_DIR`, else the per-OS data directory (see `platform_app_dir`).
pub fn default_app_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    platform_app_dir(|key| std::env::var_os(key).filter(|value| !value.is_empty()))
}

/// Per-OS default data directory from an environment lookup:
/// macOS `~/Library/Application Support/TramaWorkspace` (unchanged),
/// Windows `%APPDATA%/TramaWorkspace`, other Unix
/// `$XDG_DATA_HOME/TramaWorkspace` or `~/.local/share/TramaWorkspace`.
fn platform_app_dir(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = env("HOME")?;
        Some(PathBuf::from(home).join("Library/Application Support/TramaWorkspace"))
    } else if cfg!(windows) {
        let base = env("APPDATA").map(PathBuf::from).or_else(|| {
            env("USERPROFILE").map(|home| PathBuf::from(home).join("AppData/Roaming"))
        })?;
        Some(base.join("TramaWorkspace"))
    } else {
        let base = env("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| env("HOME").map(|home| PathBuf::from(home).join(".local/share")))?;
        Some(base.join("TramaWorkspace"))
    }
}

/// Encode `value` as one JSON line (compact JSON plus a trailing `\n`).
pub fn encode_json_line<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    Ok(buf)
}

/// Decode a single JSON line. A trailing `\n` / `\r\n` is tolerated.
pub fn decode_json_line<T: DeserializeOwned>(line: &str) -> serde_json::Result<T> {
    serde_json::from_str(line.trim_end_matches(['\r', '\n']))
}
