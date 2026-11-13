use super::{LandError, LandResult};
use rosen_core::{ChangeId, ObjectId};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

pub(super) fn ensure_schema(conn: &Connection) -> LandResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS auto_reverts (
        culprit TEXT PRIMARY KEY, change_id TEXT NOT NULL, commit_id TEXT NOT NULL,
        output TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0)",
    )?;
    Ok(())
}

pub(super) struct Revert {
    pub culprit: ObjectId,
    pub base: ObjectId,
    pub commit: ObjectId,
    pub id: ChangeId,
    pub tree: ObjectId,
    pub reopened_head: ObjectId,
}

pub(super) fn completed(conn: &Connection, culprit: ObjectId) -> LandResult<Option<ObjectId>> {
    let found: Option<String> = conn
        .query_row(
            "SELECT commit_id FROM auto_reverts WHERE culprit=?1",
            [culprit.to_hex()],
            |r| r.get(0),
        )
        .optional()?;
    found
        .map(|s| ObjectId::from_hex(&s).map_err(Into::into))
        .transpose()
}

/// One CAS transaction owns the exceptional receipt/approval exemption. No
/// caller-supplied change can use it: the server change is created here.
pub(super) fn commit(
    conn: &mut Connection,
    name: &str,
    p: &Revert,
    output: &str,
) -> LandResult<bool> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: String =
        tx.query_row("SELECT target FROM base_refs WHERE name=?1", [name], |r| {
            r.get(0)
        })?;
    let (change, recovered) = culprit_change(&tx, name, p.culprit)?;
    if !eligible(
        completed(&tx, p.culprit)?.is_some(),
        current != p.base.to_hex(),
        true,
        recovered,
    ) {
        if !recovered {
            return Err(LandError::Backend(
                "culprit post-land work is pending; retry after recovery".into(),
            ));
        }
        return Ok(false);
    }
    let moved = tx.execute(
        "UPDATE base_refs SET target=?1 WHERE name=?2 AND target=?3",
        params![p.commit.to_hex(), name, p.base.to_hex()],
    )?;
    if moved == 0 {
        return Ok(false);
    }
    crate::changes::update_in(&tx, change.parse()?, |c| {
        c.reopen_after_revert(p.commit, p.reopened_head)
    })
    .map_err(|e| LandError::Backend(e.to_string()))?;
    record_inverse(&tx, name, p, &change, output)?;
    tx.commit()?;
    Ok(true)
}

fn culprit_change(conn: &Connection, name: &str, culprit: ObjectId) -> LandResult<(String, bool)> {
    let row: Option<(Option<String>, bool)> = conn
        .query_row(
            "SELECT change_id,post_land_done FROM base_history WHERE ref_name=?1 AND commit_id=?2",
            params![name, culprit.to_hex()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((Some(change), recovered)) => Ok((change, recovered)),
        _ => Err(LandError::Backend("culprit is not a landed change".into())),
    }
}

pub(super) fn culprit_recovered(
    conn: &Connection,
    name: &str,
    culprit: ObjectId,
) -> LandResult<bool> {
    culprit_change(conn, name, culprit).map(|(_, recovered)| recovered)
}

pub(super) fn eligible(already: bool, moved: bool, clean: bool, recovered: bool) -> bool {
    recovered && !already && !moved && clean
}

#[test]
fn inverse_gate_matches_proved_fixtures() {
    // R12 risk: replaying an inverse or replacing a concurrently moved base.
    for row in include_str!("../../../../proofs/fixtures/auto-revert.tsv")
        .lines()
        .skip(1)
    {
        let bits: Vec<bool> = row.split('\t').map(|s| s == "1").collect();
        assert_eq!(eligible(bits[0], bits[1], bits[2], bits[3]), bits[4]);
    }
}

fn insert_server_change(tx: &Connection, p: &Revert) -> LandResult<()> {
    tx.execute(
        "INSERT INTO changes(id,owner,parent_base,state,head,created_at,updated_at)
        VALUES(?1,'rosen-server',?2,'landed',?3,0,0)",
        params![p.id.to_string(), p.base.to_hex(), p.tree.to_hex()],
    )?;
    Ok(())
}

pub(super) fn output(conn: &Connection, culprit: ObjectId) -> LandResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT output FROM auto_reverts WHERE culprit=?1",
            [culprit.to_hex()],
            |r| r.get(0),
        )
        .optional()?)
}

pub(super) fn pending(conn: &Connection) -> LandResult<Vec<(String, String, String, String)>> {
    let mut q = conn
        .prepare("SELECT culprit,change_id,commit_id,output FROM auto_reverts WHERE delivered=0")?;
    let rows = q
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

pub(super) fn delivered(conn: &Connection, culprit: &str) -> LandResult<()> {
    conn.execute(
        "UPDATE auto_reverts SET delivered=1 WHERE culprit=?1",
        [culprit],
    )?;
    Ok(())
}

/// Older servers allowed only one historical land per change. Reopened R12
/// changes need another land; retain every history row and index active lookup.
/// SAVEPOINT works both standalone and inside metadata initialization.
pub(super) fn migrate_history(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("SAVEPOINT auto_revert_migration")?;
    let result = migrate_history_in(conn);
    if result.is_err() {
        conn.execute_batch("ROLLBACK TO auto_revert_migration; RELEASE auto_revert_migration")?;
    } else {
        conn.execute_batch("RELEASE auto_revert_migration")?;
    }
    result
}

fn migrate_history_in(conn: &Connection) -> rusqlite::Result<()> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE name='base_history'",
        [],
        |r| r.get(0),
    )?;
    if sql.contains("change_id      TEXT UNIQUE") {
        let replacement = sql
            .replace("base_history (", "base_history_revert_migration (")
            .replace("change_id      TEXT UNIQUE", "change_id      TEXT");
        conn.execute_batch(&replacement)?;
        conn.execute_batch("INSERT INTO base_history_revert_migration SELECT * FROM base_history;
            DROP TABLE base_history; ALTER TABLE base_history_revert_migration RENAME TO base_history;")?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS base_history_change ON base_history(change_id);
        CREATE TABLE IF NOT EXISTS auto_reverts (
        culprit TEXT PRIMARY KEY, change_id TEXT NOT NULL, commit_id TEXT NOT NULL,
        output TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0)",
    )
}

fn record_inverse(
    tx: &Connection,
    name: &str,
    p: &Revert,
    change: &str,
    output: &str,
) -> LandResult<()> {
    // Record the inverse before reopening can be landed again. The active
    // landing lookup excludes this commit while history retains its owner.
    tx.execute(
        "INSERT INTO auto_reverts(culprit,change_id,commit_id,output) VALUES(?1,?2,?3,?4)",
        params![p.culprit.to_hex(), change, p.commit.to_hex(), output],
    )?;
    tx.execute(
        "UPDATE base_history SET post_land_done=1 WHERE commit_id=?1",
        [p.culprit.to_hex()],
    )?;
    tx.execute(
        "UPDATE review_approvals SET valid=0 WHERE change_id=?1",
        [&change],
    )?;
    insert_server_change(tx, p)?;
    tx.execute("INSERT INTO base_history(ref_name,commit_id,change_id,head,tier) VALUES(?1,?2,?3,?4,'normal')",
        params![name, p.commit.to_hex(), p.id.to_string(), p.tree.to_hex()])?;
    Ok(())
}
