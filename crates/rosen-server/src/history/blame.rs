//! Blame: which commit, or which unlanded change, last changed each line.
//!
//! At a commit, blame follows first parents back to the nearest commit whose
//! blame of the file is cached, or to where the file was added, then carries
//! line origins forward through each commit that changed the file, caching
//! the blame at each. Commits that did not change the file are skipped using
//! the path index, without reading the file. At a change, the base commit's
//! blame is carried through the head of each change in the stack, bottom
//! first, and lines a change edited are credited to that change.

use super::index::{CommitInfo, Run};
use super::revision::{stack_root, Revision};
use super::trees::{file_at, normalise, Entry};
use super::{Ctx, HistoryError, HistoryResult};
use rosen_core::{Change, ChangeId, ObjectId};
use rosen_protocol::{Blame, BlameRange, BlameRequest};
use rosen_store::ObjectStore;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Commit(ObjectId),
    Change(ChangeId),
}

/// Where one line came from: its source, and its 0-based line number there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Origin {
    source: Source,
    line: u32,
}

/// A blamed file: its entry and the origin of each line.
type Blamed = (Entry, Vec<Origin>);

pub fn blame(ctx: &Ctx, request: &BlameRequest) -> HistoryResult<Blame> {
    let path = normalise(&request.path)?;
    if path.is_empty() {
        return Err(HistoryError::Invalid("blame needs a file path".into()));
    }
    let missing = || HistoryError::NotFound(format!("{path} is not a file there"));
    let (entry, origins) = match Revision::resolve(ctx, request.at.as_deref())? {
        Revision::Commit(commit) => blame_commit(ctx, commit, &path)?.ok_or_else(missing)?,
        Revision::Change(change) => blame_change(ctx, &change, &path)?.ok_or_else(missing)?,
        Revision::Tree(tree) => {
            return Err(HistoryError::Invalid(format!(
                "{tree} is a tree, which has no history; blame a commit or a change"
            )))
        }
    };
    answer(ctx, path, entry.id, &origins)
}

fn blame_commit(ctx: &Ctx, commit: ObjectId, path: &str) -> HistoryResult<Option<Blamed>> {
    let target = ctx.index.commit(&ctx.store, &commit)?;
    let Some(target_file) = blamable(ctx, target.tree, path)? else {
        return Ok(None);
    };
    let (start, chain) = walk_back(ctx, &target, path)?;
    let (mut previous, mut origins) = match start {
        Some((entry, runs)) => (read(ctx, entry)?, expand(&runs)),
        None => (Vec::new(), Vec::new()),
    };
    let target_changed = chain.first().is_some_and(|(id, _)| *id == target.id);
    for (id, entry) in chain.into_iter().rev() {
        let data = read(ctx, entry)?;
        origins = carry(&previous, &origins, &data, Source::Commit(id));
        ctx.index.put_blame(&id, path, &runs_of(&origins))?;
        previous = data;
    }
    if !target_changed {
        // Cached at the queried commit too, so asking again is one read.
        ctx.index.put_blame(&target.id, path, &runs_of(&origins))?;
    }
    Ok(Some((target_file, origins)))
}

/// The commits from `target` back that changed the file at `path`, newest
/// first, and the cached blame where the walk stopped (`None` when it
/// reached the commit that added the file).
#[allow(clippy::type_complexity)]
fn walk_back(
    ctx: &Ctx,
    target: &CommitInfo,
    path: &str,
) -> HistoryResult<(Option<(Entry, Vec<Run>)>, Vec<(ObjectId, Entry)>)> {
    let mut chain = Vec::new();
    let mut current = target.clone();
    loop {
        if let Some(runs) = ctx.index.blame(&current.id, path)? {
            if let Some(entry) = blamable(ctx, current.tree, path)? {
                return Ok((Some((entry, runs)), chain));
            }
        }
        let changed = current.first_parent().is_none()
            || ctx
                .index
                .changed_paths(&ctx.store, &current)?
                .iter()
                .any(|p| p == path);
        if changed {
            match blamable(ctx, current.tree, path)? {
                Some(entry) => chain.push((current.id, entry)),
                // Absent here, so the newer commit in the chain added it.
                None => return Ok((None, chain)),
            }
        }
        match current.first_parent() {
            Some(parent) => current = ctx.index.commit(&ctx.store, &parent)?,
            None => return Ok((None, chain)),
        }
    }
}

