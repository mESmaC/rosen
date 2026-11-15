//! Cargo's implicit members must be shared by unit discovery and input reach;
//! otherwise an inherited dependency can disappear from one side of re-checking.
use super::*;

pub(super) fn members(
    store: &dyn ObjectStore,
    files: &Files,
    root: &toml::Value,
) -> Checked<BTreeSet<String>> {
    let mut members = BTreeSet::new();
    for path in files
        .keys()
        .filter(|p| *p == "Cargo.toml" || p.ends_with("/Cargo.toml"))
    {
        if path == "Cargo.toml"
            || (matches(root, "members", path)? && !matches(root, "exclude", path)?)
        {
            members.insert(path.clone());
        }
    }
    let Some(workspace) = root.get("workspace") else {
        return Ok(members);
    };
    if !workspace.is_table() {
        return Err(unreadable("invalid Cargo workspace"));
    }
    let mut pending: Vec<_> = members.iter().cloned().collect();
    while let Some(path) = pending.pop() {
        let (mode, id) = files
            .get(&path)
            .ok_or_else(|| unreadable("missing path dependency manifest"))?;
        if !matches!(
            mode,
            rosen_core::FileMode::Regular | rosen_core::FileMode::Executable
        ) {
            return Err(unreadable("Cargo manifest is not a regular file"));
        }
        let value = manifest(store, id)?;
        check_workspace(&value, &path)?;
        for target in path_dependencies(&value, root, &path)? {
            // Excluded, external and nested workspaces need a wider resolver.
            // Refuse to prove independence rather than omit a consumer edge.
            if matches(root, "exclude", &target)? {
                return Err(unreadable("excluded Cargo path dependency"));
            }
            if members.insert(target.clone()) {
                pending.push(target);
            }
        }
    }
    Ok(members)
}

fn matches(root: &toml::Value, key: &str, path: &str) -> Checked<bool> {
    let Some(values) = root.get("workspace").and_then(|w| w.get(key)) else {
        return Ok(false);
    };
    let dir = path.strip_suffix("/Cargo.toml").unwrap_or("");
    for value in values
        .as_array()
        .ok_or_else(|| unreadable("invalid workspace member list"))?
    {
        let pattern = value
            .as_str()
            .ok_or_else(|| unreadable("invalid workspace member"))?;
        if PathPattern::new(pattern).map_err(unreadable)?.matches(dir) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn check_workspace(value: &toml::Value, path: &str) -> Checked<()> {
    if path != "Cargo.toml" && value.get("workspace").is_some() {
        return Err(unreadable("nested Cargo workspace"));
    }
    if let Some(workspace) = value.get("package").and_then(|p| p.get("workspace")) {
        let target = workspace
            .as_str()
            .ok_or_else(|| unreadable("invalid package.workspace"))?;
        if !relative_dir(path, target)?.is_empty() {
            return Err(unreadable("package belongs to another Cargo workspace"));
        }
    }
    Ok(())
}

fn path_dependencies(value: &toml::Value, root: &toml::Value, path: &str) -> Checked<Vec<String>> {
    let mut paths = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(value) = value.get(key) {
            for (name, dep) in value
                .as_table()
                .ok_or_else(|| unreadable("invalid dependency table"))?
            {
                let (dep, origin) =
                    if dep.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                        let inherited = root
                            .get("workspace")
                            .and_then(|w| w.get("dependencies"))
                            .and_then(|deps| deps.get(name))
                            .ok_or_else(|| unreadable("missing inherited dependency"))?;
                        (inherited, "Cargo.toml")
                    } else {
                        (dep, path)
                    };
                if let Some(target) = dep.get("path") {
                    let target = target
                        .as_str()
                        .ok_or_else(|| unreadable("invalid dependency path"))?;
                    let dir = relative_dir(origin, target)?;
                    paths.push(if dir.is_empty() {
                        "Cargo.toml".into()
                    } else {
                        format!("{dir}/Cargo.toml")
                    });
                }
            }
        }
    }
    if let Some(targets) = value.get("target") {
        for target in targets
            .as_table()
            .ok_or_else(|| unreadable("invalid target table"))?
            .values()
        {
            paths.extend(path_dependencies(target, root, path)?);
        }
    }
    Ok(paths)
}

fn relative_dir(manifest: &str, dependency: &str) -> Checked<String> {
    let dependency = dependency.replace('\\', "/");
    if dependency.starts_with('/') || dependency.contains(':') {
        return Err(unreadable("absolute Cargo path dependency"));
    }
    let mut parts: Vec<_> = manifest
        .strip_suffix("Cargo.toml")
        .unwrap_or("")
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    for part in dependency.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts
                    .pop()
                    .ok_or_else(|| unreadable("Cargo path leaves the workspace"))?;
            }
            _ => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}
