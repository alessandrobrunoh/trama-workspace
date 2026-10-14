//! Device identity: an MLS basic credential (the device id) and its Ed25519
//! signature key, plus key package generation.

use openmls::prelude::tls_codec::Serialize as _;
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;

use crate::provider::Provider;
use crate::{CIPHERSUITE, E2eeError, Result};

/// A device's MLS identity. The credential identity is the Trama device id;
/// binding it to an account (GitHub OAuth) is the server's job, out of scope.
pub struct DeviceIdentity {
    pub(crate) device_id: String,
    pub(crate) signer: SignatureKeyPair,
    pub(crate) credential: CredentialWithKey,
}

impl DeviceIdentity {
    /// Generates a fresh signature key for `device_id`.
    pub fn generate(device_id: &str) -> Result<Self> {
        let signer =
            SignatureKeyPair::new(CIPHERSUITE.signature_algorithm()).map_err(E2eeError::mls)?;
        Ok(Self::from_signer(device_id, signer))
    }

    fn from_signer(device_id: &str, signer: SignatureKeyPair) -> Self {
        let credential = CredentialWithKey {
            credential: BasicCredential::new(device_id.as_bytes().to_vec()).into(),
            signature_key: signer.public().into(),
        };
        Self {
            device_id: device_id.to_owned(),
            signer,
            credential,
        }
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Public signature key (Ed25519, raw bytes).
    pub fn signature_public_key(&self) -> &[u8] {
        self.signer.public()
    }

    /// Serializes the identity including the **private** key.
    pub fn to_secret_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(&(&self.device_id, &self.signer))
            .map_err(|e| E2eeError::State(e.to_string()))
    }

    pub fn from_secret_bytes(bytes: &[u8]) -> Result<Self> {
        let (id, signer): (String, SignatureKeyPair) =
            serde_json::from_slice(bytes).map_err(|e| E2eeError::State(e.to_string()))?;
        Ok(Self::from_signer(&id, signer))
    }

    /// Creates a single-use key package; its private part stays in
    /// `provider`, so the same provider must later process the Welcome.
    pub(crate) fn key_package(&self, provider: &Provider) -> Result<Vec<u8>> {
        let bundle = KeyPackage::builder()
            .build(CIPHERSUITE, provider, &self.signer, self.credential.clone())
            .map_err(E2eeError::mls)?;
        MlsMessageOut::from(bundle.key_package().clone())
            .tls_serialize_detached()
            .map_err(E2eeError::mls)
    }
}

/// Decodes and validates (signature, lifetime, ciphersuite) a key package
/// received from another device.
pub(crate) fn parse_key_package(provider: &Provider, bytes: &[u8]) -> Result<KeyPackage> {
    use openmls::prelude::tls_codec::Deserialize as _;
    let msg = MlsMessageIn::tls_deserialize_exact(bytes)
        .map_err(|_| E2eeError::Malformed("key package"))?;
    let MlsMessageBodyIn::KeyPackage(kp) = msg.extract() else {
        return Err(E2eeError::Malformed("key package"));
    };
    let kp = kp
        .validate(provider.crypto(), ProtocolVersion::Mls10)
        .map_err(E2eeError::mls)?;
    if kp.ciphersuite() != CIPHERSUITE {
        return Err(E2eeError::Ciphersuite);
    }
    Ok(kp)
}

pub(crate) fn identity_of(credential: &Credential) -> Result<String> {
    let basic = BasicCredential::try_from(credential.clone()).map_err(E2eeError::mls)?;
    String::from_utf8(basic.identity().to_vec()).map_err(|_| E2eeError::Malformed("credential"))
}
