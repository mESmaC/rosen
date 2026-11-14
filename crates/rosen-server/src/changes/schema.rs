use rusqlite::Connection;

/// Creates the change tables if they are missing. Idempotent, so the server
/// can run it on every start.
///
/// IDs are stored in their 32-letter text form so that resolving a short
/// prefix typed by an agent is one range scan on the primary key.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    crate::metadata::ensure_schema(conn)
}

pub(crate) fn ensure_change_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS changes (
             id            TEXT PRIMARY KEY CHECK (length(id) = 32),
             owner         TEXT NOT NULL,
             parent_base   TEXT,
             parent_change TEXT REFERENCES changes(id),
             state         TEXT NOT NULL
                 CHECK (state IN ('open', 'ready', 'landed', 'abandoned')),
             head          TEXT NOT NULL,
             created_at    INTEGER NOT NULL,
             updated_at    INTEGER NOT NULL,
             CHECK ((parent_base IS NULL) <> (parent_change IS NULL))
         );
         CREATE INDEX IF NOT EXISTS changes_by_owner ON changes(owner);
         CREATE INDEX IF NOT EXISTS changes_by_parent ON changes(parent_change);
         CREATE TABLE IF NOT EXISTS stack_bases (
             change_id TEXT PRIMARY KEY REFERENCES changes(id),
             tree TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS stack_delivery (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             change_id TEXT NOT NULL,
             conflicted INTEGER NOT NULL,
             detail TEXT NOT NULL
         );
         -- A change's own commits, append-only (R2). A NULL message marks an
         -- automatic snapshot; `rosen commit` stores its message.
         CREATE TABLE IF NOT EXISTS change_snapshots (
             change_id TEXT NOT NULL REFERENCES changes(id),
             seq       INTEGER NOT NULL,
             commit_id TEXT NOT NULL,
             tree_id   TEXT NOT NULL,
             message   TEXT,
             PRIMARY KEY (change_id, seq)
         ) WITHOUT ROWID;
         -- An unedited legacy child's initial head is its exact parent basis.
         INSERT OR IGNORE INTO stack_bases (change_id, tree)
         SELECT id, head FROM changes c WHERE parent_change IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM change_snapshots s WHERE s.change_id = c.id);",
    )?;
    // The owner's machine (R14). Rows from before it was recorded keep ''
    // and show the bare nick: their machine is unknown, never guessed.
    add_text_column(conn, "changes", "owner_machine")
}

/// Adds a `TEXT NOT NULL DEFAULT ''` column unless `table` has it already.
pub(crate) fn add_text_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<()> {
    let present: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
        [table, column],
        |row| row.get(0),
    )?;
    if !present {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} TEXT NOT NULL DEFAULT ''"
        ))?;
    }
    Ok(())
}
