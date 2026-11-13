//! Review threads and comments in the metadata store. Each write is one
//! `BEGIN IMMEDIATE` transaction that re-reads the change inside it, so a
//! comment is never stored against a head the change no longer has.

use super::ReviewError;
use crate::changes;
use rosen_core::{AgentId, ChangeId, ObjectId, ThreadState};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use std::time::{SystemTime, UNIX_EPOCH};

pub use rosen_protocol::{ReviewComment, ReviewThread};

/// Creates the review tables if they are missing. Safe to call on every start.
///
/// IDs autoincrement and rows are never deleted, so a thread number an agent
/// saw once always names the same thread.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS review_threads (
             id          INTEGER PRIMARY KEY AUTOINCREMENT,
             change_id   TEXT    NOT NULL REFERENCES changes(id),
             path        TEXT    NOT NULL,
             line        INTEGER NOT NULL CHECK (line >= 1),
             head        TEXT    NOT NULL,
             state       TEXT    NOT NULL CHECK (state IN ('open', 'resolved')),
             opened_by   TEXT    NOT NULL,
             resolved_by TEXT,
             created_at  INTEGER NOT NULL,
             updated_at  INTEGER NOT NULL,
             CHECK ((state = 'resolved') = (resolved_by IS NOT NULL))
         );
         CREATE INDEX IF NOT EXISTS review_threads_by_change
             ON review_threads(change_id, state);
         CREATE TABLE IF NOT EXISTS review_comments (
             id         INTEGER PRIMARY KEY AUTOINCREMENT,
             thread_id  INTEGER NOT NULL REFERENCES review_threads(id),
             author     TEXT    NOT NULL,
             body       TEXT    NOT NULL,
             head       TEXT    NOT NULL,
             created_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS review_comments_by_thread
             ON review_comments(thread_id, id);",
    )
}

/// A new thread's first comment, already checked against the head it names.
pub struct NewThread<'a> {
    pub change: ChangeId,
    pub head: ObjectId,
    pub path: &'a str,
    pub line: u32,
    pub author: &'a AgentId,
    pub body: &'a str,
}

/// Stores a new thread, unless the change's head moved after the anchor was
/// checked. `Ok(None)` means it moved: the caller checks again and retries.
pub fn open_thread(
    conn: &mut Connection,
    new: &NewThread,
) -> Result<Option<ReviewThread>, ReviewError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if live_head(&tx, new.change)? != new.head {
        return Ok(None);
    }
    let now = now_millis();
    tx.execute(
        "INSERT INTO review_threads
             (change_id, path, line, head, state, opened_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6, ?6)",
        params![
            new.change.to_string(),
            new.path,
            new.line,
            new.head.to_hex(),
            new.author.as_str(),
            now
        ],
    )?;
    let thread = u64::try_from(tx.last_insert_rowid())
        .map_err(|_| ReviewError::Store("negative thread id".into()))?;
    insert_comment(&tx, thread, new.author, new.body, new.head, now)?;
    let stored = load(&tx, thread)?;
    tx.commit()?;
    Ok(Some(stored))
}

/// Appends a reply to a thread on `change`, reopening it if it was resolved.
/// Returns the thread and whether the reply reopened it.
pub fn reply(
    conn: &mut Connection,
    change: ChangeId,
    expected_head: Option<ObjectId>,
    thread: u64,
    author: &AgentId,
    body: &str,
) -> Result<(ReviewThread, bool), ReviewError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let before = load(&tx, thread)?;
    if before.change != change {
        return Err(ReviewError::Invalid(format!(
            "thread {thread} belongs to change {}, not {change}",
            before.change
        )));
    }
    let head = live_head(&tx, change)?;
    super::check_head(expected_head, head)?;
    let now = now_millis();
    insert_comment(&tx, thread, author, body, head, now)?;
    let next = before.state.after_reply();
    tx.execute(
        "UPDATE review_threads SET state = ?2, resolved_by = NULL, updated_at = ?3
         WHERE id = ?1",
        params![thread, next.as_str(), now],
    )?;
    let after = load(&tx, thread)?;
    tx.commit()?;
    Ok((after, before.state != next))
}

/// Resolves a thread. Returns it and whether this call changed its state;
/// resolving a resolved thread changes nothing, so a retried call is safe.
pub fn resolve(
    conn: &mut Connection,
    thread: u64,
    by: &AgentId,
) -> Result<(ReviewThread, bool), ReviewError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE review_threads SET state = 'resolved', resolved_by = ?2, updated_at = ?3
         WHERE id = ?1 AND state = 'open'",
        params![thread, by.as_str(), now_millis()],
    )? == 1;
    let stored = load(&tx, thread)?;
    tx.commit()?;
    Ok((stored, changed))
}

