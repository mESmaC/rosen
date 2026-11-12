use rusqlite::Connection;

/// Creates the change tables if they are missing. Idempotent, so the server
/// can run it on every start.
///
/// IDs are stored in their 32-letter text form so that resolving a short
/// prefix typed by an agent is one range scan on the primary key.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS changes (
             id            TEXT PRIMARY KEY CHECK (length(id) = 32),
             owner         TEXT NOT NULL,
             parent_base   TEXT,
             parent_change TEXT REFERENCES changes(id),
             state         TEXT NOT NULL
                 CHECK (state IN ('open', 'ready', 'landed', 'abandoned')),
             head_tree     TEXT NOT NULL,
             created_at    INTEGER NOT NULL,
             updated_at    INTEGER NOT NULL,
             CHECK ((parent_base IS NULL) <> (parent_change IS NULL))
         );
         CREATE INDEX IF NOT EXISTS changes_by_owner ON changes(owner);
         CREATE INDEX IF NOT EXISTS changes_by_parent ON changes(parent_change);
         CREATE TABLE IF NOT EXISTS change_commits (
             change_id TEXT NOT NULL REFERENCES changes(id),
             seq       INTEGER NOT NULL,
             commit_id TEXT NOT NULL,
             PRIMARY KEY (change_id, seq)
         ) WITHOUT ROWID;",
    )
}
