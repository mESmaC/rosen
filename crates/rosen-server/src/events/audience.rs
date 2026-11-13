//! Who an event about a change concerns (SPEC R9): the change's owner, its
//! reviewers, the owners of changes stacked on it, and the holders of
//! overlapping changes or claims.
//!
//! This module reads what the metadata tables record: owners, reviews,
//! stacks and path claims. Owners of changes whose edits overlap come from
//! the §2 overlap rule, which the server installs with
//! [`super::EventLog::set_overlap_owners`], since edits live in trees.
//!
//! The audience is fixed when the event is published, from the state the
//! publishing operation just committed, so a later release or land never
//! changes who an already-published event reaches.

use rosen_core::{AgentId, PathPattern};
use rusqlite::{params, Connection};
use std::collections::BTreeSet;

/// What the publisher knows about an event's audience beyond the change
/// itself. Most events need neither field.
#[derive(Debug, Clone, Default)]
pub struct Concern {
    /// Agents the change and review records do not name.
    pub agents: Vec<AgentId>,
    /// Paths the event is about that the change may no longer hold, such as
    /// the paths a release just dropped. Holders of overlapping claims on
    /// them are concerned too.
    pub paths: Vec<String>,
}

/// Every agent the event about `change` concerns.
pub fn concerned(
    conn: &Connection,
    change: &str,
    concern: &Concern,
) -> rusqlite::Result<BTreeSet<String>> {
    let mut agents: BTreeSet<String> = concern.agents.iter().map(|a| a.to_string()).collect();
    agents.extend(owner(conn, change)?);
    agents.extend(reviewers(conn, change)?);
    agents.extend(stacked_above(conn, change)?);
    let mut patterns = held_patterns(conn, change)?;
    patterns.extend(
        concern
            .paths
            .iter()
            .filter_map(|p| PathPattern::new(p).ok()),
    );
    agents.extend(overlapping_holders(conn, change, &patterns)?);
    Ok(agents)
}

fn owner(conn: &Connection, change: &str) -> rusqlite::Result<Option<String>> {
    let mut stmt = conn.prepare_cached("SELECT owner FROM changes WHERE id = ?1")?;
    let mut rows = stmt.query_map([change], |row| row.get(0))?;
    rows.next().transpose()
}

/// Everyone who has commented on or resolved a review thread on the change
/// (R7): the change model records no reviewer list, so taking part in its
/// review is what makes an agent a reviewer.
fn reviewers(conn: &Connection, change: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT rc.author FROM review_comments rc
             JOIN review_threads rt ON rt.id = rc.thread_id WHERE rt.change_id = ?1
         UNION
         SELECT resolved_by FROM review_threads
             WHERE change_id = ?1 AND resolved_by IS NOT NULL",
    )?;
    let rows = stmt.query_map([change], |row| row.get(0))?;
    rows.collect()
}

/// Owners of live changes anywhere above `change` in its stack: a land or
/// rebase of a lower change moves every change above it (D19).
fn stacked_above(conn: &Connection, change: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE above(id) AS (
             SELECT id FROM changes WHERE parent_change = ?1
             UNION SELECT c.id FROM changes c JOIN above a ON c.parent_change = a.id)
         SELECT owner FROM changes
         WHERE id IN (SELECT id FROM above) AND state IN ('open', 'ready')",
    )?;
    let rows = stmt.query_map([change], |row| row.get(0))?;
    rows.collect()
}

/// Every claim, share and wait the change still records, live or not: an
/// event about a change that just landed still concerns its old neighbours.
fn held_patterns(conn: &Connection, change: &str) -> rusqlite::Result<Vec<PathPattern>> {
    let mut stmt = conn.prepare_cached("SELECT pattern FROM path_claims WHERE change_id = ?1")?;
    let rows = stmt.query_map([change], |row| row.get::<_, String>(0))?;
    let texts = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(texts
        .iter()
        .filter_map(|t| PathPattern::new(t).ok())
        .collect())
}

/// Owners of other live changes whose claims, shares or waits overlap any
/// of `patterns`.
fn overlapping_holders(
    conn: &Connection,
    change: &str,
    patterns: &[PathPattern],
) -> rusqlite::Result<Vec<String>> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare_cached(
        "SELECT c.owner, pc.pattern FROM path_claims pc JOIN changes c ON c.id = pc.change_id
         WHERE pc.change_id <> ?1 AND c.state IN ('open', 'ready')",
    )?;
    let rows = stmt.query_map(params![change], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut holders = Vec::new();
    for row in rows {
        let (owner, text) = row?;
        let Ok(theirs) = PathPattern::new(&text) else {
            continue;
        };
        if patterns.iter().any(|ours| ours.overlaps(&theirs)) {
            holders.push(owner);
        }
    }
    Ok(holders)
}
