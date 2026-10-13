//! Errors of the E2EE layer. MLS errors are flattened to strings so callers
//! never depend on `openmls` types.

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, E2eeError>;

#[derive(Debug, thiserror::Error)]
pub enum E2eeError {
    /// An MLS operation failed (bad message, wrong epoch, invalid signature…).
    #[error("mls: {0}")]
    Mls(String),
    /// Bytes did not decode as the expected MLS wire structure.
    #[error("malformed {0}")]
    Malformed(&'static str),
    /// The message used a ciphersuite other than [`crate::CIPHERSUITE`].
    #[error("ciphersuite not allowed")]
    Ciphersuite,
    /// A received application message was not bound to the expected metadata.
    #[error("authenticated data mismatch")]
    AadMismatch,
    /// The message was a handshake/proposal where an application message was
    /// expected, or vice versa.
    #[error("unexpected message: {0}")]
    Unexpected(&'static str),
    /// No member with this identity is in the group.
    #[error("unknown member")]
    UnknownMember,
    /// Persisted epoch state is missing or corrupt.
    #[error("state: {0}")]
    State(String),
}

impl E2eeError {
    pub(crate) fn mls(e: impl std::fmt::Display) -> Self {
        Self::Mls(e.to_string())
    }
}
