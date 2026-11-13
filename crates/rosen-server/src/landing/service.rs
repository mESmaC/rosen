//! The server's land request (SPEC R8 step 2): the landing engine wired to
//! the change registry, the tree merge checkers build, the object store and
//! the event log.
//!
//! The engine holds no lock across a land. It merges outside any lock and
//! moves the base by one compare-and-swap transaction in the metadata store,
//! so lands of unrelated changes proceed in parallel and only lose a race,
//! never wait in a queue.

use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};

use rosen_core::{AgentId, Change, ChangeParent, ChangeState, SnapshotKind};
use rosen_protocol::{ErrorBody, ErrorCode, LandAnswer, LandStatus, Outcome, Response};
use rosen_store::{LooseObjectStore, ObjectStore};
use rusqlite::params;

use super::review_gate::StoredReviews;
use super::{
    ChangeBook, ChangeFacts, ChangeId, CommitId, CommitWriter, EventSink, LandError, LandOutcome,
    LandResult, LandedCommit, Landing, LandingConfig, LandingDeps, LandingEvent, Ledger,
    MergeOutcome, Merger, Refusal, Tier, TreeId,
};
use crate::changes::{self, ChangeError};
use crate::events::LandingEvents;
use crate::rpc::change_error_code;
use crate::ServerState;

/// Approvals and review tiers, as landing reads them (SPEC R7). The server
/// reads stored approvals ([`StoredReviews`]); tests and the tiering piece
/// install their own with [`LandingService::set_review_gate`].
pub trait ReviewGate: Send + Sync {
    /// The head the change's approvals cover, and who approved it there. A
    /// land needs it to equal the change's current head.
    fn approval(&self, change: &Change) -> Result<Option<(CommitId, Vec<AgentId>)>, String>;
    fn tier(&self, change: &Change) -> Result<Tier, String>;
}

pub struct LandingService {
    state: Weak<ServerState>,
    /// Built on the first land. Rebuilt only when the base's configuration
    /// renames the pre-land check set, which receipts are matched by.
    engine: Mutex<Option<Arc<Landing>>>,
    gate: RwLock<Arc<dyn ReviewGate>>,
}

/// Answers `land` on the blocking pool: merging and SQLite I/O must not
/// stall the async workers that carry every other connection.
pub async fn land(state: &Arc<ServerState>, change: String) -> Outcome {
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || state.landing.land(&state, &change)).await {
        Ok(Ok(answer)) => Outcome::Ok(Response::Land(answer)),
        Ok(Err(body)) => Outcome::Error(body),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

impl LandingService {
    pub fn new(state: Weak<ServerState>) -> Self {
        Self {
            gate: RwLock::new(Arc::new(StoredReviews(state.clone()))),
            state,
            engine: Mutex::new(None),
        }
    }

    pub fn set_review_gate(&self, gate: Arc<dyn ReviewGate>) {
        *self.gate.write().unwrap_or_else(PoisonError::into_inner) = gate;
    }

    fn gate(&self) -> Arc<dyn ReviewGate> {
        Arc::clone(&self.gate.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Lands `change`, a change ID or unique prefix. An open change is marked
    /// ready first: asking to land is what `ready` means (SPEC §2).
    pub fn land(&self, state: &ServerState, change: &str) -> Result<LandAnswer, ErrorBody> {
        let record = state.changes.show(change).map_err(change_body)?;
        let id = record.change.id;
        if record.change.state == ChangeState::Open {
            let ready = |c: &mut Change| c.mark_ready();
            state
                .changes
                .with_conn(|conn| changes::update(conn, id, ready))
                .map_err(change_body)?;
        }
        let engine = self.engine(state)?;
        let outcome = engine.land(&id).map_err(land_body)?;
        let base_ref = engine.config().base_ref.clone();
        let base = engine
            .ledger()
            .base(&base_ref)
            .map_err(land_body)?
            .ok_or_else(|| land_body(LandError::NoBase(base_ref.clone())))?;
        let outcome = match outcome {
            LandOutcome::Landed {
                commit,
                attempts,
                restack,
            } => {
                // The land stands either way; recovery replays the rebase.
                if let Err(why) = restack {
                    eprintln!("rosen-server: post-land work for {id} failed: {why}");
                }
                LandStatus::Landed { commit, attempts }
            }
            LandOutcome::AlreadyLanded { commit } => LandStatus::AlreadyLanded { commit },
            LandOutcome::Refused(refusal) => {
                refusal_status(state, &engine, &record.change, &base, refusal)?
            }
        };
        Ok(LandAnswer {
            change: id,
            base_ref,
            base,
            outcome,
        })
    }

    /// Replays post-land work a crash interrupted (R8 step 4). Does nothing
    /// before the project has a base.
    pub fn recover(&self, state: &ServerState) -> Result<usize, ErrorBody> {
        super::revert::recover(state).map_err(land_body)?;
        if state.project_config.at_base().map_err(internal)?.is_none() {
            return Ok(0);
        }
        self.engine(state)?.recover().map_err(land_body)
    }

    fn engine(&self, state: &ServerState) -> Result<Arc<Landing>, ErrorBody> {
        let (base_ref, in_force) = state
            .project_config
            .at_base()
            .map_err(internal)?
            .ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::Refused,
                    "the project has no base yet; import a repository with `rosen import`",
                )
            })?;
        // The check set comes from the base's configuration, so a change
        // cannot rename the check set its own receipt must cover.
        let check_set = in_force.config.checks.pre_land.name.clone();
        let mut slot = self.engine.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(engine) = slot.as_ref() {
            let config = engine.config();
            if config.base_ref == base_ref && config.pre_land_check_set == check_set {
                return Ok(Arc::clone(engine));
            }
        }
        let ledger = Ledger::open(&state.config.metadata_path()).map_err(land_body)?;
        let adapters = || Box::new(Adapters(self.state.clone()));
        let deps = LandingDeps {
            book: adapters(),
            merger: adapters(),
            writer: adapters(),
            events: adapters(),
        };
        let config = LandingConfig {
            base_ref,
            pre_land_check_set: check_set,
            ..LandingConfig::default()
        };
        let store = LooseObjectStore::open(state.config.objects_dir())
            .map_err(|err| internal(err.to_string()))?;
        let engine =
            Arc::new(Landing::new(ledger, deps, config).with_recheck_store(Arc::new(store)));
        *slot = Some(Arc::clone(&engine));
        Ok(engine)
    }
}

