//! One change's document (R9 state feed): what Collapse's Review tab and
//! change view show for a change the base does not hold yet.
//!
//! The live parts come from the same handlers the CLI calls, so the feed
//! shows exactly what `rosen land --check`, `rosen review list` and
//! `rosen overlaps` would. A part that cannot be read is left out and named
//! in `unavailable`, so one failure never holds back the rest.

use std::collections::BTreeMap;
use std::sync::Arc;

use rosen_core::ChangeId;
use rosen_protocol::{
    ClaimKind, DiffRequest, LineKind, Outcome, OverlapQuery, PathClaim, Response, ThreadFilter,
};
use serde_json::{json, Map, Value};

use super::facts::{self, Facts, Stores};
use super::unit::{Section, Unit};
use crate::claims::ClaimCall;
use crate::history::HistoryCall;
use crate::reviews::ReviewCall;
use crate::ServerState;

/// Builds the document for change `id`.
pub async fn build(
    state: &Arc<ServerState>,
    stores: &Stores,
    id: ChangeId,
) -> Result<Unit, String> {
    let reading = stores.clone();
    let facts = tokio::task::spawn_blocking(move || facts::read(&reading, id))
        .await
        .map_err(|join| join.to_string())??;
    if facts.record.change.state.is_closed() {
        return Ok(closed(facts));
    }
    let mut parts = Parts::default();
    parts.read(state, &id.to_string()).await;
    Ok(open(facts, parts))
}

/// A landed or abandoned change: sent once, with the landed commit.
fn closed(facts: Facts) -> Unit {
    let mut doc = core(&facts);
    doc.insert("landed".into(), json!(facts.landed));
    Unit::new(
        document(&facts),
        vec![(Section::Changes, Value::Object(doc))],
    )
}

fn open(facts: Facts, parts: Parts) -> Unit {
    let mut doc = core(&facts);
    let ids =
        |items: &[Value]| -> Vec<Value> { items.iter().map(|item| item["id"].clone()).collect() };
    doc.insert("land_check".into(), parts.land_check);
    doc.insert("conflicts".into(), Value::Array(ids(&facts.conflicts)));
    doc.insert("threads".into(), Value::Array(ids(&parts.threads)));
    doc.insert("approvals".into(), parts.approvals);
    doc.insert("receipt".into(), json!(facts.receipt));
    doc.insert("edits".into(), parts.edits);
    doc.insert("claims".into(), parts.claims);
    doc.insert("overlaps".into(), parts.overlaps);
    doc.insert("unavailable".into(), Value::Object(parts.unavailable));
    let mut items = vec![(Section::Changes, Value::Object(doc))];
    items.extend(
        facts
            .conflicts
            .iter()
            .cloned()
            .map(|c| (Section::Conflicts, c)),
    );
    items.extend(parts.threads.into_iter().map(|t| (Section::Threads, t)));
    let mut unit = Unit::new(document(&facts), items);
    unit.related = parts.related;
    unit
}

fn document(facts: &Facts) -> String {
    format!("change:{}", facts.record.change.id)
}

/// What every change document holds.
fn core(facts: &Facts) -> Map<String, Value> {
    let record = &facts.record;
    let change = &record.change;
    let owner = match &change.owner_machine {
        Some(machine) => format!("{}@{machine}", change.owner),
        None => change.owner.to_string(),
    };
    let doc = json!({
        "id": change.id,
        "owner": owner,
        "state": change.state.as_str(),
        "parent": change.parent,
        "head": change.head,
        "title": facts.title,
        "created_at": record.created_at,
        "updated_at": record.updated_at,
        "snapshots": { "named": facts.named, "count": change.snapshots.len() },
    });
    match doc {
        Value::Object(map) => map,
        _ => unreachable!("json! of an object literal is an object"),
    }
}

/// The parts read through the server's own handlers.
#[derive(Default)]
struct Parts {
    land_check: Value,
    approvals: Value,
    threads: Vec<Value>,
    edits: Value,
    claims: Value,
    overlaps: Value,
    related: std::collections::BTreeSet<String>,
    unavailable: Map<String, Value>,
}

