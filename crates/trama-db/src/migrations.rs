use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};
use trama_domain::{DomainError, Result};

/// Migrations are applied by position (`user_version`), never by file name.
pub(crate) fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("../migrations/0001_schema.sql")),
        M::up(include_str!("../migrations/0002_checkout_locations.sql")),
        M::up(include_str!("../migrations/0003_subthreads.sql")),
        M::up(include_str!("../migrations/0004_comments.sql")),
        M::up(include_str!("../migrations/0005_composer.sql")),
        M::up(include_str!("../migrations/0006_collab.sql")),
        M::up(include_str!("../migrations/0007_message_claude_uuid.sql")),
        M::up(include_str!("../migrations/0008_subagents_checkpoints.sql")),
        M::up(include_str!("../migrations/0009_turn_activity.sql")),
        M::up(include_str!("../migrations/0010_thread_archive.sql")),
        M::up(include_str!("../migrations/0011_collab_history.sql")),
        M::up(include_str!("../migrations/0012_share_vnext.sql")),
        M::up(include_str!("../migrations/0013_share_transport.sql")),
        M::up(include_str!("../migrations/0014_share_e2ee.sql")),
        M::up(include_str!("../migrations/0015_share_message_state.sql")),
        M::up(include_str!("../migrations/0016_share_warnings.sql")),
        M::up(include_str!("../migrations/0017_comment_detach.sql")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_valid() {
        migrations().validate().expect("valid migrations");
    }

    #[test]
    fn share_vnext_tables_enforce_constraints() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        apply(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO shares (share_id, scope, policy_json, status, created_at)
                 VALUES ('s', 'subtree', '{}', 'preparing', 't');
             INSERT INTO share_threads (share_id, thread_gid, kind) VALUES ('s', 'g', 'main');
             INSERT INTO share_checkout_states (share_id, thread_gid, repo_gid, generation,
                 head_oid, index_tree_oid, working_tree_oid, manifest_content_id, status, created_at)
                 VALUES ('s', 'g', 'r', 0, 'h', 'i', 'w', 'm', 'ready', 't');",
        )
        .unwrap();
        let bad_scope = conn.execute(
            "INSERT INTO shares (share_id, scope, policy_json, status, created_at)
                 VALUES ('x', 'everything', '{}', 'preparing', 't')",
            [],
        );
        assert!(bad_scope.is_err());
        let orphan_state = conn.execute(
            "INSERT INTO share_checkout_states (share_id, thread_gid, repo_gid, generation,
                 head_oid, index_tree_oid, working_tree_oid, manifest_content_id, status, created_at)
                 VALUES ('s', 'missing', 'r', 0, 'h', 'i', 'w', 'm', 'ready', 't')",
            [],
        );
        assert!(
            orphan_state.is_err(),
            "states need a thread in the perimeter"
        );
        let bad_generation = conn.execute(
            "INSERT INTO share_checkout_states (share_id, thread_gid, repo_gid, generation,
                 previous_generation, head_oid, index_tree_oid, working_tree_oid,
                 manifest_content_id, status, created_at)
                 VALUES ('s', 'g', 'r', 1, 1, 'h', 'i', 'w', 'm', 'ready', 't')",
            [],
        );
        assert!(bad_generation.is_err());
    }
}