/// A refusal, with what the agent must do next.
fn refusal_status(
    state: &ServerState,
    engine: &Landing,
    change: &Change,
    base: &CommitId,
    refusal: Refusal,
) -> Result<LandStatus, ErrorBody> {
    let set = &engine.config().pre_land_check_set;
    let head = change.head;
    let submit = format!(
        "a checker runs the `{set}` check set on the merge result and submits \
         `rosen receipt submit {} --head {head} --base {base}`",
        change.id
    );
    let mut paths = Vec::new();
    let message = match refusal {
        Refusal::AlreadyLanded => "the change has already landed".to_owned(),
        Refusal::Abandoned => "the change is abandoned".to_owned(),
        Refusal::ParentUnlanded => {
            "the change below it in its stack has not landed; a stack lands bottom first".into()
        }
        Refusal::Conflicts => {
            paths = super::conflict_routes::paths(state, change.id).map_err(land_body)?;
            "the change has stored conflicts; resolve them with `rosen resolve`".into()
        }
        Refusal::Unapproved => format!(
            "no approval for the current head {head}; a reviewer reads that head and runs \
             `rosen review approve {} --head {head}`. A new head from the author voids an approval",
            change.id
        ),
        Refusal::NoReceipt => format!("the change has no receipt: {submit}"),
        Refusal::ReceiptStale => format!(
            "no receipt is bound to the current head {head} and the `{set}` check set: {submit}"
        ),
        Refusal::NeedsRecheck => format!(
            "the base moved past the receipt's base with lands that may affect this change, \
             so the receipt no longer counts: {submit}"
        ),
        Refusal::MergeConflict => {
            paths = super::conflict_routes::paths(state, change.id).map_err(land_body)?;
            format!("merging onto {base} conflicts in: {}", paths.join(", "))
        }
    };
    Ok(LandStatus::Refused {
        reason: refusal.code().to_owned(),
        message,
        paths,
    })
}

/// The landing engine's view of this server: the change registry, the tree
/// merge, the object store and the event log. Holds the server weakly
/// because the server holds the engine.
struct Adapters(Weak<ServerState>);

impl Adapters {
    fn state(&self) -> LandResult<Arc<ServerState>> {
        self.0
            .upgrade()
            .ok_or_else(|| LandError::Backend("the server is stopping".into()))
    }

    fn with_changes<T>(
        &self,
        work: impl FnOnce(&mut rusqlite::Connection) -> Result<T, ChangeError>,
    ) -> LandResult<T> {
        self.state()?.changes.with_conn(work).map_err(backend)
    }
}

impl ChangeBook for Adapters {
    fn facts(&self, id: &ChangeId) -> LandResult<Option<ChangeFacts>> {
        let state = self.state()?;
        let change = match state.changes.with_conn(|c| changes::get(c, *id)) {
            Ok(record) => record.change,
            Err(ChangeError::NotFound(_)) => return Ok(None),
            Err(err) => return Err(backend(err)),
        };
        let merge_base = match change.parent {
            ChangeParent::Base(commit) => commit,
            ChangeParent::Change(below) => {
                self.with_changes(|c| changes::get(c, below))?.change.head
            }
        };
        let gate = state.landing.gate();
        let approval = gate.approval(&change).map_err(LandError::Backend)?;
        let (approved_head, approvers) = approval.map_or((None, Vec::new()), |(h, a)| (Some(h), a));
        Ok(Some(ChangeFacts {
            id: *id,
            owner: change.owner.clone(),
            description: description(&change),
            state: change.state,
            parent: change.parent,
            head: change.head,
            merge_base,
            tier: gate.tier(&change).map_err(LandError::Backend)?,
            has_conflicts: !change.conflicts.is_empty(),
            approved_head,
            approvers,
        }))
    }

