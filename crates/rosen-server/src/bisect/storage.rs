use super::known::KnownFailures;
use super::model::Session;
use rosen_protocol::{ErrorBody, ErrorCode};
use rusqlite::{params, Connection, OptionalExtension};

pub(crate) fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS bisections (
        id INTEGER PRIMARY KEY, ref_name TEXT NOT NULL,
        failed TEXT NOT NULL, commits TEXT NOT NULL,
        low INTEGER NOT NULL, high INTEGER NOT NULL,
        phase TEXT NOT NULL, step INTEGER NOT NULL, output TEXT NOT NULL,
        kind TEXT NOT NULL DEFAULT 'test' CHECK(kind IN ('build','test')),
        known_output TEXT, opened_by TEXT, opened_at INTEGER,
        step_offered_at INTEGER,
        platform TEXT NOT NULL DEFAULT '',
        UNIQUE(ref_name,failed,kind,platform));",
    )?;
    migrate_kind(conn)?;
    migrate_known_output(conn)?;
    super::progress::ensure_schema(conn)?;
    migrate_platform(conn)
}

pub(super) fn load(conn: &Connection, id: i64) -> Result<Session, ErrorBody> {
    let has_platform = crate::post_land::submitter::has_column(conn, "bisections", "platform")
        .map_err(super::internal)?;
    let has_step_offered =
        crate::post_land::submitter::has_column(conn, "bisections", "step_offered_at")
            .map_err(super::internal)?;
    let has_opened_at = crate::post_land::submitter::has_column(conn, "bisections", "opened_at")
        .map_err(super::internal)?;
    let platform_expr = if has_platform { "platform" } else { "''" };
    let offered_expr = if has_step_offered && has_opened_at {
        "COALESCE(step_offered_at, opened_at)"
    } else if has_step_offered {
        "step_offered_at"
    } else if has_opened_at {
        "opened_at"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT commits,low,high,phase,step,output,kind,known_output,{platform_expr},{offered_expr}
         FROM bisections WHERE id=?1"
    );
    let row = conn
        .query_row(&sql, [id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, Option<i64>>(9)?,
            ))
        })
        .optional()
        .map_err(super::internal)?
        .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "unknown bisection"))?;
    Ok(Session {
        id,
        commits: serde_json::from_str(&row.0).map_err(super::internal)?,
        low: row.1,
        high: row.2,
        phase: row.3,
        step: row.4,
        output: row.5,
        known: KnownFailures::from_stored(row.7),
        kind: if row.6 == "build" {
            rosen_protocol::CheckKind::Build
        } else {
            rosen_protocol::CheckKind::Test
        },
        platform: (!row.8.is_empty()).then_some(row.8),
        step_offered_at: row.9,
    })
}

pub(super) fn save(conn: &Connection, session: &Session) -> Result<(), ErrorBody> {
    conn.execute(
        "UPDATE bisections SET low=?2,high=?3,phase=?4,step=?5,output=?6,step_offered_at=?7 WHERE id=?1",
        params![
            session.id,
            session.low,
            session.high,
            session.phase,
            session.step,
            session.output,
            session.step_offered_at,
        ],
    )
    .map_err(super::internal)?;
    Ok(())
}

fn migrate_kind(conn: &Connection) -> rusqlite::Result<()> {
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bisections') WHERE name='kind')",
        [],
        |r| r.get(0),
    )?;
    if present {
        return Ok(());
    }
    if conn.is_autocommit() {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        migrate_kind(&tx)?;
        return tx.commit();
    }
    conn.execute_batch(
        "ALTER TABLE bisections RENAME TO bisections_legacy;
        CREATE TABLE bisections (
        id INTEGER PRIMARY KEY, ref_name TEXT NOT NULL,
        failed TEXT NOT NULL, commits TEXT NOT NULL,
        low INTEGER NOT NULL, high INTEGER NOT NULL,
        phase TEXT NOT NULL, step INTEGER NOT NULL, output TEXT NOT NULL,
        kind TEXT NOT NULL CHECK(kind IN ('build','test')), UNIQUE(ref_name,failed,kind));
        INSERT INTO bisections SELECT *, 'test' FROM bisections_legacy;
        DROP TABLE bisections_legacy;",
    )
}

fn migrate_known_output(conn: &Connection) -> rusqlite::Result<()> {
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bisections') WHERE name='known_output')",
        [],
        |r| r.get(0),
    )?;
    if present {
        return Ok(());
    }
    if conn.is_autocommit() {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        migrate_known_output(&tx)?;
        return tx.commit();
    }
    conn.execute_batch("ALTER TABLE bisections ADD COLUMN known_output TEXT")
}

fn migrate_platform(conn: &Connection) -> rusqlite::Result<()> {
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bisections') WHERE name='platform')",
        [],
        |r| r.get(0),
    )?;
    if present {
        return Ok(());
    }
    if conn.is_autocommit() {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        migrate_platform(&tx)?;
        return tx.commit();
    }
    conn.execute_batch(
        "ALTER TABLE bisections RENAME TO bisections_pre_platform;
        CREATE TABLE bisections (
        id INTEGER PRIMARY KEY, ref_name TEXT NOT NULL,
        failed TEXT NOT NULL, commits TEXT NOT NULL,
        low INTEGER NOT NULL, high INTEGER NOT NULL,
        phase TEXT NOT NULL, step INTEGER NOT NULL, output TEXT NOT NULL,
        kind TEXT NOT NULL CHECK(kind IN ('build','test')),
        known_output TEXT, opened_by TEXT, opened_at INTEGER,
        step_offered_at INTEGER,
        platform TEXT NOT NULL DEFAULT '',
        UNIQUE(ref_name,failed,kind,platform));
        INSERT INTO bisections
        (id,ref_name,failed,commits,low,high,phase,step,output,kind,known_output,opened_by,opened_at,step_offered_at,platform)
        SELECT id,ref_name,failed,commits,low,high,phase,step,output,kind,known_output,opened_by,opened_at,step_offered_at,''
        FROM bisections_pre_platform;
        DROP TABLE bisections_pre_platform;",
    )
}
