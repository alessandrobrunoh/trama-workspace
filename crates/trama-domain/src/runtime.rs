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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_stable<T>(value: &T, expected: &str)
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let json = serde_json::to_string(value).unwrap();
        assert_eq!(json, expected, "stable json shape");
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(value).unwrap()
        );
    }

    #[test]
    fn exec_spec_serde_roundtrip() {
        let spec = ExecSpec {
            argv: vec!["git".into(), "status".into()],
            cwd: PathBuf::from("/tmp/work"),
            env: BTreeMap::from([("GIT_DIR".into(), "/tmp/work/.git".into())]),
            pty: true,
        };
        assert_stable(
            &spec,
            r#"{"argv":["git","status"],"cwd":"/tmp/work","env":{"GIT_DIR":"/tmp/work/.git"},"pty":true}"#,
        );

        let defaults: ExecSpec = serde_json::from_str(r#"{"argv":["true"],"cwd":"/"}"#).unwrap();
        assert!(defaults.env.is_empty());
        assert!(!defaults.pty);
    }

    #[test]
    fn exec_event_serde_roundtrip_every_variant() {
        assert_stable(
            &ExecEvent::Started { pid: Some(42) },
            r#"{"type":"started","pid":42}"#,
        );
        assert_stable(
            &ExecEvent::Started { pid: None },
            r#"{"type":"started","pid":null}"#,
        );
        assert_stable(
            &ExecEvent::Stdout {
                bytes: b"hello".to_vec(),
            },
            r#"{"type":"stdout","bytes":[104,101,108,108,111]}"#,
        );
        assert_stable(
            &ExecEvent::Stderr {
                bytes: vec![1, 2, 3],
            },
            r#"{"type":"stderr","bytes":[1,2,3]}"#,
        );
        assert_stable(
            &ExecEvent::Exit { code: Some(0) },
            r#"{"type":"exit","code":0}"#,
        );
        assert_stable(
            &ExecEvent::Exit { code: None },
            r#"{"type":"exit","code":null}"#,
        );
    }
}
