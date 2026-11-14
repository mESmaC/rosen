//! Overlap detection (SPEC §2 "Overlap", R6): which live changes overlap a
//! change or a set of paths.
//!
//! Two changes overlap when they claim the same path, or their edits in one
//! file touch or sit within 3 lines of each other. A claim covers every line
//! of the paths it names, so a claim also overlaps another change's edit of
//! a path it matches. Shares count as claims, since a sharer edits the path
//! too; waits do not, since a waiter holds off. Generated files and
//! registration lists from the base's configuration never count.
//!
//! Edits are read fresh from each change's head against its parent, so they
//! follow every snapshot and rebase. Ignored paths are never uploaded (R2),
//! so they never appear among them.

pub mod edits;

use crate::changes::{self, ChangeError};
use crate::history::HistoryError;
use crate::ServerState;
use edits::ChangedFile;
use rosen_core::{
    AgentId, ChangeId, ChangeParent, ChangeState, ObjectId, PathPattern, ProjectConfig,
};
use rosen_protocol::{
    ChangeFilter, ChangeRecord, ClaimKind, ErrorBody, ErrorCode, Outcome, OverlapAnswer,
    OverlapQuery, OverlapRecord, Response,
};
use rosen_store::{LooseObjectStore, ObjectStore, StoreError};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

/// What to find overlaps for.
#[derive(Debug, Clone)]
pub enum Subject {
    /// A change, by full ID or unique prefix: its claims, shares and edits.
    Change(String),
    /// Paths or globs, as if a change claimed them.
    Paths(Vec<PathPattern>),
}

