//! API keys of user-added providers. Stored in the OS keychain (macOS
//! Keychain, Windows Credential Manager, Secret Service) under service
//! `trama`, account `provider:<id>`. When no keychain is reachable
//! (headless Linux, CI) or `TRAMA_PROVIDER_KEY_FILE=1` is set, keys go to
//! `<app_dir>/provider-keys.json` (0600 on Unix), the same fallback policy
//! as the E2EE at-rest key. Never written to `providers.json`, never logged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SERVICE: &str = "trama";
pub(crate) const KEY_FILE: &str = "provider-keys.json";

pub(crate) struct KeyStore {
    file: PathBuf,
    file_only: bool,
}

fn account(id: &str) -> String {
    format!("provider:{id}")
}

impl KeyStore {
    pub(crate) fn new(app_dir: &Path) -> Self {
        Self {
            file: app_dir.join(KEY_FILE),
            file_only: cfg!(test) || std::env::var_os("TRAMA_PROVIDER_KEY_FILE").is_some(),
        }
    }

    fn read_file(&self) -> BTreeMap<String, String> {
        std::fs::read_to_string(&self.file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write_file(&self, keys: &BTreeMap<String, String>) -> Result<(), String> {
        if keys.is_empty() {
            let _ = std::fs::remove_file(&self.file);
            return Ok(());
        }
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(keys).map_err(|e| e.to_string())?;
        let tmp = self.file.with_extension("json.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp).map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut file, text.as_bytes()).map_err(|e| e.to_string())?;
        drop(file);
        std::fs::rename(&tmp, &self.file).map_err(|e| e.to_string())
    }

    pub(crate) fn get(&self, id: &str) -> Option<String> {
        if let Some(key) = self.read_file().remove(id) {
            return Some(key);
        }
        if self.file_only {
            return None;
        }
        keyring::Entry::new(SERVICE, &account(id))
            .and_then(|entry| entry.get_password())
            .ok()
    }

    pub(crate) fn set(&self, id: &str, key: &str) -> Result<(), String> {
        if !self.file_only {
            let stored = keyring::Entry::new(SERVICE, &account(id)).and_then(|entry| {
                entry.set_password(key)?;
                // Read back: some backends accept writes they cannot persist.
                entry.get_password()
            });
            match stored {
                Ok(back) if back == key => {
                    // A stale fallback copy must not shadow the keychain.
                    let mut keys = self.read_file();
                    if keys.remove(id).is_some() {
                        self.write_file(&keys)?;
                    }
                    return Ok(());
                }
                Ok(_) => crate::log::warn("OS keychain did not keep the provider key; using file"),
                Err(err) => crate::log::warn(format!(
                    "OS keychain unavailable ({err}); using {KEY_FILE} fallback"
                )),
            }
        }
        let mut keys = self.read_file();
        keys.insert(id.to_owned(), key.to_owned());
        self.write_file(&keys)
    }

    pub(crate) fn delete(&self, id: &str) {
        let mut keys = self.read_file();
        if keys.remove(id).is_some() {
            let _ = self.write_file(&keys);
        }
        if !self.file_only
            && let Ok(entry) = keyring::Entry::new(SERVICE, &account(id))
        {
            let _ = entry.delete_credential();
        }
    }
}
