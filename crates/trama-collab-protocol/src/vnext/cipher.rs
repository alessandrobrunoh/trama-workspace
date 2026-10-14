//! Payload encryption seam. The MLS layer (separate work) implements
//! [`PayloadCipher`]; transport code only moves the bytes it returns.
//! Until it exists, [`NoCipher`] fails closed: nothing is ever sent in
//! plaintext by accident.

/// What a payload is, so a cipher can bind it into its AAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    Event,
    HistoryManifest,
    ContentManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CipherError(pub String);

impl std::fmt::Display for CipherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "payload cipher: {}", self.0)
    }
}

impl std::error::Error for CipherError {}

/// Encrypts and decrypts share payloads for one key epoch.
pub trait PayloadCipher: Send + Sync {
    fn seal(
        &self,
        share_id: &str,
        key_epoch: u64,
        kind: PayloadKind,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CipherError>;

    fn open(
        &self,
        share_id: &str,
        key_epoch: u64,
        kind: PayloadKind,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, CipherError>;
}

/// Refuses every operation (no keys available).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCipher;

impl PayloadCipher for NoCipher {
    fn seal(&self, _: &str, _: u64, _: PayloadKind, _: &[u8]) -> Result<Vec<u8>, CipherError> {
        Err(CipherError("no encryption layer configured".into()))
    }

    fn open(&self, _: &str, _: u64, _: PayloadKind, _: &[u8]) -> Result<Vec<u8>, CipherError> {
        Err(CipherError("no encryption layer configured".into()))
    }
}
