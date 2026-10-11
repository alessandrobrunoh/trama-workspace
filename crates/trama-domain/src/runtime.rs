use crate::error::Result;
use async_channel::Sender;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where checkouts, processes and agents actually live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentSpec {
    Local,
    Ssh {
        host: String,
        user: Option<String>,
        port: u16,
        remote_root: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecSpec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub pty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    Interrupt,
    Terminate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecHandle {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecEvent {
    Started { pid: Option<u32> },
    Stdout { bytes: Vec<u8> },
    Stderr { bytes: Vec<u8> },
    Exit { code: Option<i32> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    Created,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchEvent {
    pub path: PathBuf,
    pub change: FileChange,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchHandle {
    pub id: String,
}

/// Port implemented by `LocalRunner` and `SshRunner` — the same interface,
/// so remote support is not a feature, it is an implementation.
#[async_trait]
pub trait ExecutionRuntime: Send + Sync {
    fn env(&self) -> &EnvironmentSpec;
    async fn exec(&self, spec: ExecSpec, events: Sender<ExecEvent>) -> Result<ExecHandle>;
    async fn signal(&self, handle: &ExecHandle, signal: Signal) -> Result<()>;
    async fn read_file(&self, path: &Path) -> Result<Vec<u8>>;
    async fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<()>;
    async fn list_dir(&self, path: &Path) -> Result<Vec<DirEntry>>;
    async fn watch(&self, path: &Path, events: Sender<WatchEvent>) -> Result<WatchHandle>;
}
