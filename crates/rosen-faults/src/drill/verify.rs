//! Checks SPEC.md §5's guarantees after a faulted run, from the server's
//! own store and metadata rather than from anything a client says.

use super::agent::Ack;
use crate::failing;
use anyhow::anyhow;
use rosen_core::{ChangeState, Commit, LandedTrailers, ObjectId};
use rosen_server::changes::ChangeService;
use rosen_server::landing::Ledger;
use rosen_server::ServerConfig;
use rosen_store::{LooseObjectStore, ObjectStore};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    /// What broke, or what was counted when nothing did.
    pub detail: String,
}

/// One commit on the base's first-parent line.
struct Landed {
    id: ObjectId,
    commit: Commit,
    trailers: Option<LandedTrailers>,
}

impl Landed {
    /// A land that went through the gates: a receipt and an approval.
    fn gated(&self) -> bool {
        self.trailers
            .as_ref()
            .is_some_and(|t| t.receipt.is_some() && !t.approvers.is_empty())
    }
}

struct World {
    store: LooseObjectStore,
    /// Newest first, without the root.
    history: Vec<Landed>,
    tip_tree: ObjectId,
    changes: ChangeService,
}

pub fn verify(config: &ServerConfig, acks: &[(String, Ack)], failing: bool) -> anyhow::Result<Vec<Check>> {
    let world = load(config)?;
    let culprit = acks.iter().find_map(|(_, ack)| match ack {
        Ack::Culprit { change } => Some(change.clone()),
        _ => None,
    });
    let mut checks = vec![
        gated(&world, failing)?,
        applied_once(&world),
        acked_lands_kept(&world, acks),
        acked_edits_kept(&world, acks, culprit.as_deref())?,
    ];
    if failing {
        checks.push(culprit_reverted(&world, culprit.as_deref())?);
    }
    Ok(checks)
}

fn load(config: &ServerConfig) -> anyhow::Result<World> {
    let store = LooseObjectStore::open(config.objects_dir())?;
    let ledger = Ledger::open(&config.metadata_path()).map_err(|err| anyhow!("{err}"))?;
    let tip = ledger
        .base("main")
        .map_err(|err| anyhow!("{err}"))?
        .ok_or_else(|| anyhow!("the project has no base"))?;
    let tip_tree = store.read_commit(&tip)?.tree;
    let mut history = Vec::new();
    let mut next = Some(tip);
    while let Some(id) = next {
        let commit = store.read_commit(&id)?;
        next = commit.parents.first().copied();
        if next.is_some() {
            let trailers = LandedTrailers::parse(&commit.message);
            history.push(Landed { id, commit, trailers });
        }
    }
    let changes = ChangeService::new(&config.data_dir);
    Ok(World {
        store,
        history,
        tip_tree,
        changes,
    })
}

/// The base never moves without a receipt and an approval, or as a server
/// revert of the injected failure.
fn gated(world: &World, failing: bool) -> anyhow::Result<Check> {
    let mut ungated = Vec::new();
    let mut reverts = 0;
    for (index, landed) in world.history.iter().enumerate() {
        if landed.gated() {
            continue;
        }
        let parent = world.history.get(index + 1).map(|p| p.commit.tree);
        if failing && is_revert(world, landed, parent)? {
            reverts += 1;
        } else {
            ungated.push(landed.id.to_string());
        }
    }
    Ok(Check {
        name: "base_moves_only_with_receipt_and_approval",
        ok: ungated.is_empty(),
        detail: if ungated.is_empty() {
            format!("{} lands, {reverts} server reverts", world.history.len())
        } else {
            format!("ungated commits: {}", ungated.join(", "))
        },
    })
}

/// A revert of the injected failure: it removes the failure file its first
/// parent held. The oldest commit's parent is the root, which never holds it.
fn is_revert(world: &World, landed: &Landed, parent_tree: Option<ObjectId>) -> anyhow::Result<bool> {
    let Some(parent_tree) = parent_tree else {
        return Ok(false);
    };
    Ok(failing::fails(&world.store, &parent_tree)? && !failing::fails(&world.store, &landed.commit.tree)?)
}

