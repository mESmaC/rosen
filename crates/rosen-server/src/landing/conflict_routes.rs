//! R8: attach a failed merge to its change before publishing the conflict event.
use super::{ChangeId, CommitId, LandError, LandResult, Routing};
use crate::history::trees::{content, file_at};
use crate::{changes, conflicts, ServerState};
use rosen_core::{Conflict, ObjectId, ObjectKind};
use rosen_merge::{merge_file, FileConflict, FileMerge, Hunk, MergePolicy};
use rosen_store::{LooseObjectStore, ObjectStore};

pub(super) fn record(
    state: &ServerState,
    id: ChangeId,
    base: CommitId,
    onto: CommitId,
    head: CommitId,
    paths: &[String],
) -> LandResult<Routing> {
    let store = LooseObjectStore::open(state.config.objects_dir()).map_err(error)?;
    let trees = [
        tree(&store, base)?,
        tree(&store, head)?,
        tree(&store, onto)?,
    ];
    let regions = paths
        .iter()
        .map(|path| regions(&store, trees, path))
        .collect::<LandResult<Vec<_>>>()?;
    state
        .changes
        .with_conn(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let change = changes::get(&tx, id)?.change;
            // A snapshot racing the merge must never receive conflicts from
            // an older head. Dropping `tx` rolls back; the caller merges again.
            if !accepts(change.head == head, change.state.is_closed()) {
                return Ok(Routing::ChangeMoved);
            }
            let mut set = conflicts::load(&tx, &id.to_string())
                .map_err(|e| changes::ChangeError::Corrupt(e.to_string()))?;
            for (path, regions) in regions {
                set.record_merge(&path, regions);
            }
            conflicts::save(&tx, &id.to_string(), &set)
                .map_err(|e| changes::ChangeError::Corrupt(e.to_string()))?;
            tx.commit()?;
            Ok(Routing::Stored)
        })
        .map_err(error)
}

pub(super) fn paths(state: &ServerState, id: ChangeId) -> LandResult<Vec<String>> {
    state
        .changes
        .with_conn(|conn| {
            let set = conflicts::load(conn, &id.to_string())
                .map_err(|e| changes::ChangeError::Corrupt(e.to_string()))?;
            Ok(set.open_paths().into_iter().map(str::to_owned).collect())
        })
        .map_err(error)
}

fn tree(store: &dyn ObjectStore, id: ObjectId) -> LandResult<ObjectId> {
    match store.get_raw(&id).map_err(error)?.map(|o| o.kind) {
        Some(ObjectKind::Tree) => Ok(id),
        Some(ObjectKind::Commit) => Ok(store.read_commit(&id).map_err(error)?.tree),
        _ => Err(LandError::Backend(format!("{id} is not a commit or tree"))),
    }
}

/// `ours` always means the owning change, even though the landing merge
/// applies that change onto the base's tree in the opposite argument order.
fn regions(
    store: &dyn ObjectStore,
    trees: [ObjectId; 3],
    path: &str,
) -> LandResult<(String, Vec<Conflict>)> {
    let mut sides = Vec::new();
    let mut blobs = true;
    for tree in trees {
        let entry = file_at(store, tree, path).map_err(error)?;
        blobs &= entry.is_none_or(|e| e.is_blob());
        sides.push(content(store, entry).map_err(error)?);
    }
    let [base, ours, theirs] = [&sides[0], &sides[1], &sides[2]];
    if blobs {
        if let FileMerge::Conflicted(FileConflict::Content(text)) = merge_file(
            base.as_deref(),
            ours.as_deref(),
            theirs.as_deref(),
            MergePolicy::Text,
        ) {
            return Ok((path.into(), text_regions(path, text.hunks)?));
        }
    }
    // Binary, gitlink, modify/delete and file/directory conflicts retain all
    // three complete sides, including absence as empty bytes; no side is picked.
    let sides = sides
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect::<Vec<_>>();
    Ok((
        path.into(),
        vec![Conflict::new(
            path,
            1,
            sides[0].clone(),
            sides[1].clone(),
            sides[2].clone(),
        )],
    ))
}

fn text_regions(path: &str, hunks: Vec<Hunk>) -> LandResult<Vec<Conflict>> {
    let mut line = 1usize;
    let mut out = Vec::new();
    for hunk in hunks {
        match hunk {
            Hunk::Clean(bytes) => line += lines(&bytes),
            Hunk::Conflict(region) => {
                let start = u32::try_from(line).map_err(error)?;
                line += 4 + lines(&region.base) + lines(&region.ours) + lines(&region.theirs);
                out.push(Conflict::new(
                    path,
                    start,
                    region.base,
                    region.ours,
                    region.theirs,
                ));
            }
        }
    }
    Ok(out)
}

fn lines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b == b'\n').count()
        + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'))
}

fn error(err: impl std::fmt::Display) -> LandError {
    LandError::Backend(err.to_string())
}

fn accepts(same_head: bool, closed: bool) -> bool {
    same_head && !closed
}

#[test]
fn routing_matches_proved_fixture() {
    // Risk (R8): stale results must never attach conflicts to a newer head.
    for row in include_str!("../../../../proofs/fixtures/conflict-routing.tsv")
        .lines()
        .skip(1)
    {
        let fields: Vec<_> = row.split('\t').collect();
        assert_eq!(
            accepts(fields[0] == "1", fields[1] == "1"),
            fields[2] == "1"
        );
    }
}
