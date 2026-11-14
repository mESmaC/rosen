//! Path claim records in the metadata store. Every operation is one
//! `BEGIN IMMEDIATE` transaction: SQLite's write lock serialises them across
//! every connection, so two claims on one path are decided one after the
//! other and the second always sees the first (R6 accept rule).

use super::delivery::Releaser;
use crate::changes::{self, ChangeError};
use rosen_core::{blockers, ChangeId, HeldClaim, PathPattern};
use rosen_protocol::{
    ClaimHolder, ClaimKind, ClaimOutcome, PathClaim, ReleaseOutcome, ShareOutcome, WokenWait,
};
use rusqlite::{params, Connection, Row, Transaction, TransactionBehavior};
use std::time::{SystemTime, UNIX_EPOCH};

/// Creates the claims table if it is missing. Safe to call on every start.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    super::delivery::ensure_schema(conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS path_claims (
             change_id  TEXT    NOT NULL REFERENCES changes(id),
             pattern    TEXT    NOT NULL,
             kind       TEXT    NOT NULL CHECK (kind IN ('claim', 'share', 'wait')),
             created_at INTEGER NOT NULL,
             PRIMARY KEY (change_id, kind, pattern)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS path_claims_by_kind ON path_claims(kind);",
    )
}

/// Records joined with their change. Only open and ready changes are live:
/// a landed or abandoned change's leftover rows never block anyone, even if
/// a crash came between closing the change and releasing its claims.
const SELECT: &str = "SELECT pc.change_id, c.owner, pc.pattern, pc.kind, pc.created_at
     FROM path_claims pc JOIN changes c ON c.id = pc.change_id";
const LIVE: &str = "c.state IN ('open', 'ready')";

/// Grants every path to the change, or none of them and names the holders.
pub fn claim(
    conn: &mut Connection,
    change: &str,
    paths: &[String],
) -> Result<ClaimOutcome, ChangeError> {
    let patterns = parse(paths)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = open_change(&tx, change)?;
    let holders = holders(&tx, id, &patterns)?;
    if !holders.is_empty() {
        return Ok(ClaimOutcome::Refused { holders });
    }
    let now = now_millis();
    for pattern in &patterns {
        insert(&tx, id, pattern, ClaimKind::Claim, now)?;
        // Owning the path supersedes sharing or waiting on it.
        tx.execute(
            "DELETE FROM path_claims WHERE change_id = ?1 AND pattern = ?2 AND kind <> 'claim'",
            params![id.to_string(), pattern.as_str()],
        )?;
    }
    // Staged with the grant, so a stop after the commit cannot lose it (R9).
    let detail = serde_json::json!({ "paths": texts(&patterns) });
    let event = crate::events::Staged::about(rosen_protocol::EventKind::Claim, id, detail);
    crate::events::stage(&tx, &event)?;
    tx.commit()?;
    Ok(ClaimOutcome::Granted {
        change: id,
        paths: texts(&patterns),
    })
}

/// Records shares. A share never blocks and is never refused.
pub fn share(
    conn: &mut Connection,
    change: &str,
    paths: &[String],
) -> Result<ShareOutcome, ChangeError> {
    let patterns = parse(paths)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = open_change(&tx, change)?;
    let holders = holders(&tx, id, &patterns)?;
    let now = now_millis();
    for pattern in &patterns {
        insert(&tx, id, pattern, ClaimKind::Share, now)?;
    }
    tx.commit()?;
    Ok(ShareOutcome {
        change: id,
        paths: texts(&patterns),
        holders,
    })
}

/// Records a wait on each blocked path. A path nobody else claims is free and
/// gets no record: a wait is stored only while something blocks it, so a
/// stored wait always has a holder whose release will wake it.
pub fn wait(
    conn: &mut Connection,
    change: &str,
    paths: &[String],
) -> Result<(ChangeId, Vec<ClaimHolder>, Vec<String>), ChangeError> {
    let patterns = parse(paths)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = open_change(&tx, change)?;
    let now = now_millis();
    let mut waiting_on = Vec::new();
    let mut free = Vec::new();
    for pattern in &patterns {
        let blocked_by = holders(&tx, id, std::slice::from_ref(pattern))?;
        if blocked_by.is_empty() {
            free.push(pattern.to_string());
        } else {
            insert(&tx, id, pattern, ClaimKind::Wait, now)?;
            waiting_on.extend(blocked_by);
        }
    }
    tx.commit()?;
    Ok((id, waiting_on, free))
}

