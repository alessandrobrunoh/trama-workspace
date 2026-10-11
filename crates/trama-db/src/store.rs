use crate::codec::sql_err;
use crate::migrations;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use trama_domain::{DomainError, Result};

/// On-disk blobs live at `<db_dir>/objects/<first 2 hash chars>/<hash>`.
/// The in-memory database keeps bytes in a HashMap instead.
pub(crate) enum BlobStore {
    Disk {
        root: PathBuf,
    },
    Memory {
        map: Mutex<HashMap<String, Vec<u8>>>,
    },
}

pub(crate) struct Inner {
    pub conn: Mutex<rusqlite::Connection>,
    pub blobs: BlobStore,
}

/// SQLite-backed Trama store.
///
/// The `rusqlite::Connection` is synchronous and wrapped in a `Mutex`.
/// Async trait methods hop to `tokio::task::spawn_blocking` so callers can
/// stay on a tokio runtime without blocking worker threads for the SQL itself
/// (the hop still serializes on the mutex — one writer/reader at a time).
#[derive(Clone)]
pub struct TramaDb {
    pub(crate) inner: Arc<Inner>,
}

impl TramaDb {
    /// Open (or create) a file-backed database, run PRAGMAs + migrations.
    /// Content blobs are stored beside the file under `objects/`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut conn = rusqlite::Connection::open(path).map_err(sql_err)?;
        migrations::configure_connection(&conn)?;
        migrations::apply(&mut conn)?;
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Ok(Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                blobs: BlobStore::Disk { root: dir },
            }),
        })
    }

    /// Open an isolated in-memory database. Content blobs live in a HashMap.
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = rusqlite::Connection::open_in_memory().map_err(sql_err)?;
        migrations::configure_connection(&conn)?;
        migrations::apply(&mut conn)?;
        Ok(Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                blobs: BlobStore::Memory {
                    map: Mutex::new(HashMap::new()),
                },
            }),
        })
    }

    /// Re-run migrations (no-op when already at latest). Used to prove
    /// `to_latest` is idempotent.
    pub fn migrate(&self) -> Result<()> {
        let mut guard = self
            .inner
            .conn
            .lock()
            .map_err(|_| DomainError::Storage("db mutex poisoned".into()))?;
        migrations::apply(&mut guard)
    }

    pub(crate) async fn blocking<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner
                .conn
                .lock()
                .map_err(|_| DomainError::Storage("db mutex poisoned".into()))?;
            f(&guard)
        })
        .await
        .map_err(|err| DomainError::Storage(format!("spawn_blocking: {err}")))?
    }

    pub(crate) async fn blocking_mut<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner
                .conn
                .lock()
                .map_err(|_| DomainError::Storage("db mutex poisoned".into()))?;
            f(&mut guard)
        })
        .await
        .map_err(|err| DomainError::Storage(format!("spawn_blocking: {err}")))?
    }
}
