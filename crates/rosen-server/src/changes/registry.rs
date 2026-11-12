use super::error::ChangeError;
use rosen_core::change::{AgentId, ChangeId, ChangeParent, ChangeState};
use rosen_core::object::ObjectId;
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

/// A change as the metadata store holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangeRecord {
    pub id: ChangeId,
    pub owner: AgentId,
    pub parent: ChangeParent,
    pub state: ChangeState,
    pub head_tree: ObjectId,
    /// Snapshots and named commits, oldest first. Append-only (R2).
    pub commits: Vec<ObjectId>,
    /// Unix milliseconds.
    pub created_at: i64,
    pub updated_at: i64,
}

/// Where a new change starts. A change on the base takes the base commit's
/// tree; a stacked change takes its parent change's current head.
#[derive(Debug, Clone, Copy)]
pub enum NewParent {
    Base { commit: ObjectId, tree: ObjectId },
    Change(ChangeId),
}

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub owner: Option<AgentId>,
    pub state: Option<ChangeState>,
}

const ID_LETTERS: &str = "klmnopqrstuvwxyz";
const COLUMNS: &str =
    "id, owner, parent_base, parent_change, state, head_tree, created_at, updated_at";

/// Creates a change with a freshly minted ID, in one transaction.
pub fn create(
    conn: &mut Connection,
    owner: &AgentId,
    parent: NewParent,
) -> Result<ChangeRecord, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (parent, head_tree) = match parent {
        NewParent::Base { commit, tree } => (ChangeParent::Base(commit), tree),
        NewParent::Change(id) => {
            let below = load(&tx, id)?;
            ensure_live(&below)?;
            (ChangeParent::Change(id), below.head_tree)
        }
    };
    let now = now_millis();
    let id = insert_with_fresh_id(&tx, owner, parent, head_tree, now)?;
    tx.commit()?;
    Ok(ChangeRecord {
        id,
        owner: owner.clone(),
        parent,
        state: ChangeState::Open,
        head_tree,
        commits: Vec::new(),
        created_at: now,
        updated_at: now,
    })
}

/// 128 random bits make a collision practically impossible, but a retry on the
/// primary-key conflict costs nothing and turns "practically" into "never".
fn insert_with_fresh_id(
    conn: &Connection,
    owner: &AgentId,
    parent: ChangeParent,
    head_tree: ObjectId,
    now: i64,
) -> Result<ChangeId, ChangeError> {
    let (base, below) = parent_columns(parent);
    loop {
        let id = mint_id()?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO changes
                 (id, owner, parent_base, parent_change, state, head_tree, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6, ?6)",
            params![
                id.to_string(),
                owner.as_str(),
                base,
                below,
                head_tree.to_hex(),
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
    if text.is_empty() || text.len() > 32 || !text.chars().all(|c| ID_LETTERS.contains(c)) {
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
        [one] => one.parse().map_err(|_| ChangeError::InvalidId(one.clone())),
        [] => Err(ChangeError::NotFound(text.to_string())),
        _ => Err(ChangeError::Ambiguous(text.to_string())),
    }
}

pub fn list(conn: &Connection, filter: &ListFilter) -> Result<Vec<ChangeRecord>, ChangeError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM changes
         WHERE (?1 IS NULL OR owner = ?1) AND (?2 IS NULL OR state = ?2)
         ORDER BY created_at, id"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let owner = filter.owner.as_ref().map(AgentId::as_str);
    let state = filter.state.map(|s| s.as_str());
    let mut records = stmt
        .query_map(params![owner, state], read_row)?
        .collect::<Result<Result<Vec<_>, _>, _>>()??;
    for record in &mut records {
        record.commits = load_commits(conn, record.id)?;
    }
    Ok(records)
}

/// Moves a change to `next` if its state machine allows it. The update is a
/// compare-and-swap on the state read, so two racing transitions cannot both
/// apply.
pub fn transition(
    conn: &mut Connection,
    id: ChangeId,
    next: ChangeState,
) -> Result<ChangeRecord, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut record = load(&tx, id)?;
    if !record.state.can_transition_to(next) {
        return Err(ChangeError::InvalidTransition {
            change: id,
            from: record.state,
            to: next,
        });
    }
    let now = now_millis();
    tx.execute(
        "UPDATE changes SET state = ?1, updated_at = ?2 WHERE id = ?3 AND state = ?4",
        params![next.as_str(), now, id.to_string(), record.state.as_str()],
    )?;
    tx.commit()?;
    record.state = next;
    record.updated_at = now;
    Ok(record)
}

pub fn abandon(conn: &mut Connection, id: ChangeId) -> Result<ChangeRecord, ChangeError> {
    transition(conn, id, ChangeState::Abandoned)
}

