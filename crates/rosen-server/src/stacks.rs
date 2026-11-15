//! R8: carry parent updates through a stack without rewriting authored commits.
use crate::changes::{self, ChangeError};
use crate::events::Staged;
use crate::history::{
    self,
    trees::{content, file_at},
};
use crate::ServerState;
use rosen_core::{Change, ChangeId, ChangeParent, Conflict, ObjectId};
use rosen_protocol::{ChangeRecord, ErrorBody, ErrorCode, EventKind, Outcome, Response};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::sync::Arc;

pub(crate) fn track(conn: &Connection, id: ChangeId, tree: ObjectId) -> Result<(), ChangeError> {
    conn.execute(
        "INSERT OR REPLACE INTO stack_bases VALUES (?1, ?2)",
        params![id.to_string(), tree.to_hex()],
    )?;
    Ok(())
}

pub(crate) fn basis(
    conn: &Connection,
    id: ChangeId,
    _parent: ChangeId,
) -> Result<ObjectId, ChangeError> {
    let tree: Option<String> = conn
        .query_row(
            "SELECT tree FROM stack_bases WHERE change_id = ?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    match tree {
        Some(tree) => tree.parse().map_err(failure),
        // An edited legacy stack has no recoverable exact basis. Guessing the
        // parent's current head can interpret intervening lands as deletions;
        // its owner names the basis with `recover_basis` instead (#166).
        None => {
            let child = changes::get(conn, id)?.change;
            if child.snapshots.is_empty() {
                Ok(child.head)
            } else {
                Err(ChangeError::Base(format!(
                    "cannot safely rebase a legacy stack without its recorded parent tree; \
                     name the parent tree it started from with \
                     `rosen change recover {id} <commit-or-tree>`"
                )))
            }
        }
    }
}

pub async fn handle_recover(state: &Arc<ServerState>, change: String, basis: ObjectId) -> Outcome {
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || recover_basis(&state, &change, basis)).await {
        Ok(Ok(record)) => Outcome::Ok(Response::Change(record)),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(
            crate::rpc::change_error_code(&err),
            err.to_string(),
        )),
        Err(err) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, err.to_string())),
    }
}

/// Records the parent tree an edited legacy stack started from (#166), named
/// by its owner, then carries the parent's later edits in as any other stack
/// update: a three-way merge that keeps every authored snapshot and stores
/// conflicts on the child. The server never picks the tree. It accepts only
/// one the stack's history recorded, and never replaces a recorded basis.
pub fn recover_basis(
    state: &ServerState,
    change: &str,
    basis: ObjectId,
) -> Result<ChangeRecord, ChangeError> {
    let ctx = history::open(state).map_err(failure)?;
    let id = state.changes.show(change)?.change.id;
    state.changes.with_conn(|conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let child = changes::get(&tx, id)?.change;
        let ChangeParent::Change(parent) = child.parent else {
            return Err(refused("the change is not stacked on another change"));
        };
        if child.state.is_closed() {
            return Err(refused("the change is closed"));
        }
        let recorded: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM stack_bases WHERE change_id = ?1)",
            [id.to_string()],
            |r| r.get(0),
        )?;
        if recorded {
            return Err(refused("the change already has a recorded parent tree"));
        }
        // Without authored snapshots, the head is already the exact parent
        // tree it started from. Replacing it could discard later parent edits.
        if child.snapshots.is_empty() {
            return Err(refused(
                "the unedited change already has a safe implicit parent tree",
            ));
        }
        let parent = changes::get(&tx, parent)?.change;
        let tree = recorded_parent_tree(&ctx.store, &child, &parent, basis)?.ok_or_else(|| {
            refused(
                "the basis must be a snapshot of the parent change, or the commit \
                 the change's first snapshot followed",
            )
        })?;
        track(&tx, id, tree)?;
        tx.commit()?;
        Ok(())
    })?;
    reconcile(state)?;
    state.changes.with_conn(|conn| changes::get(conn, id))
}

