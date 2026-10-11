use crate::error::Result;
use crate::ids::RepositoryId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One recorded state of a file. Contents live in the `ContentStore`,
/// never in SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRevision {
    pub repository_id: RepositoryId,
    pub path: PathBuf,
    pub revision: u64,
    pub content_hash: String,
    pub size_bytes: u64,
    pub event_seq: Option<u64>,
    pub recorded_at: DateTime<Utc>,
}

/// Content-addressed blob storage (hash -> bytes).
#[async_trait]
pub trait ContentStore: Send + Sync {
    async fn put(&self, hash: &str, bytes: &[u8]) -> Result<()>;
    async fn get(&self, hash: &str) -> Result<Option<Vec<u8>>>;
}

/// Append-only file revision log per repository.
#[async_trait]
pub trait RevisionStore: Send + Sync {
    async fn record(&self, revision: FileRevision) -> Result<()>;
    async fn latest(
        &self,
        repository_id: &RepositoryId,
        path: &Path,
    ) -> Result<Option<FileRevision>>;
    async fn history(
        &self,
        repository_id: &RepositoryId,
        path: &Path,
        limit: u32,
    ) -> Result<Vec<FileRevision>>;
}
