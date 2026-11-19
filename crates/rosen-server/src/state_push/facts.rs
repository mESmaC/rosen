//! The parts of a change document read straight from the metadata and
//! object stores, on the blocking pool: the record, its stored conflicts
//! with when and onto what they were stored, its latest receipt, its named
//! snapshots and the commit it landed as.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rosen_core::conflict::Conflict;
use rosen_core::{ChangeId, ObjectId, SnapshotKind};
use rosen_protocol::ChangeRecord;
use rosen_store::{LooseObjectStore, ObjectStore};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::landing::Ledger;
use crate::ServerConfig;

/// How many named snapshots a document lists, newest first.
const NAMED_SNAPSHOTS: usize = 20;
/// The most bytes of each conflict side a document carries.
const CONFLICT_SIDE: usize = 64 * 1024;

/// Where the readers open their stores.
#[derive(Clone)]
pub struct Stores {
    metadata: PathBuf,
    objects: PathBuf,
}

impl Stores {
    pub fn new(config: &ServerConfig) -> Self {
        Self {
            metadata: config.metadata_path(),
            objects: config.objects_dir(),
        }
    }

    pub fn metadata(&self) -> &std::path::Path {
        &self.metadata
    }
}

/// One change's stored facts.
pub struct Facts {
    pub record: ChangeRecord,
    /// The commit it landed as, while it stays landed.
    pub landed: Option<ObjectId>,
    /// Each stored conflict, as a document item.
    pub conflicts: Vec<Value>,
    pub receipt: Option<Value>,
    pub named: Vec<Value>,
    pub title: Option<String>,
}

/// Reads one change's facts. Call it on the blocking pool.
pub fn read(stores: &Stores, id: ChangeId) -> Result<Facts, String> {
    let conn = crate::metadata::open(&stores.metadata).map_err(|e| e.to_string())?;
    let record = crate::changes::get(&conn, id).map_err(|e| e.to_string())?;
    let ledger = Ledger::open(&stores.metadata).map_err(|e| e.to_string())?;
    let landed = ledger.landing_of(&id).map_err(|e| e.to_string())?;
    let base = ledger
        .project_base()
        .map_err(|e| e.to_string())?
        .map(|(_, commit)| commit);
    let objects = LooseObjectStore::open(&stores.objects).map_err(|e| e.to_string())?;
    let title = record
        .change
        .snapshots
        .iter()
        .rev()
        .find_map(|s| match &s.kind {
            SnapshotKind::Named(message) => Some(first_line(message)),
            SnapshotKind::Automatic => None,
        });
    let closed = record.change.state.is_closed();
    Ok(Facts {
        conflicts: if closed {
            Vec::new()
        } else {
            conflicts(&conn, &id).map_err(|e| e.to_string())?
        },
        receipt: latest_receipt(&conn, &id, base).map_err(|e| e.to_string())?,
        named: named_snapshots(&objects, &record),
        title,
        landed,
        record,
    })
}

/// Stored conflicts, each side clipped, with the time and base of the
/// `conflict` event that last stored its path.
fn conflicts(conn: &Connection, id: &ChangeId) -> Result<Vec<Value>, String> {
    let set = crate::conflicts::load(conn, &id.to_string()).map_err(|e| e.to_string())?;
    let stored = stored_by_path(conn, id).map_err(|e| e.to_string())?;
    Ok(set
        .all()
        .iter()
        .map(|conflict| conflict_item(id, conflict, stored.get(&conflict.path)))
        .collect())
}

fn conflict_item(id: &ChangeId, conflict: &Conflict, stored: Option<&Stored>) -> Value {
    let side = |bytes: &[u8]| {
        let clipped = &bytes[..bytes.len().min(CONFLICT_SIDE)];
        json!({
            "text": String::from_utf8_lossy(clipped),
            "truncated": clipped.len() < bytes.len(),
        })
    };
    json!({
        "change": id,
        "id": conflict.id.to_string(),
        "path": conflict.path,
        "start_line": conflict.start_line,
        "state": if conflict.is_open() { "open" } else { "resolved" },
        "base": side(&conflict.base),
        "ours": side(&conflict.ours),
        "theirs": side(&conflict.theirs),
        "onto": stored.and_then(|s| s.onto.clone()),
        "stored_at": stored.map(|s| s.at),
    })
}

/// When a path's conflicts were last stored, and onto which base.
struct Stored {
    at: i64,
    onto: Option<String>,
}

/// Replays the change's `rebased` and `conflict` events. A conflict event
/// names the base it merged when the land stored it; one a rebase or a
/// snapshot stored follows the `rebased` event that names it.
fn stored_by_path(conn: &Connection, id: &ChangeId) -> rusqlite::Result<BTreeMap<String, Stored>> {
    let mut statement = conn.prepare_cached(
        "SELECT kind, detail, created_at FROM events
         WHERE change_id = ?1 AND kind IN ('rebased', 'conflict') ORDER BY seq",
    )?;
    let rows = statement.query_map([id.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut onto: Option<String> = None;
    let mut by_path = BTreeMap::new();
    for row in rows {
        let (kind, detail, at) = row?;
        let detail: Value = serde_json::from_str(&detail).unwrap_or(Value::Null);
        let named = detail
            .get("onto")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if kind == "rebased" {
            onto = named;
            continue;
        }
        let onto = named.or_else(|| onto.clone());
        let paths = detail.get("paths").and_then(Value::as_array);
        for path in paths.into_iter().flatten().filter_map(Value::as_str) {
            let stored = Stored {
                at,
                onto: onto.clone(),
            };
            by_path.insert(path.to_owned(), stored);
        }
    }
    Ok(by_path)
}

/// The newest stored receipt, without its signature, and whether the base
/// still points at the commit it checked.
fn latest_receipt(
    conn: &Connection,
    id: &ChangeId,
    base: Option<ObjectId>,
) -> rusqlite::Result<Option<Value>> {
    conn.query_row(
        "SELECT head, base, check_set, submitter, machine FROM receipts
         WHERE change_id = ?1 ORDER BY id DESC LIMIT 1",
        params![id.to_string()],
        |row| {
            let checked: String = row.get(1)?;
            let current = base.is_some_and(|base| base.to_hex() == checked);
            Ok(json!({
                "head": row.get::<_, String>(0)?,
                "base": checked,
                "check_set": row.get::<_, String>(2)?,
                "submitter": row.get::<_, String>(3)?,
                "machine": row.get::<_, String>(4)?,
                "base_current": current,
            }))
        },
    )
    .optional()
}

/// The newest named snapshots: commit, message title and commit time.
fn named_snapshots(objects: &LooseObjectStore, record: &ChangeRecord) -> Vec<Value> {
    let named = record
        .change
        .snapshots
        .iter()
        .rev()
        .filter_map(|s| match &s.kind {
            SnapshotKind::Named(message) => Some((s.commit, message)),
            SnapshotKind::Automatic => None,
        });
    named
        .take(NAMED_SNAPSHOTS)
        .map(|(commit, message)| {
            // Unix milliseconds, like every other time the feed sends.
            let at = objects
                .read_commit(&commit)
                .ok()
                .map(|c| c.committer.seconds.saturating_mul(1000));
            json!({ "commit": commit, "title": first_line(message), "at": at })
        })
        .collect()
}

fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or("").trim().to_owned()
}
