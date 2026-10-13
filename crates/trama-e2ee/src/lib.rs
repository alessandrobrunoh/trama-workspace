//! End-to-end encryption for Trama shares.
//!
//! Wraps [`openmls`] (MLS, RFC 9420) so the rest of the workspace never touches
//! MLS types directly. A share maps to one MLS group; every device is one leaf.
//! Share event bodies (`cipher_payload`) are MLS application messages whose
//! authenticated data binds them to the event's routing metadata.
//!
//! No transport lives here: callers move key packages, Welcome, commit and
//! application messages as opaque bytes.
//!
//! **Not audited.** The plan requires a security review before Cloud E2EE is
//! advertised; see `docs/plan/06-sicurezza-privacy-acl-e2ee.md`.

mod error;
mod group;
mod identity;
mod provider;
mod store;

#[cfg(test)]
mod tests;

pub use error::{E2eeError, Result};
pub use group::{Inbound, MembershipChange, PendingJoin, ShareGroup};
pub use identity::DeviceIdentity;
pub use store::{EpochStateStore, MemoryEpochStore};

/// The only MLS ciphersuite Trama accepts (X25519, AES-128-GCM, SHA-256,
/// Ed25519). Fixed, not negotiable: no downgrade.
pub const CIPHERSUITE: openmls::prelude::Ciphersuite =
    openmls::prelude::Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;
