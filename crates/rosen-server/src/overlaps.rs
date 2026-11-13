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

mod edits;

use crate::changes::{self, ChangeError};
use crate::history::HistoryError;
use crate::ServerState;
use edits::EditedFile;
use rosen_core::{
    AgentId, ChangeId, ChangeParent, ChangeState, ObjectId, PathPattern, ProjectConfig,
};
use rosen_protocol::{ChangeFilter, ChangeRecord, ClaimKind};
use rosen_store::{LooseObjectStore, ObjectStore, StoreError};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet, HashMap};

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
    /// Live changes' heads and their parents', so a stacked change finds its
    /// parent's tree.
    heads: HashMap<ChangeId, ObjectId>,
}

/// One side of a comparison: the patterns it claims and the files it edited.
struct Footprint {
    claims: Vec<PathPattern>,
    edits: BTreeMap<String, EditedFile>,
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
            claims: self.counting(patterns.iter().cloned()),
            edits: BTreeMap::new(),
        }
    }

    fn footprint(
        &self,
        record: &ChangeRecord,
        claimed: &HashMap<ChangeId, Vec<PathPattern>>,
    ) -> Result<Footprint, OverlapError> {
        let mut edits = edits::edited_files(self.store, &self.heads, &record.change)?;
        edits.retain(|path, _| self.config.counts_for_overlap(path));
        let claims = claimed.get(&record.change.id).cloned().unwrap_or_default();
        Ok(Footprint {
            claims: self.counting(claims.into_iter()),
            edits,
        })
    }

    /// Drops claims on a single generated file or registration list. A glob
    /// is kept: it covers other paths that do count.
    fn counting(&self, patterns: impl Iterator<Item = PathPattern>) -> Vec<PathPattern> {
        patterns
            .filter(|pattern| self.config.counts_for_overlap(pattern.as_str()))
            .collect()
    }

    /// The paths where two footprints overlap.
    fn shared(
        &self,
        ours: &Footprint,
        theirs: &Footprint,
    ) -> Result<BTreeSet<String>, OverlapError> {
        let mut paths = BTreeSet::new();
        for pattern in &ours.claims {
            if theirs.claims.iter().any(|other| other.overlaps(pattern)) {
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
