//! The derived history indexes (SPEC R11), cached in the metadata store.
//!
//! Every row is derived from one immutable commit, so a row is never stale
//! and never updated: it is written once with `INSERT OR IGNORE`, and two
//! queries racing to derive the same row write the same values. Losing the
//! tables loses nothing but time; they are rebuilt from the object store.

use super::trees::{self, Change as FileChange};
use super::{Ctx, HistoryError, HistoryResult};
use rosen_core::history::LandedTrailers;
use rosen_core::{ChangeId, CommitGraph, ObjectId};
use rosen_protocol::LogEntry;
use rosen_store::{LooseObjectStore, ObjectStore};
use rusqlite::{params, Connection, OptionalExtension};
use std::cell::RefCell;
use std::path::Path;

const SCHEMA: &str = "
-- One row per commit a query has walked: its ancestry and log line.
CREATE TABLE IF NOT EXISTS history_commits (
    commit_id     TEXT PRIMARY KEY,
    tree          TEXT NOT NULL,
    parents       TEXT NOT NULL,      -- space-separated, first parent first
    author        TEXT NOT NULL,
    time          INTEGER NOT NULL,
    summary       TEXT NOT NULL,
    change_id     TEXT,               -- the Rosen-Change trailer, if landed
    paths_indexed INTEGER NOT NULL DEFAULT 0
);
-- The files each commit changed against its first parent (path history).
CREATE TABLE IF NOT EXISTS history_paths (
    commit_id TEXT NOT NULL,
    path      TEXT NOT NULL,
    PRIMARY KEY (commit_id, path)
) WITHOUT ROWID;
-- Blame of a file at a commit, as runs of lines from one origin commit.
CREATE TABLE IF NOT EXISTS history_blame (
    commit_id TEXT NOT NULL,
    path      TEXT NOT NULL,
    runs      TEXT NOT NULL,
    PRIMARY KEY (commit_id, path)
) WITHOUT ROWID;
";

/// A commit as the log shows it.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub id: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: String,
    pub time: i64,
    pub summary: String,
    pub change: Option<ChangeId>,
}

impl CommitInfo {
    pub fn first_parent(&self) -> Option<ObjectId> {
        self.parents.first().copied()
    }

    pub fn entry(&self) -> LogEntry {
        LogEntry {
            commit: self.id,
            parents: self.parents.clone(),
            tree: self.tree,
            author: self.author.clone(),
            time: self.time,
            summary: self.summary.clone(),
            change: self.change,
        }
    }
}

/// Lines `start..start + len` of a file came from lines
/// `original_start..` of `commit`'s version of it. All 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub commit: ObjectId,
    pub original_start: u32,
    pub len: u32,
}

/// A connection to the metadata store for one query.
pub struct Index {
    conn: Connection,
}

