//! vNext share transport (docs/plan/03 §4–§5): an HTTP client for the
//! `/v3/shares` routes, a resumable event stream per share that feeds
//! [`CollabService::apply_share_event`], manifest publish/fetch through a
//! [`PayloadCipher`] and writer leases.
//!
//! Payloads are opaque ciphertext here. The MLS layer plugs in as a
//! `PayloadCipher`; until then the service uses `NoCipher`, which refuses to
//! seal or open anything, so no plaintext manifest can leave the device.
//!
//! [`PayloadCipher`]: trama_collab_protocol::vnext::cipher::PayloadCipher

mod apply;
// The publish/manifest/lease API is driven by the Share/Join flow, which is
// built separately; tests exercise it end to end until then.
#[allow(dead_code)]
mod client;
#[allow(dead_code)]
mod manifests;
mod stream;

pub(crate) use client::ShareClient;
#[allow(unused_imports)]
pub(crate) use manifests::{FetchedManifests, fetch_manifests, publish_manifests};

#[cfg(test)]
mod tests;
