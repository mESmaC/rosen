//! Snapshots replace open-conflict files without merging their marker text.
use crate::changes::ChangeError;
use crate::history::{trees, Ctx, HistoryError};
use rosen_core::{ChangeId, ObjectId, Tree};
use rosen_store::ObjectStore;
use rusqlite::Connection;
use std::collections::BTreeMap;

pub(super) fn open_paths(conn: &Connection, change: ChangeId) -> Result<Vec<String>, ChangeError> {
    let set = crate::conflicts::load(conn, &change.to_string())
        .map_err(|err| ChangeError::Corrupt(err.to_string()))?;
    Ok(set.open_paths().into_iter().map(str::to_owned).collect())
}

pub(super) fn merge_sides(
    ctx: &Ctx,
    [basis, head, snapshot]: [ObjectId; 3],
    paths: &[String],
) -> Result<[ObjectId; 3], ChangeError> {
    if paths.is_empty() {
        return Ok([basis, head, snapshot]);
    }
    // Replacing both the basis and head makes these files unchanged from
    // the snapshot's perspective, even when the local basis predates a rebase.
    // Conflict records stay open until the owner explicitly resolves them.
    Ok([
        replace(ctx, basis, snapshot, paths).map_err(failure)?,
        replace(ctx, head, snapshot, paths).map_err(failure)?,
        snapshot,
    ])
}

fn replace(
    ctx: &Ctx,
    tree: ObjectId,
    snapshot: ObjectId,
    paths: &[String],
) -> Result<ObjectId, HistoryError> {
    let edits: BTreeMap<_, _> =
        trees::changed_files(&ctx.store, Some(tree), Some(snapshot), paths)?
            .into_iter()
            .map(|change| (change.path, change.new))
            .collect();
    let mut collisions = Vec::new();
    let root = trees::apply(&ctx.store, Some(tree), &edits, &mut collisions)?;
    if !collisions.is_empty() {
        return Err(HistoryError::Refused(format!(
            "resolution needs a file and a directory at {}",
            collisions.join(", ")
        )));
    }
    Ok(match root {
        Some(root) => root,
        None => ctx.store.put_tree(&Tree::default())?,
    })
}

fn failure(err: HistoryError) -> ChangeError {
    ChangeError::Io(std::io::Error::other(err.to_string()))
}
