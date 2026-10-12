//! Local TramaWorkspace daemon: Unix-socket JSON-lines server and real sessions.

mod claude;
mod config;
mod connection;
mod error;
mod log;
mod session;

#[cfg(test)]
mod testing;

pub use error::{DaemonError, Result};
pub use session::SessionManager;

use crate::claude::ProductionAdapter;
use crate::config::{load_or_init_config, seed_from_config};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::watch;
use trama_db::TramaDb;

/// `$HOME/Library/Application Support/TramaWorkspace`
pub fn default_app_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        DaemonError::Invalid("HOME is not set; cannot locate Application Support".into())
    })?;
    Ok(PathBuf::from(home).join("Library/Application Support/TramaWorkspace"))
}

/// `$HOME/Library/Application Support/TramaWorkspace/daemon.sock`
pub fn default_socket_path() -> Result<PathBuf> {
    Ok(default_app_dir()?.join("daemon.sock"))
}

pub fn default_config_path() -> Result<PathBuf> {
    Ok(default_app_dir()?.join("config.json"))
}

pub fn default_db_path() -> Result<PathBuf> {
    Ok(default_app_dir()?.join("trama.db"))
}

pub async fn bootstrap_sessions() -> Result<SessionManager> {
    let dir = default_app_dir()?;
    std::fs::create_dir_all(&dir)?;
    let config = load_or_init_config(&dir.join("config.json"))?;
    let db = TramaDb::open(&dir.join("trama.db"))?;
    seed_from_config(&db, &config).await?;
    Ok(SessionManager::with_checkout_root(
        db,
        Arc::new(ProductionAdapter::new()),
        dir,
    ))
}

pub async fn run() -> Result<()> {
    run_on_socket(&default_socket_path()?).await
}

pub async fn run_on_socket(socket_path: &Path) -> Result<()> {
    let sessions = bootstrap_sessions().await?;
    serve_on_socket(socket_path, sessions).await
}

pub async fn serve_on_socket(socket_path: &Path, sessions: SessionManager) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(socket_path, perms)?;
    }
    crate::log::info(format!("listening on {}", socket_path.display()));

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    crate::log::info("shutdown requested");
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let sessions = sessions.clone();
                        let shutdown_tx = shutdown_tx.clone();
                        tokio::spawn(async move {
                            if let Err(err) =
                                connection::handle_connection(stream, sessions, shutdown_tx).await
                            {
                                crate::log::warn(format!("connection ended: {err}"));
                            }
                        });
                    }
                    Err(err) => {
                        crate::log::error(format!("accept failed: {err}"));
                    }
                }
            }
        }
    }

    let _ = std::fs::remove_file(socket_path);
    Ok(())
}
