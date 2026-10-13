//! Share vNext local metadata (plan 03 §3, migration `0012_share_vnext`).
//!
//! Projections only: rows describe servers, shares, perimeter threads,
//! captured checkout generations, encrypted object inventory and peer acks.
//! No plaintext tokens, absolute source paths or session ids are stored.

mod checkouts;
mod heads;
mod inventory;
mod sealed;
mod servers;
mod shares;
mod threads;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_heads;

pub use checkouts::ShareCheckoutStateRow;
pub use heads::ShareDeviceHead;
pub use inventory::{ObjectState, ShareObjectRow, SharePeerAck};
pub use servers::CollaborationServerRow;
pub use shares::{ShareRow, ShareStatus};
pub use threads::ShareThreadRow;

use trama_domain::DomainError;

pub(crate) fn invalid(what: &str, value: &str) -> DomainError {
    DomainError::Invalid(format!("unknown {what}: {value}"))
}

/// Converts a non-negative SQL integer column to `u64`.
pub(crate) fn to_u64(value: i64, what: &str) -> trama_domain::Result<u64> {
    u64::try_from(value).map_err(|_| DomainError::Storage(format!("negative {what}")))
}

/// Converts a `u64` to a SQL integer, rejecting values above `i64::MAX`.
pub(crate) fn to_i64(value: u64, what: &str) -> trama_domain::Result<i64> {
    i64::try_from(value).map_err(|_| DomainError::Invalid(format!("{what} out of range")))
}