fn blame_change(ctx: &Ctx, change: &Change, path: &str) -> HistoryResult<Option<Blamed>> {
    if blamable(ctx, change.head, path)?.is_none() {
        return Ok(None);
    }
    let (stack, base) = stack_root(ctx, change)?;
    let (mut previous, mut origins) = match blame_commit(ctx, base, path)? {
        Some((entry, origins)) => (read(ctx, entry)?, origins),
        None => (Vec::new(), Vec::new()),
    };
    let mut last = None;
    for change in stack.iter().rev() {
        let Some(entry) = blamable(ctx, change.head, path)? else {
            // A change below deleted the file; the next one starts afresh.
            (previous, origins) = (Vec::new(), Vec::new());
            continue;
        };
        let data = read(ctx, entry)?;
        origins = carry(&previous, &origins, &data, Source::Change(change.id));
        previous = data;
        last = Some(entry);
    }
    Ok(last.map(|entry| (entry, origins)))
}

/// The file at `path` if it has lines to blame; a gitlink has none.
fn blamable(ctx: &Ctx, tree: ObjectId, path: &str) -> HistoryResult<Option<Entry>> {
    Ok(file_at(&ctx.store, tree, path)?.filter(|e| e.is_blob()))
}

fn read(ctx: &Ctx, entry: Entry) -> HistoryResult<Vec<u8>> {
    Ok(ctx.store.read_blob(&entry.id)?.data)
}

/// Origins of `new`'s lines: a line kept from `old` keeps its origin, and
/// every other line comes from `source`.
fn carry(old: &[u8], old_origins: &[Origin], new: &[u8], source: Source) -> Vec<Origin> {
    let line_count = new.split_inclusive(|&b| b == b'\n').count();
    let mut origins: Vec<Option<Origin>> = vec![None; line_count];
    for (old_line, kept_as) in rosen_merge::match_file_lines(old, new).iter().enumerate() {
        if let (Some(new_line), Some(origin)) = (kept_as, old_origins.get(old_line)) {
            origins[*new_line] = Some(*origin);
        }
    }
    (0u32..)
        .zip(origins)
        .map(|(line, origin)| origin.unwrap_or(Origin { source, line }))
        .collect()
}

fn expand(runs: &[Run]) -> Vec<Origin> {
    runs.iter()
        .flat_map(|run| {
            (run.original_start..run.original_start + run.len).map(|line| Origin {
                source: Source::Commit(run.commit),
                line,
            })
        })
        .collect()
}

/// Compresses origins into runs for the cache. Only commit blame is cached,
/// so every origin here comes from a commit.
fn runs_of(origins: &[Origin]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for origin in origins {
        let Source::Commit(commit) = origin.source else {
            unreachable!("commit blame only holds commit origins")
        };
        match runs.last_mut() {
            Some(run) if run.commit == commit && run.original_start + run.len == origin.line => {
                run.len += 1
            }
            _ => runs.push(Run {
                commit,
                original_start: origin.line,
                len: 1,
            }),
        }
    }
    runs
}

fn answer(ctx: &Ctx, path: String, blob: ObjectId, origins: &[Origin]) -> HistoryResult<Blame> {
    let mut ranges: Vec<BlameRange> = Vec::new();
    let mut commits = BTreeSet::new();
    for (line, origin) in (1u32..).zip(origins) {
        let (commit, change) = match origin.source {
            Source::Commit(id) => (Some(id), None),
            Source::Change(id) => (None, Some(id)),
        };
        commits.extend(commit);
        let extends = ranges.last().is_some_and(|r| {
            r.commit == commit
                && r.change == change
                && r.original_start + r.lines == origin.line + 1
        });
        if extends {
            ranges.last_mut().expect("checked above").lines += 1;
        } else {
            ranges.push(BlameRange {
                start: line,
                lines: 1,
                commit,
                original_start: origin.line + 1,
                change,
            });
        }
    }
    let mut entries = Vec::with_capacity(commits.len());
    for id in commits {
        entries.push(ctx.index.commit(&ctx.store, &id)?.entry());
    }
    for range in &mut ranges {
        if let Some(commit) = range.commit {
            let landed_by = entries.iter().find(|e| e.commit == commit);
            range.change = landed_by.and_then(|e| e.change);
        }
    }
    Ok(Blame {
        path,
        blob,
        ranges,
        commits: entries,
    })
}
