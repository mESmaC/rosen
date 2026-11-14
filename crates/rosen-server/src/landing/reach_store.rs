//! Read build inputs from immutable base objects; never trust the candidate's
//! configuration or collapse intervening lands into a net diff (reverts count).
use super::reach::{independent, BuildGraph};
use super::{ChangeFacts, CommitId, LandError, LandResult, Tier};
use rosen_core::claim::PathPattern;
use rosen_core::project_config::BuildUnit;
use rosen_core::{Object, ObjectId, ProjectConfig};
use rosen_store::ObjectStore;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

type Files = BTreeMap<String, (rosen_core::FileMode, ObjectId)>;
type Units = BTreeMap<String, BuildUnit>;

/// The Cargo build units of the last base tree read. Consecutive bases
/// usually differ by one land, so the next base costs a tree diff instead of
/// a full walk and a reparse of every manifest (issue #108). Units are a pure
/// function of the tree, so a stale entry is only ever a cache miss.
#[derive(Default)]
pub struct UnitsCache(Mutex<Option<(ObjectId, Arc<Units>)>>);

impl UnitsCache {
    fn units(&self, store: &dyn ObjectStore, tree: ObjectId) -> Checked<Arc<Units>> {
        let cached = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let units = match cached {
            Some((at, units)) if at == tree => return Ok(units),
            Some((at, units)) if !touches_manifest(&changed(store, &at, &tree)?) => units,
            _ => Arc::new(cargo_units(store, &files(store, tree)?)?),
        };
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some((tree, Arc::clone(&units)));
        Ok(units)
    }
}

/// Whether a change can alter the Cargo build units: `cargo_units` reads
/// only the root and member manifests.
fn touches_manifest(paths: &[String]) -> bool {
    paths
        .iter()
        .any(|p| p == "Cargo.toml" || p.ends_with("/Cargo.toml"))
}

/// Why the check stopped before deciding.
#[derive(Debug)]
enum Stop {
    /// Reading an object failed; the land reports it as an error.
    Store(LandError),
    /// The trees hold something no build graph can be read from: a manifest
    /// that does not parse, two packages with one name, or a path that is
    /// not UTF-8.
    Unreadable(String),
}

impl From<LandError> for Stop {
    fn from(error: LandError) -> Self {
        Stop::Store(error)
    }
}

type Checked<T> = Result<T, Stop>;

/// Whether the change's receipt still counts after `lands` moved the base.
/// A build graph that cannot be read cannot show independence, so it needs a
/// new receipt (R8), which the agent can act on; an error would only tell
/// it that the land failed.
pub fn reusable(
    store: &dyn ObjectStore,
    cache: &UnitsCache,
    base: &CommitId,
    facts: &ChangeFacts,
    lands: &[(CommitId, Tier)],
) -> LandResult<bool> {
    match decide(store, cache, base, facts, lands) {
        Ok(reusable) => Ok(reusable),
        Err(Stop::Unreadable(why)) => {
            let change = facts.id;
            eprintln!("rosen-server: {change} needs a new receipt: unreadable build graph at {base}: {why}");
            Ok(false)
        }
        Err(Stop::Store(error)) => Err(error),
    }
}

