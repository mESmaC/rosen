use super::error::ChangeError;
use rosen_core::{AgentId, Change, ChangeId, ChangeParent, ObjectId, Snapshot, SnapshotKind};
use rosen_protocol::{ChangeFilter, ChangeRecord};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where a new change starts. A change on the base starts from the base
/// commit's tree; a stacked change starts from its parent change's head.
#[derive(Debug, Clone, Copy)]
pub enum NewParent {
    Base { commit: ObjectId, tree: ObjectId },
    Change(ChangeId),
}

const ID_LETTERS: &str = "klmnopqrstuvwxyz";
const COLUMNS: &str = "id, owner, parent_base, parent_change, state, head, created_at, updated_at";

/// Creates an open change with a freshly minted ID, in one transaction.
pub fn create(
    conn: &mut Connection,
    owner: &AgentId,
    parent: NewParent,
) -> Result<ChangeRecord, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (parent, start_tree) = match parent {
        NewParent::Base { commit, tree } => (ChangeParent::Base(commit), tree),
        NewParent::Change(below) => {
            let below = load(&tx, below)?.change;
            if below.state.is_closed() {
                return Err(ChangeError::ParentClosed {
                    parent: below.id,
                    state: below.state,
                });
            }
            (ChangeParent::Change(below.id), below.head)
        }
    };
    let now = now_millis();
    let id = insert_with_fresh_id(&tx, owner, parent, start_tree, now)?;
    tx.commit()?;
    Ok(ChangeRecord {
        change: Change::new(id, owner.clone(), parent, start_tree),
        created_at: now,
        updated_at: now,
    })
}

/// 128 random bits make a collision practically impossible, but retrying on
/// the primary-key conflict costs nothing and turns "practically" into "never".
fn insert_with_fresh_id(
    conn: &Connection,
    owner: &AgentId,
    parent: ChangeParent,
    head: ObjectId,
    now: i64,
) -> Result<ChangeId, ChangeError> {
    let (base, below) = parent_columns(parent);
    loop {
        let id = mint_id()?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO changes
                 (id, owner, parent_base, parent_change, state, head, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6, ?6)",
            params![
                id.to_string(),
                owner.as_str(),
                base,
                below,
                head.to_hex(),
                now
            ],
        )?;
        if inserted == 1 {
            return Ok(id);
        }
    }
}

fn mint_id() -> Result<ChangeId, ChangeError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|err| ChangeError::Randomness(err.to_string()))?;
    Ok(ChangeId::from_bytes(bytes))
}

pub fn get(conn: &Connection, id: ChangeId) -> Result<ChangeRecord, ChangeError> {
    load(conn, id)
}

/// Resolves a full ID or a unique prefix of one, so agents can type short IDs.
pub fn resolve(conn: &Connection, text: &str) -> Result<ChangeId, ChangeError> {
    let text = text.trim();
    let well_formed = !text.is_empty()
        && text.len() <= ChangeId::TEXT_LEN
        && text.chars().all(|c| ID_LETTERS.contains(c));
    if !well_formed {
        return Err(ChangeError::InvalidId(text.to_string()));
    }
    // '{' is the character after 'z', so [prefix, prefix + '{') holds exactly
    // the IDs that start with the prefix.
    let upper = format!("{text}{{");
    let mut stmt =
        conn.prepare_cached("SELECT id FROM changes WHERE id >= ?1 AND id < ?2 LIMIT 2")?;
    let found: Vec<String> = stmt
        .query_map(params![text, upper], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    match found.as_slice() {
        [one] => parse_column(one, "change id"),
        [] => Err(ChangeError::NotFound(text.to_string())),
        _ => Err(ChangeError::Ambiguous(text.to_string())),
    }
}

/// Lists changes oldest first, optionally narrowed by owner and state.
pub fn list(conn: &Connection, filter: &ChangeFilter) -> Result<Vec<ChangeRecord>, ChangeError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM changes
         WHERE (?1 IS NULL OR owner = ?1) AND (?2 IS NULL OR state = ?2)
         ORDER BY created_at, id"
    );
    let owner = filter.owner.as_ref().map(AgentId::as_str);
    let state = filter.state.map(|s| s.as_str());
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows: Vec<StoredRow> = stmt
        .query_map(params![owner, state], StoredRow::read)?
        .collect::<Result<_, _>>()?;
    rows.into_iter().map(|row| row.into_record(conn)).collect()
}

