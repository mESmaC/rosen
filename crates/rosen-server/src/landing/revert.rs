//! R12 server-only inverse land. The bisection service supplies a confirmed
//! culprit; this entry point is deliberately absent from the client protocol.
use super::{revert_store as db, LandError, LandResult, Ledger};
use crate::ServerState;
use rosen_core::{history::LandedTrailers, ChangeId, LandedCommit, ObjectId, Signature};
use rosen_merge::MergePolicy;
use rosen_protocol::EventKind;
use rosen_store::{LooseObjectStore, ObjectStore};

/// Called only after the checker confirms two culprit failures and a passing
/// parent. A retry after a crash returns the original revert, never a second land.
pub fn revert(state: &ServerState, culprit: ObjectId, output: &str) -> LandResult<ObjectId> {
    let mut conn = crate::metadata::open(&state.config.metadata_path())?;
    db::ensure_schema(&conn)?;
    for _ in 0..12 {
        if let Some(commit) = db::completed(&conn, culprit)? {
            recover(state)?;
            return Ok(commit);
        }
        let ledger = Ledger::open(&state.config.metadata_path())?;
        let (name, base) = ledger
            .project_base()?
            .ok_or_else(|| LandError::NoBase("main".into()))?;
        // Reopening must follow the culprit's last post-land mutation, or an
        // older worker could close it again after the inverse commits.
        if !db::culprit_recovered(&conn, &name, culprit)? {
            state.landing.recover(state).map_err(backend)?;
        }
        let plan = prepare(state, culprit, base)?;
        let commit = plan.commit;
        if db::commit(&mut conn, &name, &plan, output)? {
            state.landing.recover(state).map_err(backend)?;
            return Ok(commit);
        }
    }
    Err(LandError::Contended { attempts: 12 })
}

/// Durable failure output remains queryable after delivery and restart.
pub fn failure_output(state: &ServerState, culprit: ObjectId) -> LandResult<Option<String>> {
    let conn = crate::metadata::open(&state.config.metadata_path())?;
    db::ensure_schema(&conn)?;
    db::output(&conn, culprit)
}

/// Replays the outbox outside the registry mutex, since audience lookup reads it.
pub fn recover(state: &ServerState) -> LandResult<()> {
    let conn = crate::metadata::open(&state.config.metadata_path())?;
    db::ensure_schema(&conn)?;
    for (culprit, change, commit, output) in db::pending(&conn)? {
        let detail = serde_json::json!({"culprit":culprit,"commit":commit,"output":output});
        if state
            .events
            .publish(EventKind::Reverted, Some(change), detail)
            != 0
        {
            db::delivered(&conn, &culprit)?;
        }
    }
    Ok(())
}

fn backend(error: impl std::fmt::Display) -> LandError {
    LandError::Backend(error.to_string())
}

fn prepare(state: &ServerState, culprit: ObjectId, base: ObjectId) -> LandResult<db::Revert> {
    let store = LooseObjectStore::open(state.config.objects_dir()).map_err(backend)?;
    let bad = store.read_commit(&culprit).map_err(backend)?;
    let parent = bad
        .parents
        .first()
        .ok_or_else(|| LandError::Backend("culprit has no parent".into()))?;
    let before = store.read_commit(parent).map_err(backend)?;
    let current = store.read_commit(&base).map_err(backend)?;
    let ctx = crate::history::open(state).map_err(backend)?;
    // Generated/registration policies may discard edits. An inverse must
    // preserve every later edit or refuse with conflicts, so use strict merge.
    let merged =
        crate::history::merge::merge_trees(&ctx, [bad.tree, current.tree, before.tree], &|_| {
            MergePolicy::Text
        })
        .map_err(backend)?;
    let tree = merged
        .tree
        .ok_or_else(|| LandError::Backend(format!("revert conflicts: {:?}", merged.conflicts)))?;
    write_inverse(&store, culprit, base, tree, current.tree)
}
fn write_inverse(
    store: &LooseObjectStore,
    culprit: ObjectId,
    base: ObjectId,
    tree: ObjectId,
    reopened_head: ObjectId,
) -> LandResult<db::Revert> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(backend)?;
    let id = ChangeId::from_bytes(bytes);
    let who = Signature::new("rosen-server", "server@rosen", 0);
    let commit = store
        .put_commit(
            &LandedCommit {
                base,
                tree,
                author: who.clone(),
                committer: who,
                description: format!("Revert {culprit}"),
                trailers: LandedTrailers {
                    change: id,
                    receipt: None,
                    approvers: vec![],
                },
            }
            .to_commit(),
        )
        .map_err(backend)?;
    Ok(db::Revert {
        culprit,
        base,
        commit,
        id,
        tree,
        reopened_head,
    })
}
