//! At-rest sealing of E2EE state. A 32-byte key lives in the OS keychain
//! (macOS Keychain, Windows Credential Manager, Secret Service on Linux)
//! through `keyring`. When no keychain is reachable (headless Linux, CI)
//! or `TRAMA_E2EE_KEY_FILE=1` is set, the key falls back to
//! `<app_dir>/e2ee-at-rest.key` (0600 on Unix) next to the database: that
//! protects copies of the database alone, not a stolen app dir.

use std::path::Path;

use base64::{Engine, engine::general_purpose::STANDARD};
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

use crate::error::{DaemonError, Result};

const SERVICE: &str = "trama";
const ACCOUNT: &str = "e2ee-at-rest";
pub(crate) const KEY_FILE: &str = "e2ee-at-rest.key";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeySource {
    Keychain,
    File,
}

pub(crate) struct AtRestKey {
    key: LessSafeKey,
    pub(crate) source: KeySource,
}

fn err(what: impl std::fmt::Display) -> DaemonError {
    DaemonError::Collab(format!("e2ee at-rest key: {what}"))
}

fn random32() -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| err("rng"))?;
    Ok(bytes)
}

fn decode(text: &str) -> Result<[u8; 32]> {
    STANDARD
        .decode(text.trim())
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| err("stored key is malformed"))
}

fn from_keychain() -> std::result::Result<[u8; 32], keyring::Error> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT)?;
    match entry.get_password() {
        Ok(text) => decode(&text).map_err(|e| keyring::Error::Invalid("key".into(), e.to_string())),
        Err(keyring::Error::NoEntry) => {
            let key =
                random32().map_err(|e| keyring::Error::PlatformFailure(e.to_string().into()))?;
            entry.set_password(&STANDARD.encode(key))?;
            // Read back: some backends accept writes they cannot persist.
            let back = entry.get_password()?;
            decode(&back).map_err(|e| keyring::Error::Invalid("key".into(), e.to_string()))
        }
        Err(other) => Err(other),
    }
}

fn from_file(app_dir: &Path) -> Result<[u8; 32]> {
    let path = app_dir.join(KEY_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => decode(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = random32()?;
            std::fs::create_dir_all(app_dir).map_err(err)?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&path).map_err(err)?;
            std::io::Write::write_all(&mut file, STANDARD.encode(key).as_bytes()).map_err(err)?;
            Ok(key)
        }
        Err(e) => Err(err(e)),
    }
}

impl AtRestKey {
    pub(crate) fn load_or_create(app_dir: &Path) -> Result<Self> {
        // An existing fallback file wins, so a keychain that appears later
        // does not orphan state sealed with the file key.
        let file_only = cfg!(test)
            || std::env::var_os("TRAMA_E2EE_KEY_FILE").is_some()
            || app_dir.join(KEY_FILE).exists();
        if !file_only {
            match from_keychain() {
                Ok(key) => return Ok(Self::from_bytes(key, KeySource::Keychain)),
                Err(e) => crate::log::warn(format!(
                    "OS keychain unavailable ({e}); using {KEY_FILE} fallback"
                )),
            }
        }
        Ok(Self::from_bytes(from_file(app_dir)?, KeySource::File))
    }

    pub(crate) fn from_bytes(bytes: [u8; 32], source: KeySource) -> Self {
        let key = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &bytes).expect("32 bytes"));
        Self { key, source }
    }

    /// `nonce || ciphertext+tag`, bound to `name` so blobs cannot be swapped.
    pub(crate) fn seal(&self, name: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| err("rng"))?;
        let mut buf = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(name.as_bytes()),
                &mut buf,
            )
            .map_err(|_| err("seal"))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&buf);
        Ok(out)
    }

    pub(crate) fn open(&self, name: &str, sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() < NONCE_LEN {
            return Err(err("sealed blob too short"));
        }
        let (nonce, body) = sealed.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| err("nonce"))?;
        let mut buf = body.to_vec();
        let plain = self
            .key
            .open_in_place(nonce, Aad::from(name.as_bytes()), &mut buf)
            .map_err(|_| err("sealed blob does not authenticate"))?;
        Ok(plain.to_vec())
    }
}