/// Appends a snapshot or named commit and makes its tree the head. Earlier
/// commits are never touched (R2).
pub fn append_commit(
    conn: &mut Connection,
    id: ChangeId,
    commit: ObjectId,
    tree: ObjectId,
) -> Result<ChangeRecord, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_live(&load(&tx, id)?)?;
    tx.execute(
        "INSERT INTO change_commits (change_id, seq, commit_id)
         SELECT ?1, COALESCE(MAX(seq), 0) + 1, ?2 FROM change_commits WHERE change_id = ?1",
        params![id.to_string(), commit.to_hex()],
    )?;
    tx.execute(
        "UPDATE changes SET head_tree = ?1, updated_at = ?2 WHERE id = ?3",
        params![tree.to_hex(), now_millis(), id.to_string()],
    )?;
    let record = load(&tx, id)?;
    tx.commit()?;
    Ok(record)
}

/// Records a server-side rebase (R8, D19): a new parent and the merged head
/// tree. The ID and the commit list are left exactly as they were.
pub fn rebase(
    conn: &mut Connection,
    id: ChangeId,
    parent: ChangeParent,
    head_tree: ObjectId,
) -> Result<ChangeRecord, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_live(&load(&tx, id)?)?;
    if let ChangeParent::Change(below) = parent {
        ensure_not_above(&tx, id, below)?;
    }
    let (base, below) = parent_columns(parent);
    tx.execute(
        "UPDATE changes SET parent_base = ?1, parent_change = ?2, head_tree = ?3, updated_at = ?4
         WHERE id = ?5",
        params![
            base,
            below,
            head_tree.to_hex(),
            now_millis(),
            id.to_string()
        ],
    )?;
    let record = load(&tx, id)?;
    tx.commit()?;
    Ok(record)
}

/// Walks down from `below` to the base; finding `id` on the way means stacking
/// `id` on `below` would close a loop.
fn ensure_not_above(conn: &Connection, id: ChangeId, below: ChangeId) -> Result<(), ChangeError> {
    let mut cursor = Some(below);
    while let Some(current) = cursor {
        if current == id {
            return Err(ChangeError::Cycle {
                change: id,
                parent: below,
            });
        }
        cursor = match load(conn, current)?.parent {
            ChangeParent::Change(next) => Some(next),
            ChangeParent::Base(_) => None,
        };
    }
    Ok(())
}

fn ensure_live(record: &ChangeRecord) -> Result<(), ChangeError> {
    match record.state {
        ChangeState::Open | ChangeState::Ready => Ok(()),
        state => Err(ChangeError::Closed {
            change: record.id,
            state,
        }),
    }
}

fn load(conn: &Connection, id: ChangeId) -> Result<ChangeRecord, ChangeError> {
    let sql = format!("SELECT {COLUMNS} FROM changes WHERE id = ?1");
    let mut record = conn
        .prepare_cached(&sql)?
        .query_row(params![id.to_string()], read_row)
        .optional()?
        .ok_or_else(|| ChangeError::NotFound(id.to_string()))??;
    record.commits = load_commits(conn, id)?;
    Ok(record)
}

fn load_commits(conn: &Connection, id: ChangeId) -> Result<Vec<ObjectId>, ChangeError> {
    let mut stmt = conn
        .prepare_cached("SELECT commit_id FROM change_commits WHERE change_id = ?1 ORDER BY seq")?;
    let hexes: Vec<String> = stmt
        .query_map(params![id.to_string()], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    hexes.iter().map(|hex| parse_column(hex)).collect()
}

/// The outer result is SQLite's; the inner one reports a row whose text does
/// not parse, which only a corrupted or hand-edited store produces.
fn read_row(row: &Row<'_>) -> rusqlite::Result<Result<ChangeRecord, ChangeError>> {
    let id: String = row.get(0)?;
    let owner: String = row.get(1)?;
    let parent_base: Option<String> = row.get(2)?;
    let parent_change: Option<String> = row.get(3)?;
    let state: String = row.get(4)?;
    let head_tree: String = row.get(5)?;
    let (created_at, updated_at) = (row.get(6)?, row.get(7)?);
    Ok((|| {
        let parent = match (parent_base, parent_change) {
            (Some(base), None) => ChangeParent::Base(parse_column(&base)?),
            (None, Some(below)) => ChangeParent::Change(parse_column(&below)?),
            _ => return Err(corrupt(&id, "parent")),
        };
        Ok(ChangeRecord {
            id: parse_column(&id)?,
            owner: AgentId::new(owner),
            parent,
            state: state.parse().map_err(|_| corrupt(&id, "state"))?,
            head_tree: parse_column(&head_tree)?,
            commits: Vec::new(),
            created_at,
            updated_at,
        })
    })())
}

fn parse_column<T: std::str::FromStr>(text: &str) -> Result<T, ChangeError> {
    text.parse().map_err(|_| corrupt(text, "column"))
}

fn corrupt(text: &str, what: &str) -> ChangeError {
    ChangeError::Store(rusqlite::Error::InvalidColumnName(format!(
        "unreadable {what} {text:?}"
    )))
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
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