/// Applies one rule from `rosen_core::Change` (mark ready, land, reopen…) and
/// stores the result in the same transaction. The rule decides; this only
/// persists, so the store can never hold a state the model forbids.
pub fn update(
    conn: &mut Connection,
    id: ChangeId,
    apply: impl FnOnce(&mut Change) -> rosen_core::Result<()>,
) -> Result<ChangeRecord, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let record = apply_and_save(&tx, id, apply)?;
    tx.commit()?;
    Ok(record)
}

/// The body of [`update`], for callers that must check more inside the same
/// write transaction first.
fn apply_and_save(
    tx: &Connection,
    id: ChangeId,
    apply: impl FnOnce(&mut Change) -> rosen_core::Result<()>,
) -> Result<ChangeRecord, ChangeError> {
    let mut record = load(tx, id)?;
    let stored_snapshots = record.change.snapshots.len();
    apply(&mut record.change)?;
    record.updated_at = now_millis();
    save(tx, &record, stored_snapshots)?;
    Ok(record)
}

pub fn abandon(conn: &mut Connection, id: ChangeId) -> Result<ChangeRecord, ChangeError> {
    update(conn, id, Change::abandon)
}

/// Appends a snapshot or named commit; earlier ones are never touched (R2).
pub fn record_snapshot(
    conn: &mut Connection,
    id: ChangeId,
    snapshot: Snapshot,
) -> Result<ChangeRecord, ChangeError> {
    update(conn, id, |change| change.record_snapshot(snapshot))
}

/// Records a server-side rebase (R8, D19): a new parent and the merged head.
/// The ID and the snapshots stay exactly as they were.
///
/// The cycle check runs inside the write transaction: checked outside it, two
/// opposing rebases on separate connections could both pass and store a loop.
pub fn rebase(
    conn: &mut Connection,
    id: ChangeId,
    parent: ChangeParent,
    merged_head: ObjectId,
) -> Result<ChangeRecord, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let ChangeParent::Change(below) = parent {
        ensure_not_above(&tx, id, below)?;
    }
    let record = apply_and_save(&tx, id, |change| change.rebase(parent, merged_head))?;
    tx.commit()?;
    Ok(record)
}

/// Walks down from `below` to the base; meeting `id` on the way means stacking
/// `id` on `below` would close a loop. The visited set makes the walk end even
/// if a damaged store already holds a loop.
fn ensure_not_above(conn: &Connection, id: ChangeId, below: ChangeId) -> Result<(), ChangeError> {
    let mut visited = std::collections::HashSet::new();
    let mut cursor = Some(below);
    while let Some(current) = cursor {
        if current == id || !visited.insert(current) {
            return Err(rosen_core::Error::StackCycle(id).into());
        }
        cursor = match load(conn, current)?.change.parent {
            ChangeParent::Change(next) => Some(next),
            ChangeParent::Base(_) => None,
        };
    }
    Ok(())
}

/// Writes the row and appends any snapshots past the `stored` already held.
fn save(conn: &Connection, record: &ChangeRecord, stored: usize) -> Result<(), ChangeError> {
    let change = &record.change;
    let (base, below) = parent_columns(change.parent);
    if change.snapshots.len() > stored {
        // Only an authored head change voids review; server rebases append no snapshots.
        conn.execute(
            "UPDATE review_approvals SET valid = 0 WHERE change_id = ?1
             AND EXISTS (SELECT 1 FROM changes WHERE id = ?1 AND head <> ?2)",
            params![change.id.to_string(), change.head.to_hex()],
        )?;
    }
    conn.execute(
        "UPDATE changes SET parent_base = ?1, parent_change = ?2, state = ?3, head = ?4,
                            updated_at = ?5
         WHERE id = ?6",
        params![
            base,
            below,
            change.state.as_str(),
            change.head.to_hex(),
            record.updated_at,
            change.id.to_string()
        ],
    )?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO change_snapshots (change_id, seq, commit_id, tree_id, message)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (seq, snapshot) in change.snapshots.iter().enumerate().skip(stored) {
        let message = match &snapshot.kind {
            SnapshotKind::Automatic => None,
            SnapshotKind::Named(message) => Some(message.as_str()),
        };
        insert.execute(params![
            change.id.to_string(),
            seq as i64 + 1,
            snapshot.commit.to_hex(),
            snapshot.tree.to_hex(),
            message
        ])?;
    }
    Ok(())
}

fn load(conn: &Connection, id: ChangeId) -> Result<ChangeRecord, ChangeError> {
    let sql = format!("SELECT {COLUMNS} FROM changes WHERE id = ?1");
    conn.prepare_cached(&sql)?
        .query_row(params![id.to_string()], StoredRow::read)
        .optional()?
        .ok_or_else(|| ChangeError::NotFound(id.to_string()))?
        .into_record(conn)
}

