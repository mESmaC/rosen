//! Revision names: `base`, a 40-hex commit or tree, or a change ID or unique
//! prefix of one. Change IDs use only the letters k-z, so the two never
//! collide.

use super::{Ctx, HistoryError, HistoryResult};
use crate::changes;
use rosen_core::{Change, ChangeParent, ObjectId, ObjectKind};
use rusqlite::OptionalExtension;
use std::collections::HashSet;

/// The name that always means the base ref's current commit.
const BASE: &str = "base";

/// A resolved revision.
pub enum Revision {
    Commit(ObjectId),
    /// A bare tree, such as a merge result: it can be read and diffed, but
    /// has no history.
    Tree(ObjectId),
    Change(Box<Change>),
}

impl Revision {
    pub fn resolve(ctx: &Ctx, text: Option<&str>) -> HistoryResult<Revision> {
        let text = text.map(str::trim).filter(|t| !t.is_empty());
        match text {
            None | Some(BASE) => Ok(Revision::Commit(base_commit(ctx)?)),
            Some(hex) if hex.len() == ObjectId::HEX_LEN && is_hex(hex) => object(ctx, hex),
            Some(change) => {
                let record = ctx
                    .state
                    .changes
                    .with_conn(|c| changes::get(c, changes::resolve(c, change)?))?;
                Ok(Revision::Change(Box::new(record.change)))
            }
        }
    }

    /// The tree this revision reads as. A change reads as its head.
    pub fn tree(&self, ctx: &Ctx) -> HistoryResult<ObjectId> {
        match self {
            Revision::Commit(commit) => Ok(ctx.index.commit(&ctx.store, commit)?.tree),
            Revision::Tree(tree) => Ok(*tree),
            Revision::Change(change) => Ok(change.head),
        }
    }

    /// The commit this revision's history continues from: a commit itself,
    /// or the base commit a change's stack sits on.
    pub fn history_commit(&self, ctx: &Ctx) -> HistoryResult<ObjectId> {
        match self {
            Revision::Commit(commit) => Ok(*commit),
            Revision::Tree(tree) => Err(HistoryError::Invalid(format!(
                "{tree} is a tree, which has no history; name a commit or a change"
            ))),
            Revision::Change(change) => Ok(stack_root(ctx, change)?.1),
        }
    }
}

/// The base ref's current commit, read from the landing tables. A project
/// without a base has no such table or row yet.
pub fn base_commit(ctx: &Ctx) -> HistoryResult<ObjectId> {
    let conn = ctx.index.conn();
    let has_table: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'base_refs'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    let target: Option<String> = if has_table {
        conn.query_row("SELECT target FROM base_refs LIMIT 1", [], |row| row.get(0))
            .optional()?
    } else {
        None
    };
    let target = target.ok_or_else(|| {
        HistoryError::Refused(
            "the project has no base yet; import a repository with `rosen import`".into(),
        )
    })?;
    Ok(ObjectId::from_hex(&target)?)
}

fn object(ctx: &Ctx, hex: &str) -> HistoryResult<Revision> {
    use rosen_store::ObjectStore as _;
    let id = ObjectId::from_hex(hex)?;
    let raw = ctx
        .store
        .get_raw(&id)?
        .ok_or_else(|| HistoryError::NotFound(format!("the server holds no object {id}")))?;
    match raw.kind {
        ObjectKind::Commit => Ok(Revision::Commit(id)),
        ObjectKind::Tree => Ok(Revision::Tree(id)),
        other => Err(HistoryError::Invalid(format!(
            "{id} is a {}, not a commit or tree",
            other.as_str()
        ))),
    }
}

fn is_hex(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The changes of a stack from `change` down, and the base commit under the
/// bottom one.
pub fn stack_root(ctx: &Ctx, change: &Change) -> HistoryResult<(Vec<Change>, ObjectId)> {
    let mut stack = vec![change.clone()];
    let mut seen = HashSet::from([change.id]);
    loop {
        let below = match stack.last().map(|c| c.parent) {
            Some(ChangeParent::Base(commit)) => return Ok((stack, commit)),
            Some(ChangeParent::Change(below)) => below,
            None => unreachable!("the stack starts with one change"),
        };
        // A damaged store could hold a loop; stop rather than spin.
        if !seen.insert(below) {
            return Err(rosen_core::Error::StackCycle(below).into());
        }
        let record = ctx.state.changes.with_conn(|c| changes::get(c, below))?;
        stack.push(record.change);
    }
}

/// The tree a change's edits are measured against: its base commit's tree,
/// or the head of the change below it.
pub fn parent_tree(ctx: &Ctx, change: &Change) -> HistoryResult<ObjectId> {
    match change.parent {
        ChangeParent::Base(commit) => Ok(ctx.index.commit(&ctx.store, &commit)?.tree),
        ChangeParent::Change(below) => {
            let record = ctx.state.changes.with_conn(|c| changes::get(c, below))?;
            Ok(record.change.head)
        }
    }
}