    fn stacked_on(&self, id: &ChangeId) -> LandResult<Vec<ChangeId>> {
        self.with_changes(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id FROM changes WHERE parent_change = ?1
                 AND state IN ('open', 'ready') ORDER BY created_at, id",
            )?;
            let ids: Vec<String> = stmt
                .query_map(params![id.to_string()], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            ids.iter()
                .map(|text| text.parse().map_err(|_| ChangeError::Corrupt(text.clone())))
                .collect()
        })
    }

    fn overlapping(&self, id: &ChangeId) -> LandResult<Vec<ChangeId>> {
        super::overlap_targets::load_or_capture(&*self.state()?, *id)
    }

    fn mark_landed(&self, id: &ChangeId, _: &CommitId) -> LandResult<()> {
        // Idempotent, because a crash replays post-land work.
        let land = |change: &mut Change| match change.state {
            ChangeState::Landed => Ok(()),
            ChangeState::Open => change.mark_ready().and_then(|()| change.mark_landed()),
            _ => change.mark_landed(),
        };
        self.with_changes(|conn| changes::update(conn, *id, land))
            .map(drop)
    }

    fn record_conflicts(&self, _: &ChangeId, _: &[String]) -> LandResult<()> {
        Err(LandError::Backend(
            "conflict routing requires merge inputs".into(),
        ))
    }

    fn record_conflicts_at(
        &self,
        id: &ChangeId,
        merge_base: &CommitId,
        onto: &CommitId,
        head: &CommitId,
        paths: &[String],
    ) -> LandResult<super::Routing> {
        super::conflict_routes::record(&*self.state()?, *id, *merge_base, *onto, *head, paths)
    }

    fn reparent(&self, id: &ChangeId, onto: &CommitId, merged: Option<&TreeId>) -> LandResult<()> {
        // A conflicted change stays on its old parent, so its head never
        // holds a half-merged tree; it merges again when it lands.
        let Some(tree) = merged else {
            return Ok(());
        };
        let parent = ChangeParent::Base(*onto);
        self.with_changes(|conn| changes::rebase(conn, *id, parent, *tree))
            .map(drop)
    }

    fn release_claims(&self, id: &ChangeId) -> LandResult<()> {
        crate::claims::release_landed(&*self.state()?, *id)
            .map(drop)
            .map_err(backend)
    }
}

impl Merger for Adapters {
    fn merge(
        &self,
        merge_base: &CommitId,
        onto: &CommitId,
        head: &CommitId,
    ) -> LandResult<MergeOutcome> {
        let state = self.state()?;
        let merged = crate::history::merge_for_land(&state, merge_base, onto, head)
            .map_err(|err| LandError::Backend(err.to_string()))?;
        Ok(match merged.tree {
            Some(tree) => MergeOutcome::Clean(tree),
            None => MergeOutcome::Conflicted(merged.conflicts),
        })
    }
}

impl CommitWriter for Adapters {
    fn write_landed(&self, commit: &LandedCommit) -> LandResult<CommitId> {
        let store = LooseObjectStore::open(self.state()?.config.objects_dir())
            .map_err(|err| LandError::Backend(err.to_string()))?;
        store
            .put_commit(&commit.to_commit())
            .map_err(|err| LandError::Backend(err.to_string()))
    }
}

impl EventSink for Adapters {
    fn emit(&self, event: LandingEvent) {
        // A stopping server has no subscribers left to tell.
        if let Ok(state) = self.state() {
            LandingEvents(state).emit(event);
        }
    }
}

/// The landed commit's message: the change's newest `rosen commit` message.
fn description(change: &Change) -> String {
    change
        .snapshots
        .iter()
        .rev()
        .find_map(|snapshot| match &snapshot.kind {
            SnapshotKind::Named(message) => Some(message.clone()),
            SnapshotKind::Automatic => None,
        })
        .unwrap_or_else(|| format!("Change {}", change.id))
}

fn backend(err: ChangeError) -> LandError {
    LandError::Backend(err.to_string())
}

fn change_body(err: ChangeError) -> ErrorBody {
    ErrorBody::new(change_error_code(&err), err.to_string())
}

fn internal(why: String) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, why)
}

fn land_body(err: LandError) -> ErrorBody {
    let code = match &err {
        LandError::UnknownChange(_) => ErrorCode::NotFound,
        LandError::InvalidReceipt(_) => ErrorCode::InvalidArgument,
        LandError::NoBase(_) | LandError::Contended { .. } => ErrorCode::Refused,
        LandError::Store(_) | LandError::Backend(_) => ErrorCode::Internal,
    };
    ErrorBody::new(code, err.to_string())
}
