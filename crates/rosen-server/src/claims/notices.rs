//! Claim notices use cached immutable diffs, never object I/O under a lock.
//! Unlike the overlap query, notices include generated and registration paths.
use crate::changes::ChangeError;
use rosen_core::{AgentId, ChangeId, ObjectId, PathPattern};
use rosen_protocol::OverlapRecord;
use rosen_store::ObjectStore;
use rusqlite::{Connection, Transaction};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Parent {
    Commit(ObjectId),
    Tree(ObjectId),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct EditKey {
    parent: Parent,
    head: ObjectId,
}

struct Source {
    change: ChangeId,
    owner: AgentId,
    key: EditKey,
}

/// Request-local reuse lets a retry diff only heads that moved meanwhile.
#[derive(Default)]
pub(super) struct Edits {
    files: HashMap<EditKey, Vec<String>>,
    missing: HashSet<EditKey>,
}

impl Edits {
    /// Called only after the claim transaction and registry guard are gone.
    pub(super) fn load(&mut self, store: &dyn ObjectStore) -> Result<(), ChangeError> {
        for key in self.missing.drain() {
            let parent = match key.parent {
                Parent::Commit(id) => store.read_commit(&id).map_err(object_error)?.tree,
                Parent::Tree(id) => id,
            };
            let changed =
                crate::history::trees::changed_files(store, Some(parent), Some(key.head), &[])
                    .map_err(object_error)?;
            self.files
                .insert(key, changed.into_iter().map(|file| file.path).collect());
        }
        Ok(())
    }

    fn covers(&mut self, sources: &[Source]) -> bool {
        let live: HashSet<_> = sources.iter().map(|source| source.key).collect();
        self.files.retain(|key, _| live.contains(key));
        self.missing = live
            .into_iter()
            .filter(|key| !self.files.contains_key(key))
            .collect();
        self.missing.is_empty()
    }
}

/// None means retry after loading missing immutable diffs outside the lock.
pub(super) fn find(
    tx: &Transaction,
    edits: &mut Edits,
    claimant: ChangeId,
    patterns: &[PathPattern],
) -> Result<Option<Vec<OverlapRecord>>, ChangeError> {
    let sources = sources(tx, claimant)?;
    if !edits.covers(&sources) {
        return Ok(None);
    }
    let mut overlaps: BTreeMap<ChangeId, (AgentId, BTreeSet<String>)> = BTreeMap::new();
    for record in super::store::records(tx, claimant)? {
        let held = PathPattern::new(&record.path)?;
        if patterns.iter().any(|pattern| pattern.overlaps(&held)) {
            overlaps
                .entry(record.change)
                .or_insert_with(|| (record.owner, BTreeSet::new()))
                .1
                .insert(record.path);
        }
    }
    for source in sources {
        for path in &edits.files[&source.key] {
            if patterns.iter().any(|pattern| pattern.matches(path)) {
                overlaps
                    .entry(source.change)
                    .or_insert_with(|| (source.owner.clone(), BTreeSet::new()))
                    .1
                    .insert(path.clone());
            }
        }
    }
    Ok(Some(
        overlaps
            .into_iter()
            .map(|(change, (owner, paths))| OverlapRecord {
                change,
                owner,
                paths: paths.into_iter().collect(),
            })
            .collect(),
    ))
}

/// One SQL snapshot captures only diff inputs, without loading snapshot histories.
fn sources(conn: &Connection, claimant: ChangeId) -> Result<Vec<Source>, ChangeError> {
    let mut statement = conn.prepare_cached(
        "SELECT c.id, c.owner, c.head, c.parent_base, p.head
         FROM changes c LEFT JOIN changes p ON p.id = c.parent_change
         WHERE c.state IN ('open', 'ready') AND c.id <> ?1
           AND EXISTS (SELECT 1 FROM change_snapshots s WHERE s.change_id = c.id)",
    )?;
    let rows = statement.query_map([claimant.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (change, owner, head, base, parent) = row?;
        let parent = match (base, parent) {
            (Some(base), _) => Parent::Commit(parse_id(&base)?),
            (None, Some(head)) => Parent::Tree(parse_id(&head)?),
            _ => return Err(ChangeError::Corrupt("claim notice parent".into())),
        };
        Ok(Source {
            change: parse_id(&change)?,
            owner: AgentId::new(owner).map_err(object_error)?,
            key: EditKey {
                parent,
                head: parse_id(&head)?,
            },
        })
    })
    .collect()
}

fn parse_id<T: std::str::FromStr>(text: &str) -> Result<T, ChangeError> {
    text.parse()
        .map_err(|_| ChangeError::Corrupt(format!("claim notice id {text:?}")))
}

fn object_error(error: impl std::fmt::Display) -> ChangeError {
    ChangeError::Base(error.to_string())
}
