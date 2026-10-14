use crate::codec::{rfc3339, sql_err};
use crate::store::{BlobStore, TramaDb};
use async_trait::async_trait;
use chrono::Utc;
use std::path::PathBuf;
use trama_domain::revision::ContentStore;
use trama_domain::{DomainError, Result};

fn object_path(root: &std::path::Path, hash: &str) -> PathBuf {
    let prefix = hash.get(..2).unwrap_or(hash);
    root.join("objects").join(prefix).join(hash)
}

fn remember_object(conn: &rusqlite::Connection, hash: &str, size_bytes: u64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO content_objects (hash, size_bytes, created_at)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![hash, size_bytes as i64, rfc3339(Utc::now())],
    )
    .map_err(sql_err)?;
    Ok(())
}

#[async_trait]
impl ContentStore for TramaDb {
    async fn put(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        if hash.is_empty() {
            return Err(DomainError::Invalid(
                "content hash must not be empty".into(),
            ));
        }
        let hash = hash.to_owned();
        let bytes = bytes.to_vec();
        let inner = std::sync::Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            match &inner.blobs {
                BlobStore::Disk { root } => {
                    let dest = object_path(root, &hash);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let tmp = dest.with_file_name(format!("{hash}.tmp"));
                    std::fs::write(&tmp, &bytes)?;
                    std::fs::rename(&tmp, &dest)?;
                }
                BlobStore::Memory { map } => {
                    let mut guard = map
                        .lock()
                        .map_err(|_| DomainError::Storage("blob mutex poisoned".into()))?;
                    guard.insert(hash.clone(), bytes.clone());
                }
            }
            let conn = inner
                .conn
                .lock()
                .map_err(|_| DomainError::Storage("db mutex poisoned".into()))?;
            remember_object(&conn, &hash, bytes.len() as u64)
        })
        .await
        .map_err(|err| DomainError::Storage(format!("spawn_blocking: {err}")))?
    }

    async fn get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        if hash.is_empty() {
            return Ok(None);
        }
        let hash = hash.to_owned();
        let inner = std::sync::Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || match &inner.blobs {
            BlobStore::Disk { root } => {
                let dest = object_path(root, &hash);
                match std::fs::read(&dest) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(err) => Err(DomainError::from(err)),
                }
            }
            BlobStore::Memory { map } => {
                let guard = map
                    .lock()
                    .map_err(|_| DomainError::Storage("blob mutex poisoned".into()))?;
                Ok(guard.get(&hash).cloned())
            }
        })
        .await
        .map_err(|err| DomainError::Storage(format!("spawn_blocking: {err}")))?
    }
}
