//! SQLite connection setup, private files and backup restore.

use super::*;

pub(crate) fn open_connection(path: &Path) -> Result<Connection, String> {
    prepare_database_file(path)?;
    // macOS aliases such as /var -> /private/var are legitimate parent paths.
    // Resolve the parent only: the final component must still be protected by
    // SQLite NOFOLLOW rather than silently following a database-file symlink.
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent =
        dunce::canonicalize(parent).map_err(|e| format!("resolve database directory: {e}"))?;
    let filename = path
        .file_name()
        .ok_or_else(|| "database path must name a file".to_owned())?;
    let database_path = parent.join(filename);
    // No URI flag is passed, so the path is always treated as a plain
    // filename. NOFOLLOW refuses a symlinked final component even if one
    // appears between the checks above and this open.
    let connection = Connection::open_with_flags(
        database_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|e| format!("open database: {e}"))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|e| format!("configure database: {e}"))?;
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|e| format!("configure database: {e}"))?;
    Ok(connection)
}

/// Creates a new empty file readable by its owner only; fails if the path
/// exists (symlinks included). Missing parent directories are created 0700.
pub(crate) fn create_private_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    #[cfg(not(unix))]
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path);
    created.map(|_| ())
}

/// Replaces the database at `database` with the snapshot in `backup`
/// (made by [`AuthService::backup_to`]).
///
/// The snapshot must pass SQLite's integrity check and look like a
/// collaboration database. An existing database is kept next to the new one
/// as `<name>.before-restore-<unix time>`. Run it while the service is
/// stopped: the service holds no lock between requests, so a running one
/// would not notice the file being swapped.
pub fn restore_database(database: &Path, backup: &Path) -> Result<(), String> {
    let metadata =
        std::fs::symlink_metadata(backup).map_err(|e| format!("inspect backup file: {e}"))?;
    if !metadata.is_file() {
        return Err("backup path must be a regular file, not a symlink".into());
    }
    let resolved = dunce::canonicalize(backup).map_err(|e| format!("resolve backup path: {e}"))?;
    let source = Connection::open_with_flags(
        &resolved,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open backup: {e}"))?;
    let verdict: String = source
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| format!("check backup: {e}"))?;
    if verdict != "ok" {
        return Err("the backup failed SQLite's integrity check; nothing was changed".into());
    }
    let tables: i64 = source
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' \
             AND name IN ('auth_users', 'auth_sessions')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check backup: {e}"))?;
    if tables != 2 {
        return Err("that file is not a trama-collab database; nothing was changed".into());
    }

    let kept = if std::fs::symlink_metadata(database).is_ok() {
        let name = database
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "database path must name a file".to_owned())?;
        let kept = database.with_file_name(format!("{name}.before-restore-{}", unix_time()));
        std::fs::rename(database, &kept)
            .map_err(|e| format!("move the current database aside: {e}"))?;
        Some(kept)
    } else {
        None
    };
    let undo = |error: String| {
        let _ = std::fs::remove_file(database);
        if let Some(kept) = &kept {
            let _ = std::fs::rename(kept, database);
        }
        error
    };
    create_private_file(database).map_err(|e| undo(format!("create database file: {e}")))?;
    let target = dunce::canonicalize(database)
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .ok_or_else(|| undo("resolve database path".into()))?;
    source
        .execute("VACUUM INTO ?1", [target])
        .map_err(|e| undo(format!("write database: {e}")))?;
    Ok(())
}

/// Secures the database file before SQLite writes password hashes to it:
/// new files are pre-created owner-only (never the default 0644), existing
/// paths must be regular files (symlinks, dangling or not, are rejected)
/// and are tightened to 0600 whenever group or other permission bits are
/// set. Runs on every open, not only the first one.
pub(crate) fn prepare_database_file(path: &Path) -> Result<(), String> {
    // Only plain filesystem paths are supported: SQLite URI strings could
    // select modes or query parameters that bypass these checks.
    if path
        .as_os_str()
        .to_str()
        .is_some_and(|text| text.starts_with("file:"))
    {
        return Err("database path must be a plain file path, not a SQLite URI".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            // Pre-created owner-only; SQLite opens it as an existing file.
            Ok(_file) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(path)
                    .map_err(|e| format!("inspect database file: {e}"))?;
                if !metadata.is_file() {
                    return Err(
                        "database path must be a regular file; symlinks are not accepted".into(),
                    );
                }
                if metadata.permissions().mode() & 0o077 != 0 {
                    let mut permissions = metadata.permissions();
                    permissions.set_mode(0o600);
                    std::fs::set_permissions(path, permissions)
                        .map_err(|e| format!("restrict database file permissions: {e}"))?;
                }
                Ok(())
            }
            Err(e) => Err(format!("create database file: {e}")),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}
