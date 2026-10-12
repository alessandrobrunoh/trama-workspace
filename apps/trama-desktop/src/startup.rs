//! Find and start a local `trama-daemon` without downloading, building, or
//! replacing a daemon that is already accepting connections.
//!
//! `TRAMA_DAEMON` overrides the search. Otherwise the binary is the sibling of
//! this executable named `trama-daemon`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::net::default_socket_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonLocate {
    Found(PathBuf),
    Missing { instructions: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStart {
    Started,
    AlreadyRunning,
    Failed { message: String },
}

/// Locate the daemon binary. Does not download or compile anything.
pub fn locate_daemon() -> DaemonLocate {
    if let Some(override_path) = std::env::var_os("TRAMA_DAEMON") {
        let path = PathBuf::from(override_path);
        return if path.is_file() {
            DaemonLocate::Found(path)
        } else {
            DaemonLocate::Missing {
                instructions: format!(
                    "TRAMA_DAEMON points at {}, which is not a file.",
                    path.display()
                ),
            }
        };
    }

    let Ok(current) = std::env::current_exe() else {
        return DaemonLocate::Missing {
            instructions:
                "Could not resolve the TramaWorkspace executable, and TRAMA_DAEMON is unset.".into(),
        };
    };
    if let Some(dir) = current.parent() {
        for name in ["trama-daemon", "trama-daemon.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return DaemonLocate::Found(candidate);
            }
        }
    }
    DaemonLocate::Missing {
        instructions:
            "No trama-daemon binary was found next to TramaWorkspace, and TRAMA_DAEMON is unset."
                .into(),
    }
}

/// Spawn `binary` unless a daemon is already listening on the workspace socket.
/// The child is not killed, waited on, or given this process's controlling terminal.
pub fn start_daemon(binary: &Path) -> DaemonStart {
    if socket_accepts_connections() {
        return DaemonStart::AlreadyRunning;
    }
    match Command::new(binary).spawn() {
        Ok(mut child) => match child.try_wait() {
            Ok(Some(status)) => DaemonStart::Failed {
                message: format!("trama-daemon exited immediately ({status})."),
            },
            Ok(None) => DaemonStart::Started,
            Err(err) => DaemonStart::Failed {
                message: err.to_string(),
            },
        },
        Err(err) => DaemonStart::Failed {
            message: format!("Could not start {}: {err}", binary.display()),
        },
    }
}

fn socket_accepts_connections() -> bool {
    let Some(path) = default_socket_path() else {
        return false;
    };
    if !path.exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(&path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_override_explains_that_the_user_must_install_the_daemon() {
        let missing = DaemonLocate::Missing {
            instructions: locate_daemon_missing_message(),
        };
        let DaemonLocate::Missing { instructions } = missing else {
            unreachable!();
        };
        assert!(instructions.contains("TRAMA_DAEMON"));
        assert!(instructions.contains("trama-daemon"));
    }

    fn locate_daemon_missing_message() -> String {
        "No trama-daemon binary was found next to TramaWorkspace, and TRAMA_DAEMON is unset.".into()
    }
}
