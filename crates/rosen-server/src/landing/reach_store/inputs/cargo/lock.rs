use super::*;

// Cargo permits several versions and sources of a package in one lockfile.
// Keep the full identity so an unrelated version's upgrade stays independent.
type PackageId = (String, String, String);
type Lock = BTreeMap<PackageId, toml::Value>;

pub(super) fn reach(
    store: &dyn ObjectStore,
    graph: &BuildGraph,
    old: &Files,
    new: &Files,
    before: &Packages,
    after: &Packages,
) -> Checked<Reach> {
    let (Some(old), Some(new)) = (
        document(store, old, "Cargo.lock")?,
        document(store, new, "Cargo.lock")?,
    ) else {
        return Ok(graph.all());
    };
    let (old, new) = (parse(old)?, parse(new)?);
    let changed: BTreeSet<_> = old
        .keys()
        .chain(new.keys())
        .filter(|id| old.get(*id) != new.get(*id))
        .cloned()
        .collect();
    let mut reached = Reach::new();
    for (lock, packages) in [(&old, before), (&new, after)] {
        if packages
            .keys()
            .any(|name| !lock.keys().any(|(n, _, s)| n == name && s.is_empty()))
        {
            return Err(unreadable("workspace package missing from Cargo.lock"));
        }
        let affected = consumers(lock, &changed)?;
        for (name, (path, _)) in packages {
            if affected
                .iter()
                .any(|(package, _, source)| package == name && source.is_empty())
            {
                reached.extend(unit_reach(graph, path));
            }
        }
    }
    Ok(reached)
}

fn parse(mut value: toml::Value) -> Checked<Lock> {
    let table = value
        .as_table_mut()
        .ok_or_else(|| unreadable("invalid Cargo.lock"))?;
    if table
        .get("version")
        .and_then(toml::Value::as_integer)
        .is_none_or(|v| !(1..=4).contains(&v))
    {
        return Err(unreadable("unsupported Cargo.lock version"));
    }
    let entries = table
        .remove("package")
        .and_then(|v| v.as_array().cloned())
        .ok_or_else(|| unreadable("Cargo.lock has no package entries"))?;
    table.remove("version");
    if !table.is_empty() {
        return Err(unreadable("unsupported Cargo.lock metadata"));
    }
    let mut lock = Lock::new();
    for entry in entries {
        let name = required(&entry, "name")?;
        let version = required(&entry, "version")?;
        let source = match entry.get("source") {
            Some(value) => value
                .as_str()
                .ok_or_else(|| unreadable("invalid lock source"))?,
            None => "",
        };
        if lock
            .insert((name.into(), version.into(), source.into()), entry)
            .is_some()
        {
            return Err(unreadable("duplicate lock package"));
        }
    }
    Ok(lock)
}

fn required<'a>(value: &'a toml::Value, key: &str) -> Checked<&'a str> {
    value
        .get(key)
        .and_then(toml::Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| unreadable(format!("missing lock package {key}")))
}

fn consumers(lock: &Lock, changed: &BTreeSet<PackageId>) -> Checked<BTreeSet<PackageId>> {
    let mut reverse: BTreeMap<PackageId, BTreeSet<PackageId>> = BTreeMap::new();
    for (id, package) in lock {
        let Some(deps) = package.get("dependencies") else {
            continue;
        };
        for dep in deps
            .as_array()
            .ok_or_else(|| unreadable("invalid lock dependencies"))?
        {
            let dep = dep
                .as_str()
                .ok_or_else(|| unreadable("invalid lock dependency"))?;
            for target in resolve(lock, dep)? {
                reverse.entry(target).or_default().insert(id.clone());
            }
        }
    }
    let mut affected = changed.clone();
    let mut pending: Vec<_> = changed.iter().cloned().collect();
    while let Some(id) = pending.pop() {
        for consumer in reverse.get(&id).into_iter().flatten() {
            if affected.insert(consumer.clone()) {
                pending.push(consumer.clone());
            }
        }
    }
    Ok(affected)
}

fn resolve(lock: &Lock, dependency: &str) -> Checked<Vec<PackageId>> {
    let parts: Vec<_> = dependency.split_whitespace().collect();
    let (name, version, source) = match parts.as_slice() {
        [name] => (*name, None, None),
        [name, version] => (*name, Some(*version), None),
        [name, version, source] if source.starts_with('(') && source.ends_with(')') => {
            (*name, Some(*version), Some(&source[1..source.len() - 1]))
        }
        _ => return Err(unreadable("invalid lock dependency identity")),
    };
    let matches: Vec<_> = lock
        .keys()
        .filter(|(n, v, s)| {
            n == name
                && version.is_none_or(|version| v == version)
                && source.is_none_or(|source| s == source)
        })
        .cloned()
        .collect();
    if matches.is_empty() {
        return Err(unreadable("unresolved lock dependency"));
    }
    Ok(matches)
}