fn load_snapshots(conn: &Connection, id: &str) -> Result<Vec<Snapshot>, ChangeError> {
    let mut stmt = conn.prepare_cached(
        "SELECT commit_id, tree_id, message FROM change_snapshots
         WHERE change_id = ?1 ORDER BY seq",
    )?;
    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map(params![id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(|(commit, tree, message)| {
            Ok(Snapshot {
                commit: parse_column(&commit, "snapshot commit")?,
                tree: parse_column(&tree, "snapshot tree")?,
                kind: message.map_or(SnapshotKind::Automatic, SnapshotKind::Named),
            })
        })
        .collect()
}

/// The open conflicts are owned by `crate::conflicts`; a change only lists
/// their IDs, so `can_land` sees them.
fn load_open_conflicts(
    conn: &Connection,
    id: &str,
) -> Result<Vec<rosen_core::ConflictId>, ChangeError> {
    let set = crate::conflicts::load(conn, id).map_err(|err| match err {
        crate::conflicts::ConflictStoreError::Sql(sql) => ChangeError::Store(sql),
        other => ChangeError::Corrupt(other.to_string()),
    })?;
    Ok(set.open().map(|conflict| conflict.id).collect())
}

/// One `changes` row as text, before parsing.
struct StoredRow {
    id: String,
    owner: String,
    parent_base: Option<String>,
    parent_change: Option<String>,
    state: String,
    head: String,
    created_at: i64,
    updated_at: i64,
}

impl StoredRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(StoredRow {
            id: row.get(0)?,
            owner: row.get(1)?,
            parent_base: row.get(2)?,
            parent_change: row.get(3)?,
            state: row.get(4)?,
            head: row.get(5)?,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
        })
    }

    fn into_record(self, conn: &Connection) -> Result<ChangeRecord, ChangeError> {
        let parent = match (&self.parent_base, &self.parent_change) {
            (Some(base), None) => ChangeParent::Base(parse_column(base, "parent")?),
            (None, Some(below)) => ChangeParent::Change(parse_column(below, "parent")?),
            _ => {
                return Err(ChangeError::Corrupt(format!(
                    "parent of change {}",
                    self.id
                )))
            }
        };
        let change = Change {
            id: parse_column(&self.id, "change id")?,
            owner: AgentId::new(self.owner.as_str())
                .map_err(|_| ChangeError::Corrupt(format!("owner {:?}", self.owner)))?,
            parent,
            snapshots: load_snapshots(conn, &self.id)?,
            head: parse_column(&self.head, "head")?,
            state: parse_column(&self.state, "state")?,
            conflicts: load_open_conflicts(conn, &self.id)?,
        };
        Ok(ChangeRecord {
            change,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

fn parse_column<T: std::str::FromStr>(text: &str, what: &str) -> Result<T, ChangeError> {
    text.parse()
        .map_err(|_| ChangeError::Corrupt(format!("{what} {text:?}")))
}

fn parent_columns(parent: ChangeParent) -> (Option<String>, Option<String>) {
    match parent {
        ChangeParent::Base(commit) => (Some(commit.to_hex()), None),
        ChangeParent::Change(id) => (None, Some(id.to_string())),
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Commit a computed rebase only while both inputs still match. The write
/// transaction prevents a concurrent snapshot or land from losing edits.
pub fn rebase_checked(
    conn: &mut Connection,
    expected: &Change,
    onto: ObjectId,
    tree: ObjectId,
) -> Result<ChangeRecord, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = load(&tx, expected.id)?;
    let base: String = tx.query_row("SELECT target FROM base_refs LIMIT 1", [], |r| r.get(0))?;
    if current.change != *expected || base != onto.to_hex() {
        return Err(ChangeError::Base(
            "rebase inputs moved; retry the rebase".into(),
        ));
    }
    let record = apply_and_save(&tx, expected.id, |c| {
        c.rebase(ChangeParent::Base(onto), tree)
    })?;
    tx.commit()?;
    Ok(record)
}

/// Preserve a pending rebase while appending the author's original snapshot.
/// Both updates are one transaction; a concurrent rebase forces a retry.
pub fn record_snapshot_merged(
    conn: &mut Connection,
    expected: &Change,
    snapshot: Snapshot,
    head: ObjectId,
) -> Result<ChangeRecord, ChangeError> {
    update(conn, expected.id, |current| {
        if current != expected {
            return Err(rosen_core::Error::InvalidPath(
                "snapshot inputs moved; retry".into(),
            ));
        }
        current.record_snapshot(snapshot)?;
        current.head = head;
        Ok(())
    })
}