/// Drops the change's records on `paths` (every path when empty), then wakes
/// each wait that no remaining claim blocks.
pub fn release(
    conn: &mut Connection,
    change: &str,
    paths: &[String],
) -> Result<ReleaseOutcome, ChangeError> {
    let patterns = parse(paths)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = changes::resolve(&tx, change)?;
    let outcome = release_in(&tx, id, Some(patterns.as_slice()).filter(|p| !p.is_empty()))?;
    tx.commit()?;
    Ok(outcome)
}

/// Drops every record of a change that landed (R6, R8 step 4). Landing
/// announces the land's own `released` event, so only woken waiters get one
/// from here.
pub fn release_all(conn: &mut Connection, id: ChangeId) -> Result<ReleaseOutcome, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let outcome = release_records(&tx, id, None, Releaser::Silent)?;
    tx.commit()?;
    Ok(outcome)
}

/// Records of live changes, oldest first; one change's when `change` is set.
pub fn list(conn: &mut Connection, change: Option<&str>) -> Result<Vec<PathClaim>, ChangeError> {
    match change {
        Some(text) => {
            let id = changes::resolve(conn, text)?;
            let sql = format!("{SELECT} WHERE pc.change_id = ?1 ORDER BY pc.created_at");
            rows(conn, &sql, [id.to_string()])
        }
        None => {
            let sql = format!("{SELECT} WHERE {LIVE} ORDER BY pc.created_at");
            rows(conn, &sql, [])
        }
    }
}

/// Releases a change's records and queues a `released` event for the
/// releaser and one for each woken waiter.
pub(crate) fn release_in(
    tx: &Transaction,
    id: ChangeId,
    only: Option<&[PathPattern]>,
) -> Result<ReleaseOutcome, ChangeError> {
    release_records(tx, id, only, Releaser::Announce)
}

fn release_records(
    tx: &Transaction,
    id: ChangeId,
    only: Option<&[PathPattern]>,
    releaser: Releaser,
) -> Result<ReleaseOutcome, ChangeError> {
    let sql = format!("{SELECT} WHERE pc.change_id = ?1");
    let mut released = rows(tx, &sql, [id.to_string()])?;
    if let Some(only) = only {
        released.retain(|record| only.iter().any(|p| p.as_str() == record.path));
    }
    for record in &released {
        tx.execute(
            "DELETE FROM path_claims WHERE change_id = ?1 AND pattern = ?2 AND kind = ?3",
            params![id.to_string(), record.path, kind_text(record.kind)],
        )?;
    }
    let freed: Vec<PathPattern> = released
        .iter()
        .filter(|record| record.kind == ClaimKind::Claim)
        .map(|record| stored_pattern(&record.path))
        .collect::<Result<_, _>>()?;
    let woken = wake_unblocked(tx, &freed)?;
    let outcome = ReleaseOutcome {
        change: id,
        released,
        woken,
    };
    super::delivery::enqueue(tx, &outcome, releaser)?;
    Ok(outcome)
}

/// Ends every live wait that overlaps a freed path and that no live claim on
/// another change still blocks. Only waits touching `freed` are examined: a
/// wait elsewhere was blocked before and nothing that blocks it changed.
fn wait_candidates(tx: &Transaction, freed: &[PathPattern]) -> Result<Vec<PathClaim>, ChangeError> {
    let sql = format!("{SELECT} WHERE pc.kind = 'wait' AND {LIVE} ORDER BY pc.created_at");
    let mut waits = rows(tx, &sql, [])?;
    let mut keep = Vec::with_capacity(waits.len());
    for wait in waits.drain(..) {
        let pattern = stored_pattern(&wait.path)?;
        if freed.iter().any(|f| f.overlaps(&pattern)) {
            keep.push(wait);
        }
    }
    Ok(keep)
}

fn wake_unblocked(tx: &Transaction, freed: &[PathPattern]) -> Result<Vec<WokenWait>, ChangeError> {
    if freed.is_empty() {
        return Ok(Vec::new());
    }
    let held = live_claims(tx)?;
    let mut woken = Vec::new();
    for wait in wait_candidates(tx, freed)? {
        let pattern = stored_pattern(&wait.path)?;
        let still_blocked = held
            .iter()
            .any(|claim| claim.change != wait.change && claim.pattern.overlaps(&pattern));
        if still_blocked {
            continue;
        }
        tx.execute(
            "DELETE FROM path_claims WHERE change_id = ?1 AND pattern = ?2 AND kind = 'wait'",
            params![wait.change.to_string(), wait.path],
        )?;
        woken.push(WokenWait {
            change: wait.change,
            owner: wait.owner,
            path: wait.path,
        });
    }
    Ok(woken)
}

