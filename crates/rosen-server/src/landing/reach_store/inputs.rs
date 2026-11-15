//! Content-aware reach. The graph always comes from the current base; edits
//! to configuration cannot choose the graph used to judge their own receipt.
use super::*;
use crate::landing::reach::Reach;

mod cargo;

pub(super) fn reach(
    store: &dyn ObjectStore,
    graph: &BuildGraph,
    old: &ObjectId,
    new: &ObjectId,
    prose: bool,
) -> Checked<Reach> {
    if prose {
        return Ok(Reach::new());
    }
    let paths = changed(store, old, new)?;
    let special = paths.iter().any(|p| special_path(p));
    let mut reached = Reach::new();
    let mut ordinary = Vec::new();
    let trees = if special {
        Some((
            files(store, root_tree(store, old)?)?,
            files(store, root_tree(store, new)?)?,
        ))
    } else {
        None
    };
    for path in paths {
        if graph.wide(&path) {
            return Ok(graph.all());
        }
        if special_path(&path) {
            let (before, after) = trees.as_ref().unwrap();
            let affected = if path == "rosen.toml" {
                config_reach(store, graph, before, after)?
            } else {
                cargo::reach(store, graph, before, after, &path)?
            };
            reached.extend(affected);
        } else {
            ordinary.push(path);
        }
    }
    reached.extend(graph.reach(&ordinary, false));
    Ok(graph.expand(reached))
}

fn special_path(path: &str) -> bool {
    matches!(path, "rosen.toml" | "Cargo.toml" | "Cargo.lock")
        || path.ends_with("/Cargo.toml")
        || path.ends_with("/Cargo.lock")
}

fn config(store: &dyn ObjectStore, files: &Files) -> Checked<ProjectConfig> {
    let Some((mode, id)) = files.get("rosen.toml") else {
        return Ok(ProjectConfig::default());
    };
    let text = text(store, *mode, id)?;
    ProjectConfig::parse(&text).map_err(unreadable)
}

fn config_reach(
    store: &dyn ObjectStore,
    graph: &BuildGraph,
    old: &Files,
    new: &Files,
) -> Checked<Reach> {
    let (old, new) = (config(store, old)?, config(store, new)?);
    let affects_build = old.checks != new.checks
        || old.build_units != new.build_units
        || old.workspace_inputs != new.workspace_inputs
        || old.generated != new.generated
        || old.registration_lists != new.registration_lists;
    Ok(if affects_build {
        graph.all()
    } else {
        Reach::new()
    })
}

fn text(store: &dyn ObjectStore, mode: rosen_core::FileMode, id: &ObjectId) -> Checked<String> {
    if !matches!(
        mode,
        rosen_core::FileMode::Regular | rosen_core::FileMode::Executable
    ) {
        return Err(unreadable("build input is not a regular file"));
    }
    let blob = store.read_blob(id).map_err(error)?;
    String::from_utf8(blob.data).map_err(unreadable)
}

fn document(store: &dyn ObjectStore, files: &Files, path: &str) -> Checked<Option<toml::Value>> {
    files
        .get(path)
        .map(|(mode, id)| toml::from_str(&text(store, *mode, id)?).map_err(unreadable))
        .transpose()
}