impl Parts {
    async fn read(&mut self, state: &Arc<ServerState>, id: &str) {
        let check = crate::landing::service::land_check(state, id.to_owned()).await;
        if let Some(Response::LandCheck(check)) = self.answer("land_check", check) {
            self.land_check = json!(check);
        }
        let approvals = ReviewCall::Approvals(id.to_owned());
        let approvals = crate::reviews::handle(state, approvals).await;
        if let Some(Response::Approvals(approvals)) = self.answer("approvals", approvals) {
            self.approvals = json!(approvals);
        }
        let filter = ThreadFilter {
            change: id.to_owned(),
            state: None,
        };
        let threads = crate::reviews::handle(state, ReviewCall::List(filter)).await;
        if let Some(Response::ReviewThreads(threads)) = self.answer("threads", threads) {
            self.threads = threads.iter().map(|thread| json!(thread)).collect();
        }
        self.read_edits(state, id).await;
        self.read_claims(state, id).await;
    }

    /// Edit regions per path, with the lines added and removed against the
    /// parent (R6, R11).
    async fn read_edits(&mut self, state: &Arc<ServerState>, id: &str) {
        let regions = crate::overlaps::edits::handle(state, id.to_owned()).await;
        let Some(Response::EditRegions(regions)) = self.answer("edits", regions) else {
            return;
        };
        let diff = DiffRequest {
            from: None,
            to: Some(id.to_owned()),
            paths: Vec::new(),
            context: Some(0),
        };
        let diff = crate::history::handle(state, HistoryCall::Diff(diff)).await;
        let mut counts: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        if let Some(Response::Diff(diff)) = self.answer("line_counts", diff) {
            for file in diff.files {
                let count = counts.entry(file.path).or_default();
                for line in file.hunks.iter().flat_map(|hunk| &hunk.lines) {
                    match line.kind {
                        LineKind::Added => count.0 += 1,
                        LineKind::Removed => count.1 += 1,
                        LineKind::Context => {}
                    }
                }
            }
        }
        let edits = regions.files.into_iter().map(|file| {
            let (added, removed) = counts.get(&file.path).copied().unwrap_or_default();
            json!({ "path": file.path, "regions": file.regions, "added": added, "removed": removed })
        });
        self.edits = Value::Array(edits.collect());
    }

    /// The change's path claims (never piece claims) and the changes that
    /// overlap it.
    async fn read_claims(&mut self, state: &Arc<ServerState>, id: &str) {
        let claims = crate::claims::handle(state, ClaimCall::List(Some(id.to_owned()))).await;
        if let Some(Response::PathClaims(claims)) = self.answer("claims", claims) {
            self.claims = Value::Array(claims.iter().map(claim).collect());
        }
        let query = OverlapQuery {
            change: Some(id.to_owned()),
            paths: Vec::new(),
        };
        let overlaps = crate::overlaps::handle(state, query).await;
        if let Some(Response::Overlaps(answer)) = self.answer("overlaps", overlaps) {
            self.related = answer
                .overlaps
                .iter()
                .map(|overlap| overlap.change.to_string())
                .collect();
            self.overlaps = json!(answer.overlaps);
        }
    }

    /// The answer, or `None` with the reason recorded under `part`.
    fn answer(&mut self, part: &str, outcome: Outcome) -> Option<Response> {
        match outcome {
            Outcome::Ok(response) => Some(response),
            Outcome::Error(error) => {
                self.unavailable
                    .insert(part.to_owned(), Value::String(error.message));
                None
            }
        }
    }
}

fn claim(claim: &PathClaim) -> Value {
    let kind = match (claim.kind, claim.exclusive) {
        (ClaimKind::Claim, false) => "notice",
        (ClaimKind::Claim, true) => "exclusive",
        (ClaimKind::Share, _) => "share",
        (ClaimKind::Wait, _) => "wait",
    };
    json!({ "path": claim.path, "kind": kind, "since": claim.since })
}

/// The document for a change whose record could not be read, so one
/// unreadable change cannot stop the feed; the next event about it
/// rebuilds it.
pub fn unreadable(id: ChangeId, why: &str) -> Unit {
    let doc = json!({ "id": id, "unavailable": { "change": why } });
    Unit::new(format!("change:{id}"), vec![(Section::Changes, doc)])
}
