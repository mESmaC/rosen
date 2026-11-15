//! Cargo changes are measured in both immutable trees. No Cargo invocation,
//! registry access, or candidate-supplied build graph is needed for reach.
use super::*;

mod lock;

type Packages = BTreeMap<String, (String, toml::Value)>;

pub(super) fn reach(
    store: &dyn ObjectStore,
    graph: &BuildGraph,
    old: &Files,
    new: &Files,
    path: &str,
) -> Checked<Reach> {
    let before = packages(store, old)?;
    let after = packages(store, new)?;
    if path == "Cargo.lock" {
        return lock::reach(store, graph, old, new, &before, &after);
    }
    if path.ends_with("/Cargo.lock") {
        return Ok(graph.all());
    }
    if path == "Cargo.toml" {
        return root_reach(store, graph, old, new, &before, &after);
    }
    let mut reached = Reach::new();
    for packages in [&before, &after] {
        for (manifest, _) in packages.values() {
            if manifest == path {
                reached.extend(unit_reach(graph, manifest));
            }
        }
    }
    // A manifest outside the discovered workspace may still feed a build.
    Ok(if reached.is_empty() {
        graph.all()
    } else {
        reached
    })
}

fn packages(store: &dyn ObjectStore, files: &Files) -> Checked<Packages> {
    let root = document(store, files, "Cargo.toml")?
        .ok_or_else(|| unreadable("Cargo input without a root manifest"))?;
    validate_workspace(&root)?;
    let members = cargo_workspace::members(store, files, &root)?;
    let mut out = Packages::new();
    for path in files
        .keys()
        .filter(|p| *p == "Cargo.toml" || p.ends_with("/Cargo.toml"))
    {
        let value = document(store, files, path)?.unwrap();
        validate_dependencies(&value)?;
        if !members.contains(path) {
            continue;
        }
        if let Some(package) = value.get("package") {
            let name = package
                .get("name")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| unreadable("Cargo package without a name"))?;
            if out.insert(name.into(), (path.clone(), value)).is_some() {
                return Err(unreadable("duplicate Cargo package"));
            }
        }
    }
    Ok(out)
}

fn unit_reach(graph: &BuildGraph, path: &str) -> Reach {
    // Configured names are arbitrary, and several units can own one path.
    graph.reach(&[path.to_owned()], false)
}

fn workspace_dependencies(value: &toml::Value) -> Checked<toml::Table> {
    match value.get("workspace").and_then(|v| v.get("dependencies")) {
        None => Ok(toml::Table::new()),
        Some(value) => value
            .as_table()
            .cloned()
            .ok_or_else(|| unreadable("workspace.dependencies is not a table")),
    }
}

fn without_workspace_dependencies(mut value: toml::Value) -> toml::Value {
    if let Some(table) = value
        .get_mut("workspace")
        .and_then(toml::Value::as_table_mut)
    {
        table.remove("dependencies");
        if table.is_empty() {
            value.as_table_mut().unwrap().remove("workspace");
        }
    }
    value
}

fn root_reach(
    store: &dyn ObjectStore,
    graph: &BuildGraph,
    old: &Files,
    new: &Files,
    before: &Packages,
    after: &Packages,
) -> Checked<Reach> {
    let (Some(old), Some(new)) = (
        document(store, old, "Cargo.toml")?,
        document(store, new, "Cargo.toml")?,
    ) else {
        return Ok(graph.all());
    };
    let (old_deps, new_deps) = (workspace_dependencies(&old)?, workspace_dependencies(&new)?);
    let mut reached = root_settings_reach(graph, old, new, before, after);
    let changed: BTreeSet<_> = old_deps
        .keys()
        .chain(new_deps.keys())
        .filter(|name| old_deps.get(*name) != new_deps.get(*name))
        .collect();
    for packages in [before, after] {
        for (path, value) in packages.values() {
            if inherited_dependencies(value)?
                .iter()
                .any(|alias| changed.contains(alias))
            {
                reached.extend(unit_reach(graph, path));
            }
        }
    }
    Ok(reached)
}

fn root_settings_reach(
    graph: &BuildGraph,
    old: toml::Value,
    new: toml::Value,
    before: &Packages,
    after: &Packages,
) -> Reach {
    let (mut old, mut new) = (
        without_workspace_dependencies(old),
        without_workspace_dependencies(new),
    );
    if old == new {
        return Reach::new();
    }
    for value in [&mut old, &mut new] {
        for key in [
            "package",
            "dependencies",
            "dev-dependencies",
            "build-dependencies",
            "target",
            "lib",
            "bin",
            "example",
            "test",
            "bench",
            "features",
            "lints",
            "badges",
        ] {
            value.as_table_mut().unwrap().remove(key);
        }
    }
    // Workspace membership, shared package metadata, profiles and patches
    // affect more than the root package and stay conservative.
    if old != new {
        return graph.all();
    }
    let mut reached = Reach::new();
    for packages in [before, after] {
        for (path, _) in packages.values() {
            if path == "Cargo.toml" {
                reached.extend(unit_reach(graph, path));
            }
        }
    }
    if reached.is_empty() {
        graph.all()
    } else {
        reached
    }
}

fn dependency_tables(value: &toml::Value) -> Checked<Vec<&toml::Table>> {
    let mut tables = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(value) = value.get(key) {
            tables.push(
                value
                    .as_table()
                    .ok_or_else(|| unreadable("invalid dependency table"))?,
            );
        }
    }
    if let Some(targets) = value.get("target") {
        for target in targets
            .as_table()
            .ok_or_else(|| unreadable("invalid target table"))?
            .values()
        {
            tables.extend(dependency_tables(target)?);
        }
    }
    Ok(tables)
}

fn validate_dependencies(value: &toml::Value) -> Checked<()> {
    for table in dependency_tables(value)? {
        for dep in table.values() {
            validate_dependency(dep)?;
        }
    }
    Ok(())
}

fn validate_dependency(dep: &toml::Value) -> Checked<()> {
    if !dep.is_str() && !dep.is_table() {
        return Err(unreadable("invalid Cargo dependency"));
    }
    for key in [
        "package", "version", "path", "git", "branch", "tag", "rev", "registry",
    ] {
        if dep.get(key).is_some_and(|v| !v.is_str()) {
            return Err(unreadable(format!("invalid dependency {key}")));
        }
    }
    if dep
        .get("workspace")
        .is_some_and(|v| v.as_bool() != Some(true))
    {
        return Err(unreadable("invalid inherited dependency"));
    }
    Ok(())
}

fn validate_workspace(root: &toml::Value) -> Checked<()> {
    if let Some(workspace) = root.get("workspace") {
        if !workspace.is_table() {
            return Err(unreadable("invalid Cargo workspace"));
        }
        for key in ["members", "exclude"] {
            if let Some(value) = workspace.get(key) {
                for member in value
                    .as_array()
                    .ok_or_else(|| unreadable("invalid workspace members"))?
                {
                    let pattern = member
                        .as_str()
                        .ok_or_else(|| unreadable("invalid workspace member"))?;
                    PathPattern::new(pattern).map_err(unreadable)?;
                }
            }
        }
    }
    for dep in workspace_dependencies(root)?.values() {
        validate_dependency(dep)?;
    }
    Ok(())
}

fn inherited_dependencies(value: &toml::Value) -> Checked<BTreeSet<String>> {
    Ok(dependency_tables(value)?
        .into_iter()
        .flat_map(|table| table.iter())
        .filter(|(_, dep)| dep.get("workspace").and_then(toml::Value::as_bool) == Some(true))
        .map(|(name, _)| name.clone())
        .collect())
}
