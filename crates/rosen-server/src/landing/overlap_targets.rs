//! Durable overlap targets (SPEC R8 step 4). Claims disappear at release, so
//! recovery must reuse the selection made before closing the landed change.

use super::{ChangeId, LandError, LandResult};
use crate::changes::ChangeError;
use crate::overlaps::{self, Subject};
use crate::ServerState;
use rusqlite::{params, Connection, TransactionBehavior};

pub(super) fn load_or_capture(state: &ServerState, landed: ChangeId) -> LandResult<Vec<ChangeId>> {
    let (key, cached) = state
        .changes
        .with_conn(|conn| {
            ensure_schema(conn)?;
            let key = active_landing(conn, landed)?;
            adopt_legacy(conn, landed, &key)?;
            Ok((key.clone(), load(conn, &key)?))
        })
        .map_err(backend)?;
    if let Some(targets) = cached {
        return Ok(targets);
    }
    let targets = overlaps::find(state, &Subject::Change(landed.to_string()))
        .map_err(|err| LandError::Backend(err.to_string()))?;
    state
        .changes
        .with_conn(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if active_landing(&tx, landed)? != key {
                return Err(ChangeError::Corrupt(
                    "land changed during overlap capture".into(),
                ));
            }
            // Concurrent recovery may have captured first; preserve its complete set.
            if let Some(targets) = load(&tx, &key)? {
                return Ok(targets);
            }
            // A stored target already shows that the selection was made, and
            // every row commits together, so only an empty selection needs the
            // marker (proofs/TargetedRebase: one row per target, at least one).
            if targets.is_empty() {
                tx.execute(
                    "INSERT INTO landing_overlap_batches (landed) VALUES (?1)",
                    [&key],
                )?;
            }
            for target in &targets {
                tx.execute(
                    "INSERT INTO landing_overlap_targets (landed, target) VALUES (?1, ?2)",
                    params![key, target.change.to_string()],
                )?;
            }
            let saved = load(&tx, &key)?.unwrap_or_default();
            tx.commit()?;
            Ok(saved)
        })
        .map_err(backend)
}

fn ensure_schema(conn: &Connection) -> Result<(), ChangeError> {
    // The batch marker distinguishes a completed empty selection from no
    // selection. Batches captured before targets implied the selection still
    // carry a marker beside their targets; `load` reads both forms.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS landing_overlap_batches (
        landed TEXT PRIMARY KEY NOT NULL);
        CREATE TABLE IF NOT EXISTS landing_overlap_targets (
        landed TEXT NOT NULL, target TEXT NOT NULL, PRIMARY KEY (landed, target));",
    )?;
    Ok(())
}

fn load(conn: &Connection, key: &str) -> Result<Option<Vec<ChangeId>>, ChangeError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM landing_overlap_batches WHERE landed = ?1)
             OR EXISTS(SELECT 1 FROM landing_overlap_targets WHERE landed = ?1)",
        [key],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT target FROM landing_overlap_targets WHERE landed = ?1 ORDER BY target",
    )?;
    let ids = stmt
        .query_map([key], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.into_iter()
        .map(|text| text.parse().map_err(|_| ChangeError::Corrupt(text)))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn active_landing(conn: &Connection, id: ChangeId) -> Result<String, ChangeError> {
    Ok(conn.query_row(
        "SELECT commit_id FROM base_history WHERE change_id=?1
         AND NOT EXISTS(SELECT 1 FROM auto_reverts WHERE culprit=base_history.commit_id)
         ORDER BY seq DESC LIMIT 1",
        [id.to_string()],
        |r| r.get(0),
    )?)
}

/// Legacy batches used ChangeId. Adopt one only for a change with exactly one
/// historical land, because after a revert its old targets belong to that land.
/// Both copies and the marker commit together, including empty selections.
fn adopt_legacy(conn: &mut Connection, id: ChangeId, key: &str) -> Result<(), ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if load(&tx, key)?.is_some() {
        return Ok(());
    }
    tx.execute(
        "INSERT OR IGNORE INTO landing_overlap_targets(landed,target)
        SELECT ?1,target FROM landing_overlap_targets WHERE landed=?2
        AND (SELECT COUNT(*) FROM base_history WHERE change_id=?2)=1",
        params![key, id.to_string()],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO landing_overlap_batches(landed)
        SELECT ?1 FROM landing_overlap_batches WHERE landed=?2
        AND (SELECT COUNT(*) FROM base_history WHERE change_id=?2)=1",
        params![key, id.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

fn backend(err: ChangeError) -> LandError {
    LandError::Backend(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    // Risk (R8 recovery): an empty saved batch must not be recomputed after release.
    #[test]
    fn saved_selection_matches_design_fixtures() {
        let id = |n| ChangeId::from_bytes([n; 16]);
        for row in include_str!("../../../../proofs/fixtures/targeted-rebase.tsv").lines() {
            let cols: Vec<_> = row.split('\t').collect();
            let mut conn = Connection::open_in_memory().unwrap();
            ensure_schema(&conn).unwrap();
            // The design's rows: a marker for an empty selection, else one
            // row per target and no marker.
            if cols[0] == "-" {
                conn.execute(
                    "INSERT INTO landing_overlap_batches VALUES (?1)",
                    [id(0).to_string()],
                )
                .unwrap();
            } else if cols[0] != "none" {
                conn.execute(
                    "INSERT INTO landing_overlap_targets VALUES (?1, ?2)",
                    params![id(0).to_string(), id(1).to_string()],
                )
                .unwrap();
            }
            conn.execute_batch("CREATE TABLE base_history(change_id TEXT, commit_id TEXT)")
                .unwrap();
            conn.execute(
                "INSERT INTO base_history VALUES(?1,'commit')",
                [id(0).to_string()],
            )
            .unwrap();
            adopt_legacy(&mut conn, id(0), "commit").unwrap();
            let selected = load(&conn, "commit")
                .unwrap()
                .unwrap_or_else(|| vec![id(2)]);
            let expected = match cols[2] {
                "-" => vec![],
                "1" => vec![id(1)],
                _ => vec![id(2)],
            };
            assert_eq!(selected, expected);
        }
    }
}
