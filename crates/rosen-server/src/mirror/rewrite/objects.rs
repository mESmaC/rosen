//! Prepare replacement objects before atomically publishing metadata.
use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use rosen_core::{Commit, ObjectId, Tree};
use rosen_store::{LooseObjectStore, ObjectStore};

use crate::history::trees;

#[derive(Default)]
pub struct Rewrite {
    pub commits: Vec<(ObjectId, ObjectId)>,
    trees: HashMap<(ObjectId, String), ObjectId>,
    paths: Vec<String>,
}

impl Rewrite {
    pub fn prepare(
        store: &LooseObjectStore,
        base: ObjectId,
        boundary: Option<ObjectId>,
        paths: Vec<String>,
    ) -> Result<Self> {
        let commits = unpublished(store, base, boundary)?;
        for path in &paths {
            let mut found = false;
            for (_, commit) in &commits {
                found |= trees::entry_at(store, commit.tree, path)?.is_some();
            }
            if !found {
                bail!("path {path:?} does not exist in the unmirrored commits; \
                    use `rosen log --change base` and inspect those commits with the history API to find \
                    its repository-relative spelling, then retry with that file or directory. \
                    For a path only in published history, use a normal deletion change; \
                    removing published content needs a separately coordinated mirror rewrite");
            }
        }
        let mut rewrite = Self {
            commits: Vec::new(),
            trees: HashMap::new(),
            paths,
        };
        let mut mapped = HashMap::new();
        for (old, mut commit) in commits {
            let original_tree = commit.tree;
            let original_parents = commit.parents.clone();
            commit.tree = rewrite.tree(store, commit.tree)?;
            for parent in &mut commit.parents {
                *parent = mapped.get(parent).copied().unwrap_or(*parent);
            }
            // These attest to the old bytes and cannot sign a rewritten commit.
            if commit.tree != original_tree || commit.parents != original_parents {
                commit.extra_headers.retain(|(key, _)| {
                    !matches!(key.as_slice(), b"gpgsig" | b"gpgsig-sha256" | b"mergetag")
                });
            }
            let new = store.put_commit(&commit)?;
            mapped.insert(old, new);
            if old != new {
                rewrite.commits.push((old, new));
            }
        }
        Ok(rewrite)
    }

    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    pub fn tree(&mut self, store: &LooseObjectStore, tree: ObjectId) -> Result<ObjectId> {
        self.directory(store, tree, "")
    }

    fn directory(
        &mut self,
        store: &LooseObjectStore,
        tree: ObjectId,
        prefix: &str,
    ) -> Result<ObjectId> {
        let key = (tree, prefix.to_owned());
        if let Some(id) = self.trees.get(&key) {
            return Ok(*id);
        }
        let original = store.read_tree(&tree)?;
        let mut entries = Vec::new();
        for mut entry in original.entries().iter().cloned() {
            // Match bytes rather than a lossy conversion: imported non-UTF8
            // names must not cause an operator to remove a different path.
            let mut path = prefix.as_bytes().to_vec();
            path.extend_from_slice(&entry.name);
            if self.paths.iter().any(|p| p.as_bytes() == path) {
                continue;
            }
            if entry.mode.is_tree() {
                if let Ok(path) = std::str::from_utf8(&path) {
                    let dir = format!("{path}/");
                    if self.paths.iter().any(|p| p.starts_with(&dir)) {
                        entry.id = self.directory(store, entry.id, &dir)?;
                        if store.read_tree(&entry.id)?.entries().is_empty() {
                            continue;
                        }
                    }
                }
            }
            entries.push(entry);
        }
        let id = if entries == original.entries() {
            tree
        } else {
            store.put_tree(&Tree::new(entries)?)?
        };
        self.trees.insert(key, id);
        Ok(id)
    }
}

/// Full DAG, parents first, excluding everything reachable from the last
/// pushed commit. First-parent-only walking can leave a bad merge parent
/// reachable and keep every subsequent mirror push rejected.
fn unpublished(
    store: &LooseObjectStore,
    base: ObjectId,
    boundary: Option<ObjectId>,
) -> Result<Vec<(ObjectId, Commit)>> {
    let mut excluded = HashSet::new();
    let mut pending: Vec<_> = boundary.into_iter().collect();
    while let Some(id) = pending.pop() {
        if excluded.insert(id) {
            pending.extend(store.read_commit(&id)?.parents);
        }
    }
    let mut visited = HashSet::new();
    let mut stack = vec![(base, false)];
    let mut out = Vec::new();
    let mut found_boundary = boundary.is_none();
    while let Some((id, expanded)) = stack.pop() {
        if Some(id) == boundary {
            found_boundary = true;
        }
        if excluded.contains(&id) {
            continue;
        }
        let commit = store
            .read_commit(&id)
            .with_context(|| format!("reading {id}"))?;
        if expanded {
            out.push((id, commit));
        } else if visited.insert(id) {
            stack.push((id, true));
            stack.extend(commit.parents.iter().rev().map(|id| (*id, false)));
        }
    }
    if !found_boundary {
        bail!("last pushed commit is not an ancestor of the base; restore the expected mirror configuration before repairing history");
    }
    Ok(out)
}

pub fn paths(paths: Vec<String>) -> Result<Vec<String>> {
    if paths.is_empty() {
        bail!("name at least one repository-relative file or directory");
    }
    let mut out = Vec::new();
    for path in paths {
        let path = path.replace('\\', "/");
        let path = path.trim_end_matches('/');
        if path.is_empty()
            || path.starts_with('/')
            || path.contains(':')
            || path.split('/').any(|p| matches!(p, "" | "." | ".."))
        {
            bail!("invalid path {path:?}; use a literal repository-relative file or directory, with no . or .. segments");
        }
        if !out.iter().any(|p| p == path) {
            out.push(path.to_owned());
        }
    }
    Ok(out)
}