impl Index {
    /// Opens the metadata store; `create` also creates the cache tables,
    /// which the server does once per process.
    pub fn open(path: &Path, create: bool) -> HistoryResult<Index> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| HistoryError::Store(e.into()))?;
        }
        let conn = crate::metadata::open(path)?;
        if create {
            conn.execute_batch(SCHEMA)?;
        }
        Ok(Index { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// A commit's row, derived from the store the first time it is asked for.
    pub fn commit(&self, store: &LooseObjectStore, id: &ObjectId) -> HistoryResult<CommitInfo> {
        if let Some(info) = self.cached_commit(id)? {
            return Ok(info);
        }
        let commit = store.read_commit(id)?;
        let message = commit.message_text();
        let info = CommitInfo {
            id: *id,
            tree: commit.tree,
            parents: commit.parents.clone(),
            author: format!(
                "{} <{}>",
                String::from_utf8_lossy(&commit.author.name),
                String::from_utf8_lossy(&commit.author.email)
            ),
            time: commit.author.seconds,
            summary: message.lines().next().unwrap_or_default().to_owned(),
            change: LandedTrailers::parse(&commit.message).map(|t| t.change),
        };
        let parents: Vec<String> = info.parents.iter().map(ObjectId::to_hex).collect();
        self.conn.execute(
            "INSERT OR IGNORE INTO history_commits
             (commit_id, tree, parents, author, time, summary, change_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id.to_hex(),
                info.tree.to_hex(),
                parents.join(" "),
                info.author,
                info.time,
                info.summary,
                info.change.map(|c| c.to_string()),
            ],
        )?;
        Ok(info)
    }

    fn cached_commit(&self, id: &ObjectId) -> HistoryResult<Option<CommitInfo>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT tree, parents, author, time, summary, change_id
             FROM history_commits WHERE commit_id = ?1",
        )?;
        let row = stmt
            .query_row([id.to_hex()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .optional()?;
        let Some((tree, parents, author, time, summary, change)) = row else {
            return Ok(None);
        };
        Ok(Some(CommitInfo {
            id: *id,
            tree: ObjectId::from_hex(&tree)?,
            parents: parents
                .split_whitespace()
                .map(ObjectId::from_hex)
                .collect::<Result<_, _>>()?,
            author,
            time,
            summary,
            change: change.map(|c| c.parse()).transpose()?,
        }))
    }

    /// The files `commit` changed against its first parent (every file, for
    /// a root commit), derived once and then read from the cache.
    pub fn changed_paths(
        &self,
        store: &LooseObjectStore,
        info: &CommitInfo,
    ) -> HistoryResult<Vec<String>> {
        let id = info.id.to_hex();
        let indexed: bool = self
            .conn
            .prepare_cached("SELECT paths_indexed FROM history_commits WHERE commit_id = ?1")?
            .query_row([&id], |row| row.get(0))
            .optional()?
            .unwrap_or(false);
        if indexed {
            let mut stmt = self
                .conn
                .prepare_cached("SELECT path FROM history_paths WHERE commit_id = ?1")?;
            let paths = stmt.query_map([&id], |row| row.get(0))?;
            return Ok(paths.collect::<Result<_, _>>()?);
        }
        let parent_tree = match info.first_parent() {
            Some(parent) => Some(self.commit(store, &parent)?.tree),
            None => None,
        };
        let changes = trees::changed_files(store, parent_tree, Some(info.tree), &[])?;
        let paths: Vec<String> = changes.into_iter().map(|c: FileChange| c.path).collect();
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR IGNORE INTO history_paths (commit_id, path) VALUES (?1, ?2)",
            )?;
            for path in &paths {
                insert.execute(params![id, path])?;
            }
        }
        tx.execute(
            "UPDATE history_commits SET paths_indexed = 1 WHERE commit_id = ?1",
            [&id],
        )?;
        tx.commit()?;
        Ok(paths)
    }

    pub fn blame(&self, commit: &ObjectId, path: &str) -> HistoryResult<Option<Vec<Run>>> {
        let runs: Option<String> = self
            .conn
            .prepare_cached("SELECT runs FROM history_blame WHERE commit_id = ?1 AND path = ?2")?
            .query_row(params![commit.to_hex(), path], |row| row.get(0))
            .optional()?;
        runs.map(|text| decode_runs(&text)).transpose()
    }

    pub fn put_blame(&self, commit: &ObjectId, path: &str, runs: &[Run]) -> HistoryResult<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO history_blame (commit_id, path, runs) VALUES (?1, ?2, ?3)",
            params![commit.to_hex(), path, encode_runs(runs)],
        )?;
        Ok(())
    }
}

/// Runs as `commit:original_start:len`, space-separated: compact, and the
/// cache never needs to query inside it.
fn encode_runs(runs: &[Run]) -> String {
    let parts: Vec<String> = runs
        .iter()
        .map(|r| format!("{}:{}:{}", r.commit, r.original_start, r.len))
        .collect();
    parts.join(" ")
}

fn decode_runs(text: &str) -> HistoryResult<Vec<Run>> {
    let corrupt = || {
        HistoryError::Core(rosen_core::Error::MalformedObject(format!(
            "unreadable cached blame {text:?}"
        )))
    };
    text.split_whitespace()
        .map(|part| {
            let mut fields = part.split(':');
            let commit = ObjectId::from_hex(fields.next().ok_or_else(corrupt)?)?;
            let mut number = || -> HistoryResult<u32> {
                fields
                    .next()
                    .and_then(|f| f.parse().ok())
                    .ok_or_else(corrupt)
            };
            Ok(Run {
                commit,
                original_start: number()?,
                len: number()?,
            })
        })
        .collect()
}

/// Commit ancestry for the core history walks, read through the cache. The
/// walks only see `rosen_core` errors, so the first real failure is kept
/// here and reported after the walk.
pub struct Graph<'a> {
    ctx: &'a Ctx<'a>,
    failure: RefCell<Option<HistoryError>>,
}

impl<'a> Graph<'a> {
    pub fn new(ctx: &'a Ctx<'a>) -> Self {
        Graph {
            ctx,
            failure: RefCell::new(None),
        }
    }

    /// The failure that cut a walk short, if any.
    pub fn take_failure(&self) -> Option<HistoryError> {
        self.failure.borrow_mut().take()
    }
}

impl CommitGraph for Graph<'_> {
    fn parents(&self, commit: &ObjectId) -> rosen_core::Result<Vec<ObjectId>> {
        match self.ctx.index.commit(&self.ctx.store, commit) {
            Ok(info) => Ok(info.parents),
            Err(err) => {
                self.failure.borrow_mut().get_or_insert(err);
                Err(rosen_core::Error::MissingCommit(*commit))
            }
        }
    }
}
