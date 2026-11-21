//! Publish the complete rewrite in one SQLite transaction. Old objects and
//! author snapshots remain available for audit and workspace reconciliation.
use anyhow::{bail, Result};
use rosen_core::ObjectId;
use rosen_protocol::{EventKind, HistoryRewrite};
use rosen_store::LooseObjectStore;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::json;

use super::objects::Rewrite;
use crate::events::{self, Staged};
use crate::ServerState;

pub fn apply(
    state: &ServerState,
    answer: &mut HistoryRewrite,
    rewrite: &mut Rewrite,
) -> Result<()> {
    let mut conn = crate::metadata::open(&state.config.metadata_path())?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let base: String = tx.query_row(
        "SELECT target FROM base_refs WHERE name=?1",
        [&answer.base_ref],
        |r| r.get(0),
    )?;
    if base != answer.old_base.to_hex() {
        bail!("base moved; retry the history repair");
    }
    tx.execute_batch("CREATE TEMP TABLE rewrite_map(old TEXT PRIMARY KEY, new TEXT NOT NULL);")?;
    for (old, new) in &rewrite.commits {
        tx.execute(
            "INSERT INTO rewrite_map VALUES(?1,?2)",
            params![old.to_hex(), new.to_hex()],
        )?;
    }
    answer.updated_changes = update_changes(&tx, &state.config.objects_dir(), rewrite)?;
    for (table, column) in [
        ("base_refs", "target"),
        ("base_history", "commit_id"),
        ("stack_lands", "commit_id"),
        ("stack_lands", "tip"),
        ("auto_reverts", "culprit"),
        ("auto_reverts", "commit_id"),
    ] {
        remap(&tx, table, column)?;
    }
    invalidate_checks(&tx)?;
    events::stage(
        &tx,
        &Staged {
            kind: EventKind::Mirror,
            change: None,
            detail: json!({"status": "rewritten", "rewrite": answer}),
            concern: Default::default(),
        },
    )?;
    tx.commit()?;
    Ok(())
}

fn remap(conn: &Connection, table: &str, column: &str) -> Result<()> {
    if !exists(conn, table)? {
        return Ok(());
    }
    conn.execute(
        &format!(
            "UPDATE {table} SET {column}=(SELECT new FROM rewrite_map WHERE old={table}.{column})
        WHERE {column} IN (SELECT old FROM rewrite_map)"
        ),
        [],
    )?;
    Ok(())
}

fn update_changes(
    conn: &Connection,
    objects: &std::path::Path,
    rewrite: &mut Rewrite,
) -> Result<usize> {
    let store = LooseObjectStore::open(objects)?;
    let mut query =
        conn.prepare("SELECT id,head,parent_base FROM changes WHERE state IN ('open','ready')")?;
    let rows = query
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut updated = 0;
    for (id, head, parent) in rows {
        updated += usize::from(update_change(
            conn,
            &store,
            rewrite,
            &id,
            &head,
            parent.as_deref(),
        )?);
    }
    Ok(updated)
}

fn update_change(
    conn: &Connection,
    store: &LooseObjectStore,
    rewrite: &mut Rewrite,
    id: &str,
    head: &str,
    parent: Option<&str>,
) -> Result<bool> {
    // Keeping an old head while removing a path from its parent would
    // reintroduce those bytes as an apparent authored addition at next land.
    let old_head = ObjectId::from_hex(head)?;
    let new_head = rewrite.tree(store, old_head)?;
    let new_parent = mapped_parent(conn, parent)?;
    let (basis, new_basis) = stack_basis(conn, store, rewrite, id)?;
    let moved = new_head != old_head
        || new_parent.is_some()
        || new_basis.is_some_and(|b| Some(b.to_hex()) != basis);
    if !moved {
        return Ok(false);
    }
    conn.execute(
        "UPDATE changes SET head=?2,parent_base=COALESCE(?3,parent_base),
        updated_at=MAX(updated_at+1,CAST(strftime('%s','now') AS INTEGER)*1000) WHERE id=?1",
        params![id, new_head.to_hex(), new_parent],
    )?;
    if let Some(tree) = new_basis {
        conn.execute(
            "UPDATE stack_bases SET tree=?2 WHERE change_id=?1",
            params![id, tree.to_hex()],
        )?;
    }
    conn.execute(
        "UPDATE review_approvals SET valid=0 WHERE change_id=?1",
        [id],
    )?;
    events::stage(
        conn,
        &Staged::about(
            EventKind::Rebased,
            id,
            json!({"head": new_head, "reason": "mirror_history_rewrite"}),
        ),
    )?;
    Ok(true)
}

fn mapped_parent(conn: &Connection, parent: Option<&str>) -> Result<Option<String>> {
    Ok(parent
        .map(|old| {
            conn.query_row("SELECT new FROM rewrite_map WHERE old=?1", [old], |r| {
                r.get(0)
            })
            .optional()
        })
        .transpose()?
        .flatten())
}

fn stack_basis(
    conn: &Connection,
    store: &LooseObjectStore,
    rewrite: &mut Rewrite,
    id: &str,
) -> Result<(Option<String>, Option<ObjectId>)> {
    let basis: Option<String> = conn
        .query_row(
            "SELECT tree FROM stack_bases WHERE change_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let new = basis
        .as_ref()
        .map(|s| rewrite.tree(store, ObjectId::from_hex(s)?))
        .transpose()?;
    Ok((basis, new))
}

/// A result about the old tree cannot certify its replacement. Old receipts
/// and failure evidence remain auditable, but new-base jobs must run again.
fn invalidate_checks(conn: &Connection) -> Result<()> {
    for table in ["post_land_green", "post_land_build_green"] {
        if !exists(conn, table)? {
            continue;
        }
        conn.execute(
            &format!("DELETE FROM {table} WHERE commit_id IN (SELECT old FROM rewrite_map)"),
            [],
        )?;
    }
    if exists(conn, "post_land_delivery")? {
        conn.execute(
            "DELETE FROM post_land_delivery WHERE commit_id IN (SELECT old FROM rewrite_map)",
            [],
        )?;
    }
    crate::post_land::deploy::invalidate_rewritten(conn)?;
    if exists(conn, "bisections")? {
        conn.execute(
            "UPDATE bisections SET phase='flaky',step=step+1,
                output=output || char(10) || 'Superseded by mirror history rewrite; verify replacement commits again.'
             WHERE EXISTS (SELECT 1 FROM rewrite_map WHERE instr(bisections.commits,rewrite_map.old)>0)",
            [],
        )?;
    }
    // Closed changes' original snapshots and historical events are retained;
    // their IDs describe what was reviewed, rather than signing new content.
    Ok(())
}

fn exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )
}