/// The parent trees a legacy stack's own history recorded: each snapshot of
/// its parent change, and the commit its own first snapshot followed, which
/// was the parent's newest commit then. `basis` names one by commit or tree.
fn recorded_parent_tree(
    store: &dyn rosen_store::ObjectStore,
    child: &Change,
    parent: &Change,
    basis: ObjectId,
) -> Result<Option<ObjectId>, ChangeError> {
    if let Some(s) = parent
        .snapshots
        .iter()
        .find(|s| s.commit == basis || s.tree == basis)
    {
        return Ok(Some(s.tree));
    }
    let Some(first) = child.snapshots.first() else {
        return Ok(None);
    };
    let followed = store.read_commit(&first.commit).map_err(failure)?.parents;
    let Some(followed) = followed.first() else {
        return Ok(None);
    };
    let tree = store.read_commit(followed).map_err(failure)?.tree;
    Ok((*followed == basis || tree == basis).then_some(tree))
}

fn refused(reason: &str) -> ChangeError {
    ChangeError::Base(format!("cannot recover the stack basis: {reason}"))
}

/// Updates are durable before notification. A failed pass is retried at the
/// next snapshot, land or startup; its unchanged basis identifies pending work.
pub fn refresh(state: &ServerState) {
    if let Err(err) = reconcile(state) {
        eprintln!("rosen-server: stack rebase recovery pending: {err}");
    }
}

pub fn reconcile(state: &ServerState) -> Result<(), ChangeError> {
    let ctx = history::open(state).map_err(failure)?;
    let config = state.project_config.at_base().map_err(failure)?;
    let policy = |path: &str| {
        use rosen_merge::MergePolicy;
        match &config {
            Some((_, c)) if c.config.is_generated(path) => MergePolicy::Generated,
            Some((_, c)) if c.config.is_registration_list(path) => MergePolicy::RegistrationList,
            _ => MergePolicy::Text,
        }
    };
    // Every snapshot ends here, and most find no stack to move: that look
    // takes no write lock. A step's merge runs with nothing held, then
    // `apply` checks the step's inputs still stand and commits it. Inputs
    // that moved are read again; more than a few such rounds in a row mean
    // the stack is being written steadily, and the next snapshot or land
    // comes back here.
    let mut moved = 0;
    while moved < MOVED_ROUNDS {
        let Some(step) = state.changes.with_conn(|conn| pending_step(conn))? else {
            return Ok(());
        };
        let trees = [step.base, step.onto, step.current.head];
        let merged = history::merge::merge_trees(&ctx, trees, &policy).map_err(failure)?;
        let sides = conflict_sides(&ctx, [step.base, step.current.head, step.onto], &merged)?;
        let applied = state
            .changes
            .with_conn(|conn| apply(conn, &step, merged, sides))?;
        deliver(state)?;
        moved = if applied { 0 } else { moved + 1 };
    }
    Ok(())
}

/// Each conflicting path with its content at the base, in the change and in
/// the parent.
type ConflictSides = Vec<(String, [Vec<u8>; 3])>;

/// How many times in a row a stack step may find its inputs moved.
const MOVED_ROUNDS: u32 = 8;

/// One stacked change whose parent's head moved past the tree it was last
/// rebased onto, with everything its rebase is computed from.
struct Step {
    current: rosen_core::Change,
    /// The parent tree the change's edits are measured from.
    base: ObjectId,
    /// The parent's head now.
    onto: ObjectId,
}

const PENDING: &str = "SELECT c.id, b.tree, p.head FROM changes c
         JOIN stack_bases b ON b.change_id = c.id
         JOIN changes p ON p.id = c.parent_change
         WHERE c.state IN ('open', 'ready') AND p.state IN ('open', 'ready')
           AND b.tree <> p.head
           AND NOT EXISTS (SELECT 1 FROM conflicts f WHERE f.change_id = c.id AND f.state = 'open')";

