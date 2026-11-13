//! Durable overlap targets (SPEC R8 step 4). Claims disappear at release, so
//! recovery must reuse the selection made before closing the landed change.

use super::{ChangeId, LandError, LandResult};
use crate::changes::ChangeError;
use crate::overlaps::{self, Subject};
use crate::ServerState;
use rusqlite::{params, Connection, TransactionBehavior};

pub(super) fn load_or_capture(state: &ServerState, landed: ChangeId) -> LandResult<Vec<ChangeId>> {
    let cached = state
        .changes
        .with_conn(|conn| {
            ensure_schema(conn)?;
            load(conn, landed)
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
            // Concurrent recovery may have captured first; preserve its complete set.
            if let Some(targets) = load(&tx, landed)? {
                return Ok(targets);
            }
            tx.execute(
                "INSERT INTO landing_overlap_batches (landed) VALUES (?1)",
                [landed.to_string()],
            )?;
            for target in &targets {
                tx.execute(
                    "INSERT INTO landing_overlap_targets (landed, target) VALUES (?1, ?2)",
                    params![landed.to_string(), target.change.to_string()],
                )?;
            }
            let saved = load(&tx, landed)?.unwrap_or_default();
            tx.commit()?;
            Ok(saved)
        })
        .map_err(backend)
}

fn ensure_schema(conn: &Connection) -> Result<(), ChangeError> {
    // The batch marker distinguishes a completed empty selection from no selection.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS landing_overlap_batches (
        landed TEXT PRIMARY KEY NOT NULL);
        CREATE TABLE IF NOT EXISTS landing_overlap_targets (
        landed TEXT NOT NULL, target TEXT NOT NULL, PRIMARY KEY (landed, target));",
    )?;
    Ok(())
}

fn load(conn: &Connection, landed: ChangeId) -> Result<Option<Vec<ChangeId>>, ChangeError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM landing_overlap_batches WHERE landed = ?1)",
        [landed.to_string()],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT target FROM landing_overlap_targets WHERE landed = ?1 ORDER BY target",
    )?;
    let ids = stmt
        .query_map([landed.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.into_iter()
        .map(|text| text.parse().map_err(|_| ChangeError::Corrupt(text)))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
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
            let conn = Connection::open_in_memory().unwrap();
            ensure_schema(&conn).unwrap();
            if cols[0] != "none" {
                conn.execute(
                    "INSERT INTO landing_overlap_batches VALUES (?1)",
                    [id(0).to_string()],
                )
                .unwrap();
                if cols[0] != "-" {
                    conn.execute(
                        "INSERT INTO landing_overlap_targets VALUES (?1, ?2)",
                        params![id(0).to_string(), id(1).to_string()],
                    )
                    .unwrap();
                }
            }
            let selected = load(&conn, id(0)).unwrap().unwrap_or_else(|| vec![id(2)]);
            let expected = match cols[2] {
                "-" => vec![],
                "1" => vec![id(1)],
                _ => vec![id(2)],
            };
            assert_eq!(selected, expected);
        }
    }
}