/// A change's threads, oldest first, each with its comments.
pub fn list(
    conn: &Connection,
    change: ChangeId,
    state: Option<ThreadState>,
) -> Result<Vec<ReviewThread>, ReviewError> {
    let mut stmt = conn.prepare_cached(&format!(
        "{THREAD_SELECT} WHERE change_id = ?1 AND (?2 IS NULL OR state = ?2) ORDER BY id"
    ))?;
    let mut threads: Vec<ReviewThread> = stmt
        .query_map(
            params![change.to_string(), state.map(ThreadState::as_str)],
            read_thread,
        )?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|row| row.parse())
        .collect::<Result<_, _>>()?;
    for thread in &mut threads {
        thread.comments = comments(conn, thread.id)?;
    }
    Ok(threads)
}

/// The change's head, refusing a change that can no longer be reviewed.
fn live_head(tx: &Transaction, change: ChangeId) -> Result<ObjectId, ReviewError> {
    let record = changes::get(tx, change)?;
    super::check_reviewable(&record)?;
    Ok(record.change.head)
}

fn insert_comment(
    tx: &Transaction,
    thread: u64,
    author: &AgentId,
    body: &str,
    head: ObjectId,
    now: i64,
) -> Result<(), ReviewError> {
    tx.execute(
        "INSERT INTO review_comments (thread_id, author, body, head, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![thread, author.as_str(), body, head.to_hex(), now],
    )?;
    Ok(())
}

const THREAD_SELECT: &str = "SELECT id, change_id, path, line, head, state, opened_by,
         resolved_by, created_at, updated_at
     FROM review_threads";

fn load(conn: &Connection, thread: u64) -> Result<ReviewThread, ReviewError> {
    let row = conn
        .prepare_cached(&format!("{THREAD_SELECT} WHERE id = ?1"))?
        .query_row(params![thread], read_thread)
        .optional()?
        .ok_or(ReviewError::ThreadNotFound(thread))?;
    let mut stored = row.parse()?;
    stored.comments = comments(conn, thread)?;
    Ok(stored)
}

fn comments(conn: &Connection, thread: u64) -> Result<Vec<ReviewComment>, ReviewError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, author, body, head, created_at FROM review_comments
         WHERE thread_id = ?1 ORDER BY id",
    )?;
    let rows: Vec<(u64, String, String, String, i64)> = stmt
        .query_map(params![thread], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(|(id, author, body, head, created_at)| {
            Ok(ReviewComment {
                id,
                author: agent(&author, "comment author")?,
                body,
                head: parse(&head, "comment head")?,
                created_at,
            })
        })
        .collect()
}

/// A thread row as stored; parsed after the query so a damaged row is
/// reported as corrupt instead of as an SQL error.
struct ThreadRow {
    id: u64,
    change: String,
    path: String,
    line: u32,
    head: String,
    state: String,
    opened_by: String,
    resolved_by: Option<String>,
    created_at: i64,
    updated_at: i64,
}

fn read_thread(row: &Row) -> rusqlite::Result<ThreadRow> {
    Ok(ThreadRow {
        id: row.get(0)?,
        change: row.get(1)?,
        path: row.get(2)?,
        line: row.get(3)?,
        head: row.get(4)?,
        state: row.get(5)?,
        opened_by: row.get(6)?,
        resolved_by: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

impl ThreadRow {
    fn parse(self) -> Result<ReviewThread, ReviewError> {
        Ok(ReviewThread {
            id: self.id,
            change: parse(&self.change, "thread change")?,
            path: self.path,
            line: self.line,
            head: parse(&self.head, "thread head")?,
            state: parse(&self.state, "thread state")?,
            opened_by: agent(&self.opened_by, "thread author")?,
            resolved_by: self
                .resolved_by
                .map(|by| agent(&by, "thread resolver"))
                .transpose()?,
            created_at: self.created_at,
            updated_at: self.updated_at,
            comments: Vec::new(),
        })
    }
}

pub(super) fn parse<T: std::str::FromStr>(text: &str, what: &str) -> Result<T, ReviewError> {
    text.parse().map_err(|_| corrupt(what, text))
}

pub(super) fn agent(text: &str, what: &str) -> Result<AgentId, ReviewError> {
    AgentId::new(text).map_err(|_| corrupt(what, text))
}

fn corrupt(what: &str, text: &str) -> ReviewError {
    ReviewError::Store(format!(
        "metadata store holds an unreadable {what} {text:?}"
    ))
}

pub(super) fn now_millis() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}