/// The oldest stacked change that needs a rebase, read without a write lock.
fn pending_step(conn: &Connection) -> Result<Option<Step>, ChangeError> {
    let pending: Option<(String, String, String)> = conn
        .query_row(
            &format!("{PENDING} ORDER BY c.created_at, c.id LIMIT 1"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((id, base, onto)) = pending else {
        return Ok(None);
    };
    step_of(conn, id.parse().map_err(failure)?, &base, &onto)
}

/// The step for `id` from `base` onto `onto`, if the change may be rebased.
fn step_of(
    conn: &Connection,
    id: ChangeId,
    base: &str,
    onto: &str,
) -> Result<Option<Step>, ChangeError> {
    let base: ObjectId = base.parse().map_err(failure)?;
    let onto: ObjectId = onto.parse().map_err(failure)?;
    let current = changes::get(conn, id)?.change;
    let rosen_core::ChangeParent::Change(parent) = current.parent else {
        return Ok(None);
    };
    let may = eligible(
        current.state.is_closed(),
        changes::get(conn, parent)?.change.state.is_closed(),
        !current.conflicts.is_empty(),
        base == onto,
    );
    Ok(may.then_some(Step {
        current,
        base,
        onto,
    }))
}

/// Commits a step whose merge is done, if what it was computed from still
/// stands: the change exactly as read, still pending from the same basis
/// onto the same parent head. `false` when any of it moved, and nothing is
/// written.
fn apply(
    conn: &mut Connection,
    step: &Step,
    merged: history::merge::TreeMerge,
    sides: ConflictSides,
) -> Result<bool, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = step.current.id;
    let still: Option<(String, String)> = tx
        .query_row(&format!("{PENDING} AND c.id = ?1"), [id.to_string()], |r| {
            Ok((r.get(1)?, r.get(2)?))
        })
        .optional()?;
    let stands = match still {
        Some((base, onto)) => step_of(&tx, id, &base, &onto)?.is_some_and(|now| {
            now.current == step.current && now.base == step.base && now.onto == step.onto
        }),
        None => false,
    };
    if !stands {
        return Ok(false);
    }
    let (kind, detail) = if let Some(tree) = merged.tree {
        changes::update_in(&tx, id, |c| c.rebase(step.current.parent, tree))?;
        track(&tx, id, step.onto)?;
        (
            EventKind::Rebased,
            serde_json::json!({"tree":tree,"onto":step.onto,"regenerate":merged.regenerate}),
        )
    } else {
        store_conflicts(&tx, id, sides)?;
        (
            EventKind::Conflict,
            serde_json::json!({"paths":merged.conflicts}),
        )
    };
    // Staged with the update it announces, so both commit or neither does.
    crate::events::stage(&tx, &Staged::about(kind, id, detail))?;
    tx.commit()?;
    Ok(true)
}

/// Hands staged stack events to the event log, which stores each staged
/// row exactly once and keeps it staged until then (R9).
fn deliver(state: &ServerState) -> Result<(), ChangeError> {
    state.changes.with_conn(adopt_legacy_rows)?;
    state.events.deliver_staged();
    Ok(())
}

/// Moves rows a server from before the shared event outbox left in
/// `stack_delivery` into the outbox, in one transaction, so none is lost or
/// delivered twice. That table had its own delivery loop, which published a
/// row again on every pass while the event log still held it queued.
fn adopt_legacy_rows(conn: &mut Connection) -> Result<(), ChangeError> {
    let any: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM stack_delivery)", [], |r| {
        r.get(0)
    })?;
    if !any {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let rows = {
        let mut stmt =
            tx.prepare("SELECT change_id, conflicted, detail FROM stack_delivery ORDER BY seq")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (id, conflicted, detail) in rows {
        let kind = if conflicted {
            EventKind::Conflict
        } else {
            EventKind::Rebased
        };
        let detail = serde_json::from_str(&detail).map_err(failure)?;
        crate::events::stage(&tx, &Staged::about(kind, id, detail))?;
    }
    tx.execute("DELETE FROM stack_delivery", [])?;
    tx.commit()?;
    Ok(())
}

/// Each conflicting file's content in the three trees, read from the object
/// store before the step's transaction starts. Empty for a clean merge.
fn conflict_sides(
    ctx: &history::Ctx<'_>,
    trees: [ObjectId; 3],
    merged: &history::merge::TreeMerge,
) -> Result<ConflictSides, ChangeError> {
    if merged.tree.is_some() {
        return Ok(Vec::new());
    }
    let side = |tree: ObjectId, path: &str| -> Result<Vec<u8>, ChangeError> {
        let entry = file_at(&ctx.store, tree, path).map_err(failure)?;
        Ok(content(&ctx.store, entry)
            .map_err(failure)?
            .unwrap_or_default())
    };
    merged
        .conflicts
        .iter()
        .map(|path| {
            let sides = [
                side(trees[0], path)?,
                side(trees[1], path)?,
                side(trees[2], path)?,
            ];
            Ok((path.clone(), sides))
        })
        .collect()
}

fn store_conflicts(
    tx: &rusqlite::Transaction<'_>,
    id: ChangeId,
    sides: ConflictSides,
) -> Result<(), ChangeError> {
    let mut set = crate::conflicts::load(tx, &id.to_string()).map_err(failure)?;
    for (path, [base, ours, theirs]) in sides {
        let conflict = Conflict::new(&path, 1, base, ours, theirs);
        set.record_merge(&path, vec![conflict]);
    }
    crate::conflicts::save(tx, &id.to_string(), &set).map_err(failure)
}

fn failure(err: impl std::fmt::Display) -> ChangeError {
    ChangeError::Corrupt(err.to_string())
}

fn eligible(closed: bool, parent_closed: bool, conflicted: bool, same_basis: bool) -> bool {
    !closed && !parent_closed && !conflicted && !same_basis
}

#[test]
fn stack_selection_matches_proved_fixture() {
    // Risk (R8): replay must not rebase closed, conflicted or unchanged edges.
    for row in include_str!("../../../proofs/fixtures/stacked-changes.tsv")
        .lines()
        .skip(1)
        .filter(|row| !row.starts_with("merge\t"))
        .filter(|row| !row.starts_with("recovery\t"))
    {
        let f: Vec<_> = row.split('\t').collect();
        assert_eq!(
            eligible(f[0] == "1", f[1] == "1", f[2] == "1", f[3] == "1"),
            f[4] == "1"
        );
    }
}

/// What [`land_rebase`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LandRebase {
    Moved,
    /// Closed, already on `onto` or a later base, or stacked on an unlanded
    /// parent: nothing moved, so nothing is announced.
    Skipped,
    /// The live head conflicts with `onto`; nothing moved.
    Conflicted,
}

