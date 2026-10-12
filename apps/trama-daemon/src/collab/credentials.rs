//! Collab credentials on disk: `<app dir>/collab-credentials.json`.
//!
//! The file holds the session token (from username/password login; the
//! password itself is never stored) and the runner token. It is written
//! atomically with owner-only (0600) permissions, a symlink in its place is
//! refused, and a loosened mode is tightened back on load. `Debug` redacts
//! both tokens so they cannot reach a log line.

use crate::error::{DaemonError, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;

pub(crate) const CREDENTIALS_FILE: &str = "collab-credentials.json";
const MAX_FILE_BYTES: u64 = 64 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credentials {
    pub server_url: String,
    /// Canonical username as returned by the service at login.
    pub username: String,
    #[serde(default)]
    pub session_token: Option<String>,
    #[serde(default)]
    pub runner_token: Option<String>,
    /// `false` after `DisconnectCollab` without `forget`: kept, not used
    /// until the next `ConnectCollab`.
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

fn enabled_default() -> bool {
    true
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |token: &Option<String>| token.as_ref().map(|_| "<redacted>");
        f.debug_struct("Credentials")
            .field("server_url", &self.server_url)
            .field("username", &self.username)
            .field("session_token", &redact(&self.session_token))
            .field("runner_token", &redact(&self.runner_token))
            .field("enabled", &self.enabled)
            .finish()
    }
}

fn refuse_symlink(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(DaemonError::Invalid(format!(
            "refusing to use {}: it is a symbolic link",
            path.display()
        ))),
        Ok(meta) if !meta.is_file() => Err(DaemonError::Invalid(format!(
            "refusing to use {}: it is not a regular file",
            path.display()
        ))),
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

#[cfg(unix)]
fn tighten(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn tighten(_path: &Path) -> Result<()> {
    Ok(())
}

pub(crate) fn load(path: &Path) -> Result<Option<Credentials>> {
    if !refuse_symlink(path)? {
        return Ok(None);
    }
    tighten(path)?;
    let mut raw = String::new();
    std::fs::File::open(path)?
        .take(MAX_FILE_BYTES)
        .read_to_string(&mut raw)?;
    let credentials = serde_json::from_str(&raw).map_err(|_| {
        DaemonError::Invalid(format!(
            "{} is not valid; delete it and connect again",
            path.display()
        ))
    })?;
    Ok(Some(credentials))
}

/// Write to a fresh 0600 temporary file beside `path`, sync, then rename.
pub(crate) fn store(path: &Path, credentials: &Credentials) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| DaemonError::Invalid("credentials path has no directory".into()))?;
    std::fs::create_dir_all(parent)?;
    refuse_symlink(path)?;
    let json = serde_json::to_vec_pretty(credentials)?;
    let temp = parent.join(format!(
        ".{CREDENTIALS_FILE}.{}.tmp",
        trama_domain::ids::MessageId::generate()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = (|| -> Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written?;
    tighten(path)
}

pub(crate) fn remove(path: &Path) -> Result<()> {
    refuse_symlink(path)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Credentials {
        Credentials {
            server_url: "http://127.0.0.1:8787".into(),
            username: "alice".into(),
            session_token: Some("0123abcdsessionsecret".into()),
            runner_token: Some("trr_runnersecret".into()),
            enabled: true,
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn credentials_are_owner_only_redacted_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CREDENTIALS_FILE);
        assert!(load(&path).unwrap().is_none());
        store(&path, &sample()).unwrap();
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
        assert_eq!(load(&path).unwrap(), Some(sample()));
        // No temporary files are left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        let debug = format!("{:?}", sample());
        assert!(!debug.contains("sessionsecret"), "{debug}");
        assert!(!debug.contains("runnersecret"), "{debug}");
        assert!(debug.contains("<redacted>"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            load(&path).unwrap();
            assert_eq!(mode(&path), 0o600, "a loosened file is tightened on load");
        }
        remove(&path).unwrap();
        remove(&path).unwrap();
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_credentials_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.json");
        std::fs::write(&target, "{}").unwrap();
        let path = dir.path().join(CREDENTIALS_FILE);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(load(&path).is_err());
        assert!(store(&path, &sample()).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{}");
    }
}
