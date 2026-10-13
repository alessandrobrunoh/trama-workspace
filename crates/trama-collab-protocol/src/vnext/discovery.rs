//! `GET /.well-known/trama-collab`: public, unauthenticated server metadata
//! used to negotiate a capability profile before creating a share.
//!
//! The document carries only public metadata (no users, shares or counts).

use serde::{Deserialize, Serialize};

/// Path of the discovery document.
pub const DISCOVERY_PATH: &str = "/.well-known/trama-collab";

/// Largest discovery body a client accepts.
pub const MAX_DISCOVERY_BYTES: usize = 64 * 1024;

/// An optional server feature. Unknown capabilities from newer servers are
/// kept as [`Capability::Unknown`] instead of failing the whole document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    E2ee,
    ContentTransfer,
    SubtreeShare,
    WorkspaceShare,
    ChunkResume,
    DirectTransport,
    #[serde(other)]
    Unknown,
}

/// Server-side maxima. `None` means not advertised (the client must assume
/// the v2 defaults).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLimits {
    #[serde(default)]
    pub max_object_bytes: Option<u64>,
    #[serde(default)]
    pub max_share_bytes: Option<u64>,
    #[serde(default)]
    pub max_event_payload_bytes: Option<u64>,
    #[serde(default)]
    pub max_events_per_publish: Option<u32>,
}

/// Public policy flags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerPolicy {
    /// Shares on this server must be end-to-end encrypted.
    #[serde(default)]
    pub e2ee_required: bool,
    /// Code grants need a Git authorization attestation (03 §5bis).
    #[serde(default)]
    pub git_authorization_required: bool,
}

/// The discovery document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerDiscovery {
    /// Stable server identity, opaque to clients.
    pub server_id: String,
    pub min_client_protocol: u32,
    pub max_client_protocol: u32,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub limits: ServerLimits,
    #[serde(default)]
    pub policy: ServerPolicy,
}

/// Why negotiation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiationError {
    /// The document is malformed (`min > max`, empty id).
    Invalid(&'static str),
    /// No protocol version in common.
    Incompatible {
        client_min: u32,
        client_max: u32,
        server_min: u32,
        server_max: u32,
    },
    /// Server policy requires a capability this client cannot provide, e.g.
    /// E2EE required but not offered by both sides. Never downgraded.
    PolicyUnsatisfiable(&'static str),
}

impl std::fmt::Display for NegotiationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(why) => write!(f, "invalid discovery document: {why}"),
            Self::Incompatible {
                client_min,
                client_max,
                server_min,
                server_max,
            } => write!(
                f,
                "no common collab protocol (client {client_min}..={client_max}, server {server_min}..={server_max})"
            ),
            Self::PolicyUnsatisfiable(why) => write!(f, "server policy cannot be met: {why}"),
        }
    }
}

impl std::error::Error for NegotiationError {}

/// The agreed session profile. The app shows only features listed here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedProfile {
    pub server_id: String,
    pub protocol: u32,
    /// Sorted, deduplicated, never contains `Unknown`.
    pub capabilities: Vec<Capability>,
    pub limits: ServerLimits,
    pub policy: ServerPolicy,
}

impl NegotiatedProfile {
    pub fn supports(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

impl ServerDiscovery {
    /// Picks the highest common protocol and intersects capabilities with
    /// what this client supports.
    pub fn negotiate(
        &self,
        client_min: u32,
        client_max: u32,
        client_capabilities: &[Capability],
    ) -> Result<NegotiatedProfile, NegotiationError> {
        if self.server_id.trim().is_empty() {
            return Err(NegotiationError::Invalid("empty server_id"));
        }
        if self.min_client_protocol > self.max_client_protocol {
            return Err(NegotiationError::Invalid(
                "min_client_protocol > max_client_protocol",
            ));
        }
        let low = client_min.max(self.min_client_protocol);
        let high = client_max.min(self.max_client_protocol);
        if low > high {
            return Err(NegotiationError::Incompatible {
                client_min,
                client_max,
                server_min: self.min_client_protocol,
                server_max: self.max_client_protocol,
            });
        }
        let mut capabilities: Vec<Capability> = self
            .capabilities
            .iter()
            .copied()
            .filter(|c| *c != Capability::Unknown && client_capabilities.contains(c))
            .collect();
        capabilities.sort();
        capabilities.dedup();
        if self.policy.e2ee_required && !capabilities.contains(&Capability::E2ee) {
            return Err(NegotiationError::PolicyUnsatisfiable("e2ee required"));
        }
        Ok(NegotiatedProfile {
            server_id: self.server_id.clone(),
            protocol: high,
            capabilities,
            limits: self.limits.clone(),
            policy: self.policy.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> ServerDiscovery {
        ServerDiscovery {
            server_id: "srv".into(),
            min_client_protocol: 2,
            max_client_protocol: 3,
            capabilities: vec![Capability::SubtreeShare, Capability::E2ee],
            limits: ServerLimits::default(),
            policy: ServerPolicy::default(),
        }
    }

    #[test]
    fn picks_highest_common_version_and_intersects_capabilities() {
        let p = doc().negotiate(2, 3, &[Capability::SubtreeShare]).unwrap();
        assert_eq!(p.protocol, 3);
        assert_eq!(p.capabilities, vec![Capability::SubtreeShare]);
        assert_eq!(doc().negotiate(1, 2, &[]).unwrap().protocol, 2);
    }

    #[test]
    fn rejects_incompatible_and_invalid_documents() {
        assert!(matches!(
            doc().negotiate(4, 5, &[]),
            Err(NegotiationError::Incompatible { .. })
        ));
        let mut bad = doc();
        bad.min_client_protocol = 9;
        assert!(matches!(
            bad.negotiate(2, 3, &[]),
            Err(NegotiationError::Invalid(_))
        ));
    }

    #[test]
    fn e2ee_policy_is_never_downgraded() {
        let mut d = doc();
        d.policy.e2ee_required = true;
        assert!(matches!(
            d.negotiate(2, 3, &[]),
            Err(NegotiationError::PolicyUnsatisfiable(_))
        ));
        assert!(d.negotiate(2, 3, &[Capability::E2ee]).is_ok());
    }

    #[test]
    fn unknown_capabilities_and_missing_fields_are_tolerated() {
        let d: ServerDiscovery = serde_json::from_str(
            r#"{"server_id":"s","min_client_protocol":2,"max_client_protocol":2,
                "capabilities":["e2ee","teleport"]}"#,
        )
        .unwrap();
        assert_eq!(d.capabilities, vec![Capability::E2ee, Capability::Unknown]);
        assert_eq!(d.limits, ServerLimits::default());
        let p = d.negotiate(2, 3, &[Capability::E2ee]).unwrap();
        assert_eq!(p.capabilities, vec![Capability::E2ee]);
    }
}
