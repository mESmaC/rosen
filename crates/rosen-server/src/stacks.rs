//! R8: carry parent updates through a stack without rewriting authored commits.
use crate::changes::{self, ChangeError};
use crate::history::{
    self,
    trees::{content, file_at},
};
use crate::ServerState;
use rosen_core::{ChangeId, Conflict, ObjectId};
use rosen_protocol::EventKind;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

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
        // parent's current head can interpret intervening lands as deletions.
        None => {
            let child = changes::get(conn, id)?.change;
            if child.snapshots.is_empty() {
                Ok(child.head)
            } else {
                Err(ChangeError::Base(
                    "cannot safely rebase a legacy stack without its recorded parent tree".into(),
                ))
            }
        }
    }
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
    loop {
        let result = state.changes.with_conn(|conn| step(conn, &ctx, &policy))?;
        deliver(state)?;
        let Some(_) = result else {
            return Ok(());
        };
    }
}

type Notification = (ChangeId, EventKind, serde_json::Value);

fn step(
    conn: &mut Connection,
    ctx: &history::Ctx<'_>,
    policy: &dyn Fn(&str) -> rosen_merge::MergePolicy,
) -> Result<Option<Notification>, ChangeError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let pending: Option<(String, String, String)> = tx
        .query_row(
            "SELECT c.id, b.tree, p.head FROM changes c
         JOIN stack_bases b ON b.change_id = c.id
         JOIN changes p ON p.id = c.parent_change
         WHERE c.state IN ('open', 'ready') AND p.state IN ('open', 'ready')
           AND b.tree <> p.head
           AND NOT EXISTS (SELECT 1 FROM conflicts f WHERE f.change_id = c.id AND f.state = 'open')
         ORDER BY c.created_at, c.id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((id, base, onto)) = pending else {
        return Ok(None);
    };
    let id: ChangeId = id.parse().map_err(failure)?;
    let base: ObjectId = base.parse().map_err(failure)?;
    let onto: ObjectId = onto.parse().map_err(failure)?;
    let current = changes::get(&tx, id)?.change;
    let rosen_core::ChangeParent::Change(parent) = current.parent else {
        return Ok(None);
    };
    if !eligible(
        current.state.is_closed(),
        changes::get(&tx, parent)?.change.state.is_closed(),
        !current.conflicts.is_empty(),
        base == onto,
    ) {
        return Ok(None);
    }
    let merged =
        history::merge::merge_trees(ctx, [base, onto, current.head], policy).map_err(failure)?;
    finish_step(tx, ctx, current, base, onto, merged)
}

fn finish_step(
    tx: rusqlite::Transaction<'_>,
    ctx: &history::Ctx<'_>,
    current: rosen_core::Change,
    base: ObjectId,
    onto: ObjectId,
    merged: history::merge::TreeMerge,
) -> Result<Option<Notification>, ChangeError> {
    let id = current.id;
    let (kind, detail) = if let Some(tree) = merged.tree {
        changes::update_in(&tx, id, |c| c.rebase(current.parent, tree))?;
        track(&tx, id, onto)?;
        (
            EventKind::Rebased,
            serde_json::json!({"tree":tree,"onto":onto,"regenerate":merged.regenerate}),
        )
    } else {
        store_conflicts(&tx, ctx, id, [base, current.head, onto], &merged.conflicts)?;
        (
            EventKind::Conflict,
            serde_json::json!({"paths":merged.conflicts}),
        )
    };
    tx.execute(
        "INSERT INTO stack_delivery (change_id, conflicted, detail) VALUES (?1, ?2, ?3)",
        params![
            id.to_string(),
            kind == EventKind::Conflict,
            detail.to_string()
        ],
    )?;
    tx.commit()?;
    Ok(Some((id, kind, detail)))
}

fn deliver(state: &ServerState) -> Result<(), ChangeError> {
    let pending = state.changes.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT seq, change_id, conflicted, detail FROM stack_delivery ORDER BY seq",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    })?;
    for (seq, id, conflicted, detail) in pending {
        let kind = if conflicted {
            EventKind::Conflict
        } else {
            EventKind::Rebased
        };
        if state.events.publish(
            kind,
            Some(id),
            serde_json::from_str(&detail).map_err(failure)?,
        ) == 0
        {
            return Ok(());
        }
        state.changes.with_conn(|conn| {
            conn.execute("DELETE FROM stack_delivery WHERE seq = ?1", [seq])?;
            Ok(())
        })?;
    }
    Ok(())
}

fn store_conflicts(
    tx: &rusqlite::Transaction<'_>,
    ctx: &history::Ctx<'_>,
    id: ChangeId,
    trees: [ObjectId; 3],
    paths: &[String],
) -> Result<(), ChangeError> {
    let mut set = crate::conflicts::load(tx, &id.to_string()).map_err(failure)?;
    for path in paths {
        let mut sides = Vec::new();
        for tree in trees {
            let entry = file_at(&ctx.store, tree, path).map_err(failure)?;
            sides.push(
                content(&ctx.store, entry)
                    .map_err(failure)?
                    .unwrap_or_default(),
            );
        }
        set.record_merge(
            path,
            vec![Conflict::new(
                path,
                1,
                sides[0].clone(),
                sides[1].clone(),
                sides[2].clone(),
            )],
        );
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

/// Re-read the live child while holding the writer lock. The landing engine's
/// preview may predate an author snapshot, so its tree cannot be installed.
pub(crate) fn land_rebase(
    state: &ServerState,
    id: ChangeId,
    onto: ObjectId,
) -> Result<LandRebase, ChangeError> {
    use rosen_core::ChangeParent;
    use rosen_store::ObjectStore;
    let ctx = history::open(state).map_err(failure)?;
    let config = state
        .project_config
        .at_commit(&onto)
        .map_err(failure)?
        .config;
    let onto_tree = ctx.store.read_commit(&onto).map_err(failure)?.tree;
    state.changes.with_conn(|conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = changes::get(&tx, id)?.change;
        if current.state.is_closed() {
            return Ok(LandRebase::Skipped);
        }
        let base = match current.parent {
            ChangeParent::Base(commit) => {
                // Recovery can replay an older land after a later rebase.
                // Moving back would discard the intervening base additions.
                // Already on `onto`: a replay of this land moved it.
                if commit == onto || descends_from(&ctx.store, commit, onto)? {
                    return Ok(LandRebase::Skipped);
                }
                ctx.store.read_commit(&commit).map_err(failure)?.tree
            }
            ChangeParent::Change(parent) => {
                // Overlap alone cannot detach a child from its unlanded parent.
                if changes::get(&tx, parent)?.change.state != rosen_core::ChangeState::Landed {
                    return Ok(LandRebase::Skipped);
                }
                basis(&tx, id, parent)?
            }
        };
        let merged = history::merge::merge_trees(&ctx, [base, onto_tree, current.head], &|path| {
            if config.is_generated(path) {
                rosen_merge::MergePolicy::Generated
            } else if config.is_registration_list(path) {
                rosen_merge::MergePolicy::RegistrationList
            } else {
                rosen_merge::MergePolicy::Text
            }
        })
        .map_err(failure)?;
        let Some(tree) = merged.tree else {
            return Ok(LandRebase::Conflicted);
        };
        changes::update_in(&tx, id, |c| c.rebase(ChangeParent::Base(onto), tree))?;
        tx.execute(
            "DELETE FROM stack_bases WHERE change_id = ?1",
            [id.to_string()],
        )?;
        tx.commit()?;
        Ok(LandRebase::Moved)
    })
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
