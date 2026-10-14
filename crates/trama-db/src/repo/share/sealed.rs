//! Opaque sealed E2EE blobs (migration 0014). Synchronous on purpose: the
//! MLS epoch store trait is sync and is called from blocking tasks. Never
//! call these from an async task without `spawn_blocking`.

use crate::{TramaDb, codec::sql_err};
use rusqlite::{OptionalExtension, params};
use trama_domain::{DomainError, Result};

impl TramaDb {
    fn sealed_conn(&self) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>> {
        self.inner
            .conn
            .lock()
            .map_err(|_| DomainError::Storage("db mutex poisoned".into()))
    }

    pub fn sealed_get_sync(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.sealed_conn()?
            .query_row(
                "SELECT value FROM e2ee_sealed WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)
    }

    pub fn sealed_put_sync(&self, name: &str, value: &[u8]) -> Result<()> {
        self.sealed_conn()?
            .execute(
                "INSERT INTO e2ee_sealed (name, value) VALUES (?1, ?2)
                 ON CONFLICT (name) DO UPDATE SET value = excluded.value",
                params![name, value],
            )
            .map(|_| ())
            .map_err(sql_err)
    }

    pub fn sealed_delete_sync(&self, name: &str) -> Result<()> {
        self.sealed_conn()?
            .execute("DELETE FROM e2ee_sealed WHERE name = ?1", [name])
            .map(|_| ())
            .map_err(sql_err)
    }
}

#[cfg(test)]
mod tests {
    use crate::TramaDb;

    #[test]
    fn sealed_blobs_round_trip() {
        let db = TramaDb::open_in_memory().unwrap();
        assert_eq!(db.sealed_get_sync("a").unwrap(), None);
        db.sealed_put_sync("a", b"1").unwrap();
        db.sealed_put_sync("a", b"2").unwrap();
        assert_eq!(db.sealed_get_sync("a").unwrap().as_deref(), Some(&b"2"[..]));
        db.sealed_delete_sync("a").unwrap();
        assert_eq!(db.sealed_get_sync("a").unwrap(), None);
    }
}