fn decide(
    store: &dyn ObjectStore,
    cache: &UnitsCache,
    base: &CommitId,
    facts: &ChangeFacts,
    lands: &[(CommitId, Tier)],
) -> Checked<bool> {
    let config = crate::project_config::resolve(store, base)
        .map_err(error)?
        .config;
    let units = if config.build_units.is_empty() {
        cache
            .units(store, root_tree(store, base)?)?
            .as_ref()
            .clone()
    } else {
        config.build_units.clone()
    };
    let graph = graph(units, &config);
    let own = graph.reach(
        &changed(store, &facts.merge_base, &facts.head)?,
        facts.tier == Tier::ProseOnly,
    );
    for (commit, tier) in lands {
        if *tier == Tier::ProseOnly {
            continue;
        }
        let record = store.read_commit(commit).map_err(error)?;
        let Some(parent) = record.parents.first() else {
            return Ok(false);
        };
        let paths = changed(store, parent, commit)?;
        if !independent(&own, &graph.reach(&paths, false)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn error(e: impl std::fmt::Display) -> LandError {
    LandError::Backend(e.to_string())
}

fn unreadable(e: impl std::fmt::Display) -> Stop {
    Stop::Unreadable(e.to_string())
}

fn root_tree(store: &dyn ObjectStore, id: &ObjectId) -> Checked<ObjectId> {
    match store.read(id).map_err(error)? {
        Object::Commit(c) => Ok(c.tree),
        Object::Tree(_) => Ok(*id),
        _ => Err(error("build reach expects a commit or tree").into()),
    }
}

fn files(store: &dyn ObjectStore, tree: ObjectId) -> Checked<Files> {
    let mut out = Files::new();
    walk(store, tree, "", &mut out)?;
    Ok(out)
}

fn walk(store: &dyn ObjectStore, tree: ObjectId, prefix: &str, out: &mut Files) -> Checked<()> {
    for entry in store.read_tree(&tree).map_err(error)?.entries() {
        let name = std::str::from_utf8(&entry.name).map_err(unreadable)?;
        let path = format!("{prefix}{name}");
        if entry.mode.is_tree() {
            walk(store, entry.id, &format!("{path}/"), out)?;
        } else {
            out.insert(path, (entry.mode, entry.id));
        }
    }
    Ok(())
}

/// Paths of files that differ between two commits or trees. Subtrees with
/// equal ids are skipped, so the cost follows the size of the change rather
/// than of the repository (issue #108).
fn changed(store: &dyn ObjectStore, old: &ObjectId, new: &ObjectId) -> Checked<Vec<String>> {
    let (old, new) = (root_tree(store, old)?, root_tree(store, new)?);
    let mut out = BTreeSet::new();
    diff_trees(store, Some(old), Some(new), "", &mut out)?;
    Ok(out.into_iter().collect())
}

type Entry = (rosen_core::FileMode, ObjectId);

fn diff_trees(
    store: &dyn ObjectStore,
    old: Option<ObjectId>,
    new: Option<ObjectId>,
    prefix: &str,
    out: &mut BTreeSet<String>,
) -> Checked<()> {
    if old == new {
        return Ok(());
    }
    let (old, new) = (entries(store, old)?, entries(store, new)?);
    let names: BTreeSet<&Vec<u8>> = old.keys().chain(new.keys()).collect();
    let is_file = |e: Option<&Entry>| e.is_some_and(|(mode, _)| !mode.is_tree());
    // A file replaced by a directory, or the reverse, walks the directory
    // side against nothing, so every file under it counts as changed.
    let subtree = |e: Option<&Entry>| e.filter(|(mode, _)| mode.is_tree()).map(|(_, id)| *id);
    for name in names {
        let (before, after) = (old.get(name), new.get(name));
        if before == after {
            continue;
        }
        let path = format!("{prefix}{}", std::str::from_utf8(name).map_err(unreadable)?);
        if is_file(before) || is_file(after) {
            out.insert(path.clone());
        }
        diff_trees(
            store,
            subtree(before),
            subtree(after),
            &format!("{path}/"),
            out,
        )?;
    }
    Ok(())
}

fn entries(store: &dyn ObjectStore, tree: Option<ObjectId>) -> Checked<BTreeMap<Vec<u8>, Entry>> {
    let Some(tree) = tree else {
        return Ok(BTreeMap::new());
    };
    let tree = store.read_tree(&tree).map_err(error)?;
    Ok(tree
        .entries()
        .iter()
        .map(|e| (e.name.clone(), (e.mode, e.id)))
        .collect())
}

fn graph(units: Units, config: &ProjectConfig) -> BuildGraph {
    BuildGraph {
        units,
        workspace_inputs: config.workspace_inputs.clone(),
    }
}

fn manifest(store: &dyn ObjectStore, id: &ObjectId) -> Checked<toml::Value> {
    let blob = store.read_blob(id).map_err(error)?;
    let text = std::str::from_utf8(&blob.data).map_err(unreadable)?;
    toml::from_str(text).map_err(unreadable)
}

fn cargo_units(store: &dyn ObjectStore, files: &Files) -> Checked<Units> {
    let Some((_, root)) = files.get("Cargo.toml") else {
        return Ok(BTreeMap::new());
    };
    let root = manifest(store, root)?;
    let mut packages = BTreeMap::new();
    for (path, (_, id)) in files {
        if path != "Cargo.toml" && !path.ends_with("/Cargo.toml") {
            continue;
        }
        if !member(&root, path) {
            continue;
        }
        let value = manifest(store, id)?;
        if let Some(name) = value
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
        {
            if packages
                .insert(name.clone(), (path.clone(), value))
                .is_some()
            {
                return Err(unreadable(format!("two Cargo packages are named {name:?}")));
            }
        }
    }
    let mut units = BTreeMap::new();
    for (name, (path, value)) in &packages {
        let prefix = path.strip_suffix("Cargo.toml").unwrap();
        let pattern = format!("{prefix}**");
        let deps = dependencies(value, &root)
            .into_iter()
            .filter(|d| packages.contains_key(d))
            .collect();
        units.insert(
            name.clone(),
            BuildUnit {
                paths: vec![PathPattern::new(&pattern).map_err(unreadable)?],
                dependencies: deps,
            },
        );
    }
    Ok(units)
}

fn member(root: &toml::Value, path: &str) -> bool {
    if path == "Cargo.toml" {
        return true;
    }
    let Some(workspace) = root.get("workspace") else {
        return false;
    };
    let dir = path.strip_suffix("/Cargo.toml").unwrap_or("");
    let matches = |key: &str| {
        workspace
            .get(key)
            .and_then(|v| v.as_array())
            .is_some_and(|ps| {
                ps.iter()
                    .filter_map(|p| p.as_str())
                    .any(|p| PathPattern::new(p).is_ok_and(|p| p.matches(dir)))
            })
    };
    matches("members") && !matches("exclude")
}

fn dependencies(value: &toml::Value, root: &toml::Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = value.get(key).and_then(|v| v.as_table()) {
            for (name, dep) in table {
                let dep = if dep.get("workspace").and_then(|v| v.as_bool()) == Some(true) {
                    root.get("workspace")
                        .and_then(|w| w.get("dependencies"))
                        .and_then(|d| d.get(name))
                        .unwrap_or(dep)
                } else {
                    dep
                };
                out.push(
                    dep.get("package")
                        .and_then(|v| v.as_str())
                        .unwrap_or(name)
                        .to_owned(),
                );
            }
        }
    }
    if let Some(targets) = value.get("target").and_then(|v| v.as_table()) {
        for target in targets.values() {
            out.extend(dependencies(target, root));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rosen_core::{
        AgentId, ChangeId, ChangeParent, ChangeState, Commit, FileMode, Signature, Tree, TreeEntry,
    };
    use rosen_store::MemoryObjectStore;

    fn tree(store: &dyn ObjectStore, a: &str, b: &str, config: &str) -> ObjectId {
        let entries = [("a.rs", a), ("b.rs", b), ("rosen.toml", config)];
        let entries = entries
            .into_iter()
            .map(|(name, data)| TreeEntry {
                name: name.as_bytes().to_vec(),
                mode: FileMode::Regular,
                id: store.put_blob(data.as_bytes()).unwrap(),
            })
            .collect();
        store.put_tree(&Tree::new(entries).unwrap()).unwrap()
    }

    fn commit(store: &dyn ObjectStore, tree: ObjectId, parents: Vec<ObjectId>) -> ObjectId {
        let signature = Signature::new("test", "test@rosen", 0);
        store
            .put_commit(&Commit {
                tree,
                parents,
                author: signature.clone(),
                committer: signature,
                message: b"test\n".to_vec(),
                extra_headers: vec![],
            })
            .unwrap()
    }

    // Risk: reusing a receipt for independent file edits that feed a common
    // dependent build, or ignoring a reverted input, lands unchecked code (R8).
    #[test]
    fn stored_objects_recheck_complete_land_interval() {
        let store = MemoryObjectStore::new();
        let config = "[build_units.a]\npaths=['a.rs']\n[build_units.b]\npaths=['b.rs']\n";
        let root = commit(&store, tree(&store, "a", "b", config), vec![]);
        let landed = commit(&store, tree(&store, "a", "new b", config), vec![root]);
        let facts = ChangeFacts {
            id: ChangeId::from_bytes([1; 16]),
            owner: AgentId::new("test").unwrap(),
            description: "a".into(),
            state: ChangeState::Ready,
            parent: ChangeParent::Base(root),
            head: tree(&store, "new a", "b", config),
            merge_base: root,
            tier: Tier::Normal,
            has_conflicts: false,
            approved_head: None,
            approvers: vec![],
        };
        assert!(reusable(
            &store,
            &UnitsCache::default(),
            &landed,
            &facts,
            &[(landed, Tier::Normal)]
        )
        .unwrap());
        let shared =
            format!("{config}[build_units.app]\npaths=['app/**']\ndependencies=['a','b']\n");
        let base = commit(&store, tree(&store, "a", "b", &shared), vec![landed]);
        assert!(!reusable(
            &store,
            &UnitsCache::default(),
            &base,
            &facts,
            &[(landed, Tier::Normal)]
        )
        .unwrap());
        let changed_a = commit(&store, tree(&store, "changed a", "b", config), vec![root]);
        let reverted = commit(&store, tree(&store, "a", "b", config), vec![changed_a]);
        assert!(!reusable(
            &store,
            &UnitsCache::default(),
            &reverted,
            &facts,
            &[(changed_a, Tier::Normal), (reverted, Tier::Normal)]
        )
        .unwrap());
    }

    // Risk (R8, issue #107): a base whose build graph cannot be read failed
    // every moved-base land with an internal error, not `needs_recheck`.
    #[test]
    fn unreadable_build_graph_needs_a_new_receipt() {
        let store = MemoryObjectStore::new();
        let manifest = TreeEntry {
            name: b"Cargo.toml".to_vec(),
            mode: FileMode::Regular,
            id: store
                .put_blob(
                    b"[workspace
",
                )
                .unwrap(),
        };
        let tree = store.put_tree(&Tree::new(vec![manifest]).unwrap()).unwrap();
        let root = commit(&store, tree, vec![]);
        let landed = commit(&store, tree, vec![root]);
        let facts = ChangeFacts {
            id: ChangeId::from_bytes([1; 16]),
            owner: AgentId::new("test").unwrap(),
            description: "a".into(),
            state: ChangeState::Ready,
            parent: ChangeParent::Base(root),
            head: tree,
            merge_base: root,
            tier: Tier::Normal,
            has_conflicts: false,
            approved_head: None,
            approvers: vec![],
        };
        assert!(!reusable(
            &store,
            &UnitsCache::default(),
            &landed,
            &facts,
            &[(landed, Tier::Normal)]
        )
        .unwrap());
    }

    fn nested(store: &dyn ObjectStore, files: &[(&str, &str)]) -> ObjectId {
        let mut dirs: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
        let mut entries = vec![];
        for (path, text) in files {
            match path.split_once('/') {
                Some((dir, rest)) => dirs.entry(dir).or_default().push((rest, text)),
                None => entries.push(TreeEntry {
                    name: path.as_bytes().to_vec(),
                    mode: FileMode::Regular,
                    id: store.put_blob(text.as_bytes()).unwrap(),
                }),
            }
        }
        for (dir, inner) in dirs {
            entries.push(TreeEntry {
                name: dir.as_bytes().to_vec(),
                mode: FileMode::Directory,
                id: nested(store, &inner),
            });
        }
        store.put_tree(&Tree::new(entries).unwrap()).unwrap()
    }

    // Risk (issue #108): units cached for one base and reused for the next
    // after a member manifest changed deep in the tree would miss a new
    // dependency, so a receipt is reused for code it never built (R8).
    #[test]
    fn cached_units_follow_a_nested_manifest_change() {
        let store = MemoryObjectStore::new();
        let root = ("Cargo.toml", "[workspace]\nmembers=['crates/*']\n");
        let a = ("crates/a/Cargo.toml", "[package]\nname='a'\n");
        let b = ("crates/b/Cargo.toml", "[package]\nname='b'\n");
        let source = ("crates/b/src/lib.rs", "");
        let app = ("crates/app/Cargo.toml", "[package]\nname='app'\n");
        let uses_b = (
            app.0,
            "[package]\nname='app'\n[dependencies]\nb={path='../b'}\n",
        );
        let cache = UnitsCache::default();
        let before = nested(&store, &[root, a, b, source, app]);
        assert!(cache.units(&store, before).unwrap()["app"]
            .dependencies
            .is_empty());
        let edited = nested(&store, &[root, a, b, ("crates/b/src/lib.rs", "x"), app]);
        assert!(cache.units(&store, edited).unwrap()["app"]
            .dependencies
            .is_empty());
        let after = nested(&store, &[root, a, b, source, uses_b]);
        assert_eq!(
            cache.units(&store, after).unwrap()["app"].dependencies,
            ["b"]
        );
        assert_eq!(
            changed(&store, &before, &after).unwrap(),
            ["crates/app/Cargo.toml"]
        );
    }

    // Risk: missing inherited or target-specific dependencies makes unrelated
    // paths appear independent even though a common consumer builds both.
    #[test]
    fn cargo_manifests_include_inherited_and_target_dependencies() {
        let store = MemoryObjectStore::new();
        let sources = [
            ("Cargo.toml", "[workspace]\nmembers=['crates/*']\n[workspace.dependencies]\naliased={package='a',path='crates/a'}\n"),
            ("crates/a/Cargo.toml", "[package]\nname='a'\n"),
            ("crates/b/Cargo.toml", "[package]\nname='b'\n"),
            ("crates/app/Cargo.toml", "[package]\nname='app'\n[dependencies]\naliased={workspace=true}\n[target.'cfg(windows)'.build-dependencies]\nb={path='../b'}\n"),
        ];
        let files = sources
            .into_iter()
            .map(|(p, text)| {
                (
                    p.into(),
                    (FileMode::Regular, store.put_blob(text.as_bytes()).unwrap()),
                )
            })
            .collect();
        let graph = graph(
            cargo_units(&store, &files).unwrap(),
            &ProjectConfig::default(),
        );
        let a = graph.reach(&["crates/a/src/lib.rs".into()], false);
        let b = graph.reach(&["crates/b/src/lib.rs".into()], false);
        assert!(a.contains("app") && b.contains("app"));
        assert!(!independent(&a, &b));
        assert!(!independent(
            &a,
            &graph.reach(&["Cargo.lock".into()], false)
        ));
    }
}
