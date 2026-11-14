//! SPEC.md §3 risk: a partial schema or destructive upgrade loses authoritative
//! metadata; a reopen must preserve rows and restore connection-local safeguards.
use rosen_server::metadata;
use rusqlite::Connection;

#[test]
fn adopts_existing_metadata_and_reopens_with_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata.sqlite");
    let legacy = Connection::open(&path).unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE base_refs (name TEXT PRIMARY KEY, target TEXT NOT NULL);
         INSERT INTO base_refs VALUES ('main', 'preserved');",
        )
        .unwrap();
    drop(legacy);
    let conn = metadata::open(&path).unwrap();
    for table in [
        "changes",
        "change_snapshots",
        "path_claims",
        "conflicts",
        "review_threads",
        "review_comments",
        "receipts",
        "base_history",
    ] {
        conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
    }
    let mode: String = pragma(&conn, "journal_mode");
    assert_eq!(mode, "wal");
    let sync: i64 = pragma(&conn, "synchronous");
    assert_eq!(sync, 2);
    let version: i64 = pragma(&conn, "user_version");
    assert_eq!(version, metadata::SCHEMA_VERSION);
    assert!(conn
        .execute(
            "INSERT INTO path_claims VALUES ('missing', 'src', 'claim', 0)",
            []
        )
        .is_err());
    drop(conn);
    let conn = metadata::open(&path).unwrap();
    let tip: String = conn
        .query_row("SELECT target FROM base_refs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tip, "preserved");
    conn.pragma_update(None, "user_version", metadata::SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);
    assert!(metadata::open(&path)
        .unwrap_err()
        .to_string()
        .contains("newer"));
}

fn pragma<T: rusqlite::types::FromSql>(conn: &Connection, name: &str) -> T {
    conn.pragma_query_value(None, name, |row| row.get(0))
        .unwrap()
}