/// No change's land is applied twice.
fn applied_once(world: &World) -> Check {
    let mut seen = HashMap::new();
    for landed in world.history.iter().filter(|l| l.gated()) {
        let change = landed.trailers.as_ref().map(|t| t.change.to_string());
        *seen.entry(change.unwrap_or_default()).or_insert(0) += 1;
    }
    let twice: Vec<_> = seen.iter().filter(|(_, n)| **n > 1).map(|(c, _)| c.clone()).collect();
    Check {
        name: "no_land_applied_twice",
        ok: twice.is_empty(),
        detail: if twice.is_empty() {
            format!("{} distinct changes landed", seen.len())
        } else {
            format!("landed more than once: {}", twice.join(", "))
        },
    }
}

/// Every land an agent was told succeeded is on the base.
fn acked_lands_kept(world: &World, acks: &[(String, Ack)]) -> Check {
    let on_base: HashSet<String> = world.history.iter().map(|l| l.id.to_string()).collect();
    let mut acked = 0;
    let mut lost = Vec::new();
    for (agent, ack) in acks {
        if let Ack::Landed { change, commit } = ack {
            acked += 1;
            if !on_base.contains(commit) {
                lost.push(format!("{agent} {change} {commit}"));
            }
        }
    }
    Check {
        name: "no_acknowledged_land_lost",
        ok: lost.is_empty(),
        detail: if lost.is_empty() {
            format!("{acked} acknowledged lands on the base")
        } else {
            format!("acknowledged but not on the base: {}", lost.join("; "))
        },
    }
}

/// Every write the server acknowledged is still somewhere it belongs: on
/// the base once its change landed, otherwise in its change's head. The
/// reverted culprit's edits belong in its reopened change.
fn acked_edits_kept(world: &World, acks: &[(String, Ack)], culprit: Option<&str>) -> anyhow::Result<Check> {
    let mut checked = 0;
    let mut lost = Vec::new();
    for (agent, ack) in acks {
        let Ack::Wrote { change, path, data } = ack else {
            continue;
        };
        checked += 1;
        let record = world.changes.show(change).map_err(|err| anyhow!("{change}: {err}"))?;
        let tree = match record.change.state {
            ChangeState::Landed if Some(change.as_str()) != culprit => world.tip_tree,
            ChangeState::Abandoned => {
                lost.push(format!("{agent} {change} was abandoned"));
                continue;
            }
            _ => record.change.head,
        };
        if read_path(&world.store, tree, path)?.as_deref() != Some(data.as_bytes()) {
            lost.push(format!("{agent} {change} {path} ({:?})", record.change.state));
        }
    }
    Ok(Check {
        name: "no_acknowledged_edit_lost",
        ok: lost.is_empty(),
        detail: if lost.is_empty() {
            format!("{checked} acknowledged writes found")
        } else {
            format!("lost: {}", lost.join("; "))
        },
    })
}

/// The injected failure was bisected and reverted, and its change reopened
/// with its work (SPEC.md §5, R12).
fn culprit_reverted(world: &World, culprit: Option<&str>) -> anyhow::Result<Check> {
    let mut problems = Vec::new();
    let Some(culprit) = culprit else {
        return Ok(Check {
            name: "failing_commit_reverted",
            ok: false,
            detail: "the culprit agent never wrote the failing commit".into(),
        });
    };
    let landed = world
        .history
        .iter()
        .any(|l| l.gated() && l.trailers.as_ref().is_some_and(|t| t.change.to_string() == culprit));
    if !landed {
        problems.push("the culprit never landed".to_owned());
    }
    if failing::fails(&world.store, &world.tip_tree)? {
        problems.push("the base still fails its post-land check".to_owned());
    }
    let record = world.changes.show(culprit).map_err(|err| anyhow!("{culprit}: {err}"))?;
    if record.change.state != ChangeState::Open {
        problems.push(format!("the culprit is {:?}, not reopened", record.change.state));
    }
    if !failing::fails(&world.store, &record.change.head)? {
        problems.push("the reopened change lost its work".to_owned());
    }
    Ok(Check {
        name: "failing_commit_reverted",
        ok: problems.is_empty(),
        detail: if problems.is_empty() {
            format!("{culprit} reverted and reopened with its work")
        } else {
            problems.join("; ")
        },
    })
}

/// The bytes at a slash-separated path in a tree, if it holds a file there.
fn read_path(store: &LooseObjectStore, tree: ObjectId, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let mut current = tree;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        let tree = store.read_tree(&current)?;
        let Some(entry) = tree.get(part.as_bytes()) else {
            return Ok(None);
        };
        if parts.peek().is_none() {
            return Ok((!entry.mode.is_tree()).then(|| store.read_blob(&entry.id).map(|b| b.data)).transpose()?);
        }
        current = entry.id;
    }
    Ok(None)
}
