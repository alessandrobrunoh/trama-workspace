use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};
use trama_domain::{DomainError, Result};

pub(crate) fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("../migrations/0001_schema.sql")),
        M::up(include_str!("../migrations/0002_checkout_locations.sql")),
    ])
}

pub(crate) fn apply(conn: &mut Connection) -> Result<()> {
    migrations()
        .to_latest(conn)
        .map_err(|err| DomainError::Storage(format!("migration failed: {err}")))
}

pub(crate) fn configure_connection(conn: &Connection) -> Result<()> {
    // Per-connection PRAGMAs (and WAL/synchronous which persist on file DBs).
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(super::codec::sql_err)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(super::codec::sql_err)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(super::codec::sql_err)?;
    conn.pragma_update(None, "busy_timeout", 5000)
        .map_err(super::codec::sql_err)?;
    Ok(())
}
