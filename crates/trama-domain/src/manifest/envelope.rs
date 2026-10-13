//! `SignedEnvelope { payload_type, payload_bytes, alg, key_id, signature }`
//! (plan 03 §2). The signature covers a length-prefixed pre-authentication
//! encoding of the payload type and the canonical payload bytes, so a
//! signature for one manifest kind can never be replayed as another.

use super::canonical::{CanonicalError, from_canonical_bytes, to_canonical_bytes};
use crate::share::DeviceId;
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;

/// Domain-separation tag of the pre-authentication encoding.
const PAE_TAG: &[u8] = b"TRAMA-ENVELOPE-V1";

/// Signature algorithms. Only Ed25519 exists; there is no "none" and no
/// negotiable downgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureAlg {
    Ed25519,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedEnvelope {
    /// E.g. `trama.history_manifest.v1`.
    pub payload_type: String,
    #[serde(with = "b64")]
    pub payload_bytes: Vec<u8>,
    pub alg: SignatureAlg,
    /// Identifies the signing device key.
    pub key_id: String,
    #[serde(with = "b64")]
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("unexpected payload type {found:?}, expected {expected:?}")]
    PayloadType { expected: String, found: String },
    #[error("unknown signing key {0:?}")]
    UnknownKey(String),
    #[error("signing key {key_id:?} does not belong to publisher {publisher}")]
    PublisherMismatch { key_id: String, publisher: String },
    #[error("bad signature")]
    BadSignature,
    #[error(transparent)]
    Canonical(#[from] CanonicalError),
}

/// A device's public key as trusted by this device.
#[derive(Debug, Clone)]
pub struct TrustedKey {
    pub device_id: DeviceId,
    pub key: VerifyingKey,
}

/// Public keys this device trusts, by `key_id`. Fail closed: an empty
/// keyring verifies nothing.
#[derive(Debug, Clone, Default)]
pub struct Keyring(BTreeMap<String, TrustedKey>);

impl Keyring {
    pub fn insert(&mut self, key_id: impl Into<String>, device_id: DeviceId, key: VerifyingKey) {
        self.0.insert(key_id.into(), TrustedKey { device_id, key });
    }

    pub fn get(&self, key_id: &str) -> Option<&TrustedKey> {
        self.0.get(key_id)
    }
}

pub(crate) fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PAE_TAG.len() + payload_type.len() + payload.len() + 24);
    out.extend_from_slice(PAE_TAG);
    for part in [payload_type.as_bytes(), payload] {
        out.push(b' ');
        out.extend_from_slice(part.len().to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(part);
    }
    out
}

impl SignedEnvelope {
    /// Canonically encodes `payload` and signs it.
    pub fn seal<T: Serialize>(
        payload_type: &str,
        payload: &T,
        key_id: &str,
        key: &SigningKey,
    ) -> Result<Self, EnvelopeError> {
        let payload_bytes = to_canonical_bytes(payload)?;
        let signature = key.sign(&pae(payload_type, &payload_bytes));
        Ok(Self {
            payload_type: payload_type.to_string(),
            payload_bytes,
            alg: SignatureAlg::Ed25519,
            key_id: key_id.to_string(),
            signature: signature.to_bytes().to_vec(),
        })
    }

    /// Verifies type, key and signature, then decodes the canonical payload.
    /// Returns the payload and the device that signed it; callers must
    /// still check that device against the payload's declared publisher.
    pub fn open<T: DeserializeOwned>(
        &self,
        expected_type: &str,
        keyring: &Keyring,
    ) -> Result<(T, DeviceId), EnvelopeError> {
        if self.payload_type != expected_type {
            return Err(EnvelopeError::PayloadType {
                expected: expected_type.into(),
                found: self.payload_type.clone(),
            });
        }
        let trusted = keyring
            .get(&self.key_id)
            .ok_or_else(|| EnvelopeError::UnknownKey(self.key_id.clone()))?;
        let SignatureAlg::Ed25519 = self.alg;
        let signature =
            Signature::from_slice(&self.signature).map_err(|_| EnvelopeError::BadSignature)?;
        trusted
            .key
            .verify(&pae(&self.payload_type, &self.payload_bytes), &signature)
            .map_err(|_| EnvelopeError::BadSignature)?;
        let payload = from_canonical_bytes(&self.payload_bytes)?;
        Ok((payload, trusted.device_id.clone()))
    }
}

mod b64 {
    use super::{Engine, STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(D::Error::custom)
    }
}