/// Every live claim on another change that overlaps one of `patterns`.
fn holders(
    tx: &Transaction,
    claimant: ChangeId,
    patterns: &[PathPattern],
) -> Result<Vec<ClaimHolder>, ChangeError> {
    let sql = format!("{SELECT} WHERE pc.kind = 'claim' AND {LIVE} ORDER BY pc.created_at");
    let records = rows(tx, &sql, [])?;
    let held = to_held(&records)?;
    Ok(blockers(claimant, patterns, &held)
        .into_iter()
        .map(|blocker| ClaimHolder {
            requested: patterns[blocker.requested].to_string(),
            held: records[blocker.held].clone(),
        })
        .collect())
}

fn live_claims(tx: &Transaction) -> Result<Vec<HeldClaim>, ChangeError> {
    let sql = format!("{SELECT} WHERE pc.kind = 'claim' AND {LIVE}");
    to_held(&rows(tx, &sql, [])?)
}

fn to_held(records: &[PathClaim]) -> Result<Vec<HeldClaim>, ChangeError> {
    records
        .iter()
        .map(|record| {
            Ok(HeldClaim {
                change: record.change,
                pattern: stored_pattern(&record.path)?,
            })
        })
        .collect()
}

/// Resolves a change that may take new claims: open or ready.
fn open_change(tx: &Transaction, text: &str) -> Result<ChangeId, ChangeError> {
    let record = changes::get(tx, changes::resolve(tx, text)?)?;
    if record.change.state.is_closed() {
        return Err(rosen_core::Error::ChangeClosed(record.change.id).into());
    }
    Ok(record.change.id)
}

fn insert(
    tx: &Transaction,
    id: ChangeId,
    pattern: &PathPattern,
    kind: ClaimKind,
    now: i64,
) -> Result<(), ChangeError> {
    // OR IGNORE keeps the first record's time when a path is repeated.
    tx.execute(
        "INSERT OR IGNORE INTO path_claims (change_id, pattern, kind, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![id.to_string(), pattern.as_str(), kind_text(kind), now],
    )?;
    Ok(())
}

fn rows<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    args: P,
) -> Result<Vec<PathClaim>, ChangeError> {
    let mut statement = conn.prepare_cached(sql)?;
    let raw = statement.query_map(args, raw_row)?;
    let mut out = Vec::new();
    for row in raw {
        out.push(parse_row(row?)?);
    }
    Ok(out)
}

type RawRow = (String, String, String, String, i64);

fn raw_row(row: &Row) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn parse_row((change, owner, path, kind, since): RawRow) -> Result<PathClaim, ChangeError> {
    let corrupt = |what: &str| ChangeError::Corrupt(format!("path claim {what}"));
    Ok(PathClaim {
        change: change.parse().map_err(|_| corrupt("change id"))?,
        owner: rosen_core::AgentId::new(&owner).map_err(|_| corrupt("owner"))?,
        kind: match kind.as_str() {
            "claim" => ClaimKind::Claim,
            "share" => ClaimKind::Share,
            "wait" => ClaimKind::Wait,
            _ => return Err(corrupt("kind")),
        },
        path,
        since,
    })
}

fn kind_text(kind: ClaimKind) -> &'static str {
    match kind {
        ClaimKind::Claim => "claim",
        ClaimKind::Share => "share",
        ClaimKind::Wait => "wait",
    }
}

fn stored_pattern(text: &str) -> Result<PathPattern, ChangeError> {
    PathPattern::new(text).map_err(|_| ChangeError::Corrupt(format!("path claim {text:?}")))
}

fn parse(paths: &[String]) -> Result<Vec<PathPattern>, ChangeError> {
    let mut patterns = Vec::with_capacity(paths.len());
    for path in paths {
        let pattern = PathPattern::new(path)?;
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    Ok(patterns)
}

fn texts(patterns: &[PathPattern]) -> Vec<String> {
    patterns.iter().map(ToString::to_string).collect()
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}
