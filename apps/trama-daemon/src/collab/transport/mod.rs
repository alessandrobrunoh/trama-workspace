//! vNext share transport (docs/plan/03 §4–§5): an HTTP client for the
//! `/v3/shares` routes, a resumable event stream per share that feeds
//! [`CollabService::apply_share_event`], manifest publish/fetch through a
//! [`PayloadCipher`] and writer leases.
//!
//! Payloads are opaque ciphertext here: the daemon seals them with the
//! share's MLS group (`collab::e2ee`), never in plaintext.
//!
//! [`PayloadCipher`]: trama_collab_protocol::vnext::cipher::PayloadCipher

mod apply;
mod client;
mod manifests;
mod stream;

pub(crate) use client::ShareClient;
pub(crate) use manifests::{fetch_manifests, publish_manifests};

#[cfg(test)]
mod tests;