/// One live change that overlaps the subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    pub change: ChangeId,
    pub owner: AgentId,
    /// The paths and patterns the overlap is on, sorted.
    pub paths: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum OverlapError {
    #[error(transparent)]
    Change(#[from] ChangeError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("reading the project configuration: {0}")]
    Config(String),
    #[error("change {0} is stacked on change {1}, which the store lacks")]
    MissingParent(ChangeId, ChangeId),
    #[error("{0}")]
    Invalid(String),
}

impl OverlapError {
    fn code(&self) -> ErrorCode {
        match self {
            OverlapError::Change(err) => crate::rpc::change_error_code(err),
            OverlapError::History(err) => err.code(),
            OverlapError::Invalid(_) => ErrorCode::InvalidArgument,
            OverlapError::Store(_) | OverlapError::Config(_) | OverlapError::MissingParent(..) => {
                ErrorCode::Internal
            }
        }
    }
}

/// Answers [`rosen_protocol::Request::Overlaps`] for the channel and Velocity.
/// Reading every live change's edits is SQLite and store I/O, so it runs on
/// the blocking pool.
pub async fn handle(state: &Arc<ServerState>, query: OverlapQuery) -> Outcome {
    let state = Arc::clone(state);
    let joined = tokio::task::spawn_blocking(move || answer(&state, &query)).await;
    match joined {
        Ok(Ok(answer)) => Outcome::Ok(Response::Overlaps(answer)),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(err.code(), err.to_string())),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

fn answer(state: &ServerState, query: &OverlapQuery) -> Result<OverlapAnswer, OverlapError> {
    let patterns = query
        .paths
        .iter()
        .map(|text| PathPattern::new(text).map_err(|err| OverlapError::Invalid(err.to_string())))
        .collect::<Result<Vec<_>, _>>()?;
    let (subject, change) = match &query.change {
        Some(text) => {
            let id = state
                .changes
                .with_conn(|conn| changes::resolve(conn, text))?;
            (Subject::Change(id.to_string()), Some(id))
        }
        None if patterns.is_empty() => {
            return Err(OverlapError::Invalid(
                "an overlap query names a change or at least one path".into(),
            ))
        }
        None => (Subject::Paths(patterns.clone()), None),
    };
    let mut found = find(state, &subject)?;
    if change.is_some() && !patterns.is_empty() {
        narrow(&mut found, &patterns);
    }
    let overlaps = found
        .into_iter()
        .map(|overlap| OverlapRecord {
            change: overlap.change,
            owner: overlap.owner,
            paths: overlap.paths,
        })
        .collect();
    Ok(OverlapAnswer { change, overlaps })
}

/// Keeps only the overlap paths some pattern matches, then drops changes
/// left with none. An overlap path may be a claimed glob, so it is matched
/// as a pattern: `src/*.rs` stays under a filter of `src/lib.rs`.
fn narrow(found: &mut Vec<Overlap>, patterns: &[PathPattern]) {
    for overlap in found.iter_mut() {
        overlap.paths.retain(|path| {
            PathPattern::new(path)
                .is_ok_and(|ours| patterns.iter().any(|filter| filter.overlaps(&ours)))
        });
    }
    found.retain(|overlap| !overlap.paths.is_empty());
}

/// Every open or ready change other than the subject that overlaps it, in ID
/// order, under the configuration in force at the base.
pub fn find(state: &ServerState, subject: &Subject) -> Result<Vec<Overlap>, OverlapError> {
    let config = match state.project_config.at_base() {
        Ok(Some((_, resolved))) => resolved.config,
        Ok(None) => Default::default(),
        Err(why) => return Err(OverlapError::Config(why)),
    };
    let store = LooseObjectStore::open(state.config.objects_dir())?;
    let (own_id, records, heads, claims) = state.changes.with_conn(|conn| {
        let own_id = match subject {
            Subject::Change(text) => Some(changes::resolve(conn, text)?),
            Subject::Paths(_) => None,
        };
        let mut records = live_changes(conn)?;
        // The subject may be closed, as a change that just landed is when
        // landing asks which changes to rebase.
        if let Some(id) = own_id.filter(|id| !records.iter().any(|r| r.change.id == *id)) {
            records.push(changes::get(conn, id)?);
        }
        let heads = parent_heads(conn, &records)?;
        let mut claims = crate::claims::live_records(conn)?;
        // CAS closes the subject before post-land work. Its retained claims
        // still select rebase targets until release, even though they no
        // longer block new claimants.
        if let Some(id) = own_id {
            let retained = crate::claims::records_for(conn, id)?;
            for claim in retained {
                if !claims.contains(&claim) {
                    claims.push(claim);
                }
            }
        }
        Ok((own_id, records, heads, claims))
    })?;
    let mut claimed: HashMap<ChangeId, Vec<PathPattern>> = HashMap::new();
    for record in claims.into_iter().filter(|r| r.kind != ClaimKind::Wait) {
        let pattern = PathPattern::new(&record.path)
            .map_err(|_| ChangeError::Corrupt(format!("path claim {:?}", record.path)))?;
        claimed.entry(record.change).or_default().push(pattern);
    }
    let finder = Finder {
        store: &store,
        config: &config,
        uncounted: config.uncounted_patterns(),
        heads,
    };
    let ours = match (own_id, subject) {
        (Some(id), _) => {
            let record = records.iter().find(|r| r.change.id == id);
            let record = record.ok_or_else(|| ChangeError::NotFound(id.to_string()))?;
            finder.footprint(record, &claimed)?
        }
        (None, Subject::Paths(patterns)) => finder.footprint_of_paths(patterns),
        (None, Subject::Change(text)) => return Err(ChangeError::NotFound(text.clone()).into()),
    };
    finder.find(own_id, &ours, &records, &claimed)
}

fn live_changes(conn: &mut Connection) -> Result<Vec<ChangeRecord>, ChangeError> {
    let mut records = Vec::new();
    for state in [ChangeState::Open, ChangeState::Ready] {
        let filter = ChangeFilter {
            owner: None,
            state: Some(state),
        };
        records.extend(changes::list(conn, &filter)?);
    }
    Ok(records)
}

/// The head of every change one of `records` is stacked on, which is the
/// tree its edits are measured against.
fn parent_heads(
    conn: &mut Connection,
    records: &[ChangeRecord],
) -> Result<HashMap<ChangeId, ObjectId>, ChangeError> {
    let mut heads: HashMap<ChangeId, ObjectId> = records
        .iter()
        .map(|r| (r.change.id, r.change.head))
        .collect();
    for record in records {
        if let ChangeParent::Change(below) = record.change.parent {
            if let std::collections::hash_map::Entry::Vacant(slot) = heads.entry(below) {
                slot.insert(changes::get(conn, below)?.change.head);
            }
        }
    }
    Ok(heads)
}

/// Overlap checks against one snapshot of the store and the changes.
struct Finder<'a> {
    store: &'a dyn ObjectStore,
    config: &'a ProjectConfig,
    /// Generated files and registration lists: two claims meeting only on
    /// these paths do not overlap.
    uncounted: Vec<PathPattern>,
    /// Live changes' heads and their parents', so a stacked change finds its
    /// parent's tree.
    heads: HashMap<ChangeId, ObjectId>,
}

/// One side of a comparison: the patterns it claims and the files it edited.
struct Footprint {
    claims: Vec<PathPattern>,
    edits: BTreeMap<String, ChangedFile>,
}

impl Finder<'_> {
    fn find(
        &self,
        own_id: Option<ChangeId>,
        ours: &Footprint,
        records: &[ChangeRecord],
        claimed: &HashMap<ChangeId, Vec<PathPattern>>,
    ) -> Result<Vec<Overlap>, OverlapError> {
        let mut found = Vec::new();
        for record in records {
            let change = &record.change;
            if change.state.is_closed() || Some(change.id) == own_id {
                continue;
            }
            let theirs = self.footprint(record, claimed)?;
            let paths = self.shared(ours, &theirs)?;
            if !paths.is_empty() {
                found.push(Overlap {
                    change: change.id,
                    owner: change.owner.clone(),
                    paths: paths.into_iter().collect(),
                });
            }
        }
        found.sort_by_key(|overlap| overlap.change);
        Ok(found)
    }

    fn footprint_of_paths(&self, patterns: &[PathPattern]) -> Footprint {
        Footprint {
            claims: patterns.to_vec(),
            edits: BTreeMap::new(),
        }
    }

    fn footprint(
        &self,
        record: &ChangeRecord,
        claimed: &HashMap<ChangeId, Vec<PathPattern>>,
    ) -> Result<Footprint, OverlapError> {
        let mut edits = edits::changed_files(self.store, &self.heads, &record.change)?;
        edits.retain(|path, _| self.config.counts_for_overlap(path));
        // Claims are kept whole: a claim's uncounted paths are set aside where
        // it is compared, since a glob can cover counted paths too.
        let claims = claimed.get(&record.change.id).cloned().unwrap_or_default();
        Ok(Footprint { claims, edits })
    }

    /// The paths where two footprints overlap.
    fn shared(
        &self,
        ours: &Footprint,
        theirs: &Footprint,
    ) -> Result<BTreeSet<String>, OverlapError> {
        let mut paths = BTreeSet::new();
        // Edits on uncounted paths were dropped in `footprint`, so only the
        // claim-to-claim test needs the uncounted patterns.
        for pattern in &ours.claims {
            let meets = |other: &PathPattern| other.overlaps_outside(pattern, &self.uncounted);
            if theirs.claims.iter().any(meets) {
                paths.insert(pattern.to_string());
            }
            paths.extend(theirs.edits.keys().filter(|p| pattern.matches(p)).cloned());
        }
        for (path, edit) in &ours.edits {
            if theirs.claims.iter().any(|pattern| pattern.matches(path)) {
                paths.insert(path.clone());
            } else if let Some(other) = theirs.edits.get(path) {
                if edits::edits_overlap(self.store, edit, other)? {
                    paths.insert(path.clone());
                }
            }
        }
        Ok(paths)
    }
}
