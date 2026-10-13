//! Thread rows, ACL checks and family lookups.

use super::*;

pub(crate) struct ThreadRow {
    pub(crate) title: String,
    /// Owner and access of the root (a child has no ACL of its own).
    pub(crate) owner_id: i64,
    pub(crate) owner: String,
    pub(crate) access: AccessLevel,
    pub(crate) via: MemberVia,
    pub(crate) parent_id: Option<String>,
    pub(crate) root_id: Option<String>,
    pub(crate) kind: Option<String>,
}

impl ThreadRow {
    pub(crate) fn summary(self, thread_id: String, last_seq: i64) -> ThreadSummary {
        ThreadSummary {
            thread_id,
            title: self.title,
            owner: self.owner,
            access: self.access,
            via: self.via,
            last_seq,
            parent_thread_id: self.parent_id,
            root_thread_id: self.root_id,
            kind: self.kind,
        }
    }
}

/// Returns the thread only if `user_id` is a member of it, or of the root
/// a child thread belongs to; otherwise `NotFound`, whether or not the
/// thread exists. Child threads inherit the root's membership, so revoking
/// access to the root revokes the whole family at once.
pub(crate) fn member_thread(
    connection: &Connection,
    thread_id: &str,
    user_id: i64,
) -> Result<ThreadRow, AuthError> {
    connection
        .query_row(
            "SELECT t.title, r.owner_id, o.username, r.access, m.via, t.parent_id, t.root_id, t.kind
             FROM collab_threads t
             JOIN collab_threads r ON r.id = COALESCE(t.root_id, t.id)
             JOIN collab_members m ON m.thread_id = r.id
             JOIN auth_users o ON o.id = r.owner_id
             WHERE t.id = ?1 AND m.user_id = ?2",
            params![thread_id, user_id],
            |row| {
                Ok(ThreadRow {
                    title: row.get(0)?,
                    owner_id: row.get(1)?,
                    owner: row.get(2)?,
                    access: untag(row.get(3)?)?,
                    via: untag(row.get(4)?)?,
                    parent_id: row.get(5)?,
                    root_id: row.get(6)?,
                    kind: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(db)?
        .ok_or(AuthError::NotFound)
}

/// Owner-only actions apply to a root thread; a child's access is its
/// root's, so managing it there is `400`.
pub(crate) fn owner_thread(
    connection: &Connection,
    thread_id: &str,
    user_id: i64,
) -> Result<ThreadRow, AuthError> {
    let thread = member_thread(connection, thread_id, user_id)?;
    if thread.owner_id != user_id {
        return Err(AuthError::Forbidden);
    }
    if thread.root_id.is_some() {
        return Err(AuthError::InvalidInput);
    }
    Ok(thread)
}

pub(crate) fn is_member(
    connection: &Connection,
    thread_id: &str,
    user_id: i64,
) -> Result<bool, AuthError> {
    connection
        .query_row(
            "SELECT 1 FROM collab_threads t
             JOIN collab_members m ON m.thread_id = COALESCE(t.root_id, t.id)
             WHERE t.id = ?1 AND m.user_id = ?2",
            params![thread_id, user_id],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(db)
}

/// Usernames with access to `thread_id` (the root's members).
pub(crate) fn member_names(
    connection: &Connection,
    thread_id: &str,
) -> Result<Vec<String>, AuthError> {
    let mut statement = connection
        .prepare(
            "SELECT u.username FROM collab_members m JOIN auth_users u ON u.id = m.user_id
             WHERE m.thread_id = (SELECT COALESCE(root_id, id) FROM collab_threads WHERE id = ?1)",
        )
        .map_err(db)?;
    statement
        .query_map([thread_id], |row| row.get::<_, String>(0))
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)
}

/// A root and every child under it: the threads whose sockets must
/// re-check access when the root's membership changes.
pub(crate) fn family_ids(connection: &Connection, root_id: &str) -> Result<Vec<String>, AuthError> {
    let mut statement = connection
        .prepare("SELECT id FROM collab_threads WHERE id = ?1 OR root_id = ?1")
        .map_err(db)?;
    statement
        .query_map([root_id], |row| row.get::<_, String>(0))
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)
}
