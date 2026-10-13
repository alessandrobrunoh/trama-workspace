//! vNext (collab protocol v3) wire types: server discovery and the durable
//! share event model (docs/plan/03 §5–§6).
//!
//! These types are additive: the v2 thread protocol in the crate root is
//! unchanged and still the one the service speaks. Signed manifests and
//! their canonical encoding live elsewhere; events here only reference
//! payloads as opaque ciphertext.

pub mod api;
pub mod cipher;
pub mod discovery;
pub mod events;

/// The vNext collab protocol version. v2 payloads are never reused for it.
pub const VNEXT_PROTOCOL_VERSION: u32 = 3;
pub mod replay;
#[cfg(test)]
mod tests;