/// Rebases the live child onto a landed commit. The landing engine's preview
/// may predate an author snapshot, so its tree cannot be installed: the child
/// is read here, merged with nothing held, and the result is installed only
/// if the child is still exactly as it was read. A child that moved meanwhile
/// (its author's next snapshot) is read and merged again.
pub(crate) fn land_rebase(
    state: &ServerState,
    id: ChangeId,
    onto: ObjectId,
) -> Result<LandRebase, ChangeError> {
    use rosen_store::ObjectStore;
    let ctx = history::open(state).map_err(failure)?;
    let config = state
        .project_config
        .at_commit(&onto)
        .map_err(failure)?
        .config;
    let onto_tree = ctx.store.read_commit(&onto).map_err(failure)?.tree;
    let policy = |path: &str| {
        if config.is_generated(path) {
            rosen_merge::MergePolicy::Generated
        } else if config.is_registration_list(path) {
            rosen_merge::MergePolicy::RegistrationList
        } else {
            rosen_merge::MergePolicy::Text
        }
    };
    for _ in 0..LAND_REBASE_ATTEMPTS {
        let Some(child) = state.changes.with_conn(|conn| land_child(conn, id))? else {
            return Ok(LandRebase::Skipped);
        };
        let base = match child.basis {
            Basis::Tree(tree) => tree,
            Basis::Commit(commit) => {
                // Recovery can replay an older land after a later rebase.
                // Moving back would discard the intervening base additions.
                // Already on `onto`: a replay of this land moved it.
                if commit == onto || descends_from(&ctx.store, commit, onto)? {
                    return Ok(LandRebase::Skipped);
                }
                ctx.store.read_commit(&commit).map_err(failure)?.tree
            }
        };
        let merged =
            history::merge::merge_trees(&ctx, [base, onto_tree, child.current.head], &policy)
                .map_err(failure)?;
        let installed = state
            .changes
            .with_conn(|conn| install(conn, &child, onto, merged.tree))?;
        if let Some(outcome) = installed {
            return Ok(outcome);
        }
    }
    // The author's snapshots kept moving the child: this land leaves it on
    // its old base, and the next land or `rosen rebase` moves it.
    Ok(LandRebase::Skipped)
}

