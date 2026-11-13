//! Durable storage of each change's conflicts in the SQLite metadata store
//! (SPEC §3 "Metadata store", R5, R8).
//!
//! Every operation loads the change's [`ConflictSet`], applies the pure rule from
//! `rosen-core`, and writes the set back inside one transaction, so a crash leaves
//! the stored conflicts either before or after the operation, never between. The
//! landing transaction (R8) calls [`check_landable`] on its own transaction.

use rosen_core::conflict::{Conflict, ConflictError, ConflictId, ConflictSet, ConflictState};
use rusqlite::{params, Connection, Transaction};

/// Creates the `conflicts` table if it is missing. Safe to call on every start.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS conflicts (
             change_id   TEXT    NOT NULL,
             conflict_id TEXT    NOT NULL,
             path        TEXT    NOT NULL,
             start_line  INTEGER NOT NULL,
             base        BLOB    NOT NULL,
             ours        BLOB    NOT NULL,
             theirs      BLOB    NOT NULL,
             state       TEXT    NOT NULL CHECK (state IN ('open', 'resolved')),
             PRIMARY KEY (change_id, conflict_id)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS conflicts_by_state ON conflicts (change_id, state);",
    )
}

#[derive(Debug, thiserror::Error)]
pub enum ConflictStoreError {
    #[error(transparent)]
    Conflict(#[from] ConflictError),
    #[error("metadata store: {0}")]
    Sql(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, ConflictStoreError>;

/// Reads every conflict stored on a change, open and resolved.
pub fn load(conn: &Connection, change_id: &str) -> Result<ConflictSet> {
    let mut stmt = conn.prepare_cached(
        "SELECT conflict_id, path, start_line, base, ours, theirs, state
         FROM conflicts WHERE change_id = ?1",
    )?;
    let rows = stmt.query_map([change_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u32>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, Vec<u8>>(4)?,
            row.get::<_, Vec<u8>>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let mut conflicts = Vec::new();
    for row in rows {
        let (id, path, start_line, base, ours, theirs, state) = row?;
        let mut conflict = Conflict::new(path, start_line, base, ours, theirs);
        // The ID is content-derived; a mismatch means the row was altered outside Rosen.
        if conflict.id != id.parse::<ConflictId>()? {
            return Err(ConflictError::BadId(id).into());
        }
        conflict.state = state.parse::<ConflictState>()?;
        conflicts.push(conflict);
    }
    Ok(ConflictSet::from_stored(conflicts))
}

/// Replaces the stored conflicts of a change with `set`, writing only the rows
/// that differ. A conflict's ID is derived from its content, so a stored row
/// with the same ID differs only in state. Rewriting unchanged rows would cost
/// one write per stored conflict on every merge or resolve; the design proof
/// (`proofs/ConflictRouting`) shows the differing rows are the minimum.
pub(crate) fn save(tx: &Transaction<'_>, change_id: &str, set: &ConflictSet) -> Result<()> {
    let stored: std::collections::HashMap<String, String> = tx
        .prepare_cached("SELECT conflict_id, state FROM conflicts WHERE change_id = ?1")?
        .query_map([change_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let kept: std::collections::HashSet<String> =
        set.all().iter().map(|c| c.id.to_string()).collect();
    let mut delete =
        tx.prepare_cached("DELETE FROM conflicts WHERE change_id = ?1 AND conflict_id = ?2")?;
    for id in stored.keys().filter(|id| !kept.contains(*id)) {
        delete.execute(params![change_id, id])?;
    }
    let mut upsert = tx.prepare_cached(
        "INSERT OR REPLACE INTO conflicts
             (change_id, conflict_id, path, start_line, base, ours, theirs, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for c in set.all() {
        if stored.get(&c.id.to_string()).map(String::as_str) == Some(c.state.as_str()) {
            continue;
        }
        upsert.execute(params![
            change_id,
            c.id.to_string(),
            c.path,
            c.start_line,
            c.base,
            c.ours,
            c.theirs,
            c.state.as_str(),
        ])?;
    }
    Ok(())
}

/// Loads, changes and saves a change's conflicts in one transaction.
fn update<T>(
    conn: &mut Connection,
    change_id: &str,
    apply: impl FnOnce(&mut ConflictSet) -> Result<T>,
) -> Result<(T, ConflictSet)> {
    let tx = conn.transaction()?;
    let mut set = load(&tx, change_id)?;
    let value = apply(&mut set)?;
    save(&tx, change_id, &set)?;
    tx.commit()?;
    Ok((value, set))
}

/// Records the result of merging the files of one change: for each merged path,
/// the conflicts that merge produced (empty for a clean merge). All paths commit
/// together, so a rebase or land never stores a half-merged change.
///
/// Returns the change's conflicts after the merge; its `open_paths` are the files
/// a `conflict` event names.
pub fn record_merge(
    conn: &mut Connection,
    change_id: &str,
    merged: impl IntoIterator<Item = (String, Vec<Conflict>)>,
) -> Result<ConflictSet> {
    let (_, set) = update(conn, change_id, |set| {
        for (path, conflicts) in merged {
            set.record_merge(&path, conflicts);
        }
        Ok(())
    })?;
    Ok(set)
}

/// Resolves one conflict by ID (`rosen resolve <id>`) and returns it.
pub fn resolve(conn: &mut Connection, change_id: &str, id: ConflictId) -> Result<Conflict> {
    let (conflict, _) = update(conn, change_id, |set| Ok(set.resolve(id)?.clone()))?;
    Ok(conflict)
}

/// Resolves every open conflict in one file (`rosen resolve <path>`) and returns
/// how many were open.
pub fn resolve_path(conn: &mut Connection, change_id: &str, path: &str) -> Result<usize> {
    let (count, _) = update(conn, change_id, |set| Ok(set.resolve_path(path)))?;
    Ok(count)
}

/// The landing gate for conflicts: fails while the change has an open conflict.
/// Takes a plain connection so the land transaction (R8) can pass its own.
pub fn check_landable(conn: &Connection, change_id: &str) -> Result<()> {
    let open: i64 = conn.query_row(
        "SELECT COUNT(*) FROM conflicts WHERE change_id = ?1 AND state = 'open'",
        [change_id],
        |row| row.get(0),
    )?;
    if open == 0 {
        return Ok(());
    }
    Ok(load(conn, change_id)?.check_landable()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Risk (SPEC §2, R5, R8): a conflict that is lost from the store, or that a
    // re-merge silently drops, lets a change land with an unmerged region.
    #[test]
    fn open_conflicts_block_landing_until_resolved_or_merged_clean() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let conflict = Conflict::new("src/a.rs", 3, "base\n", "ours\n", "theirs\n");
        let merged = || vec![("src/a.rs".to_owned(), vec![conflict.clone()])];

        record_merge(&mut conn, "c1", merged()).unwrap();
        assert!(check_landable(&conn, "c1").is_err());
        assert!(check_landable(&conn, "c2").is_ok());

        resolve(&mut conn, "c1", conflict.id).unwrap();
        assert!(check_landable(&conn, "c1").is_ok());

        // The same region failing to merge again reopens it instead of trusting the old resolution.
        record_merge(&mut conn, "c1", merged()).unwrap();
        assert!(check_landable(&conn, "c1").is_err());

        record_merge(&mut conn, "c1", [("src/a.rs".to_owned(), Vec::new())]).unwrap();
        assert!(check_landable(&conn, "c1").is_ok());
    }
}