/// How often a land's rebase of one child is tried again because the child
/// moved while it was merged.
const LAND_REBASE_ATTEMPTS: u32 = 8;

/// A child change as a land's rebase read it.
struct LandChild {
    current: rosen_core::Change,
    basis: Basis,
}

/// What the child's edits are measured from.
#[derive(PartialEq, Eq)]
enum Basis {
    /// The base commit the child sits on.
    Commit(ObjectId),
    /// The tree of the landed parent it was last rebased onto.
    Tree(ObjectId),
}

/// The child and its basis; `None` when a land moves nothing for it: it is
/// closed, or stacked on a parent that has not landed.
fn land_child(conn: &Connection, id: ChangeId) -> Result<Option<LandChild>, ChangeError> {
    use rosen_core::ChangeParent;
    let current = changes::get(conn, id)?.change;
    if current.state.is_closed() {
        return Ok(None);
    }
    let basis = match current.parent {
        ChangeParent::Base(commit) => Basis::Commit(commit),
        ChangeParent::Change(parent) => {
            // Overlap alone cannot detach a child from its unlanded parent.
            if changes::get(conn, parent)?.change.state != rosen_core::ChangeState::Landed {
                return Ok(None);
            }
            Basis::Tree(basis(conn, id, parent)?)
        }
    };
    Ok(Some(LandChild { current, basis }))
}

/// Installs the merged tree, or reports the conflict, if the child is still
/// as `child` read it. `None` when it moved: nothing is written and the
/// caller reads it again.
fn install(
    conn: &mut Connection,
    child: &LandChild,
    onto: ObjectId,
    tree: Option<ObjectId>,
) -> Result<Option<LandRebase>, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = child.current.id;
    let stands = land_child(&tx, id)?
        .is_some_and(|now| now.current == child.current && now.basis == child.basis);
    if !stands {
        return Ok(None);
    }
    let Some(tree) = tree else {
        return Ok(Some(LandRebase::Conflicted));
    };
    changes::update_in(&tx, id, |c| {
        c.rebase(rosen_core::ChangeParent::Base(onto), tree)
    })?;
    tx.execute(
        "DELETE FROM stack_bases WHERE change_id = ?1",
        [id.to_string()],
    )?;
    tx.commit()?;
    Ok(Some(LandRebase::Moved))
}

fn descends_from(
    store: &dyn rosen_store::ObjectStore,
    mut commit: ObjectId,
    ancestor: ObjectId,
) -> Result<bool, ChangeError> {
    let mut seen = std::collections::HashSet::new();
    while seen.insert(commit) {
        if commit == ancestor {
            return Ok(true);
        }
        let Some(parent) = store
            .read_commit(&commit)
            .map_err(failure)?
            .parents
            .first()
            .copied()
        else {
            return Ok(false);
        };
        commit = parent;
    }
    Err(ChangeError::Corrupt("cycle in base history".into()))
}
