//! Which workspace paths snapshots skip (SPEC R2 "Ignored paths").
//!
//! A path is ignored when the workspace's `.gitignore` files or the
//! project's ignore list say so, read the way git reads them: each file
//! applies to its own directory and below, and a deeper file's match
//! overrides a shallower one. The project's list ranks below every
//! `.gitignore`, as git's `info/exclude` does. `.git` and the workspace's
//! own `.rosen` directory are always ignored.
//!
//! The project's list comes from the base's configuration, recorded in the
//! workspace marker when the workspace is made or rebased, so a change that
//! edits `rosen.toml` cannot change what its own snapshots skip.
//!
//! As in git, ignore rules apply only to untracked paths: a file the stored
//! tree already holds stays in snapshots even if a pattern matches it.
//! Otherwise a pattern added later would delete a base file from the change.

use super::marker::STATE_DIR;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct IgnoreRules {
    root: PathBuf,
    /// The project's ignore list, rooted at the workspace root.
    project: Option<Gitignore>,
    /// Parsed `.gitignore` per workspace-relative directory, or `None` where
    /// the directory has none. Filled lazily as paths are checked.
    per_dir: HashMap<String, Option<Gitignore>>,
}

impl IgnoreRules {
    /// `project` is the project configuration's ignore list (SPEC R2).
    pub fn new(root: PathBuf, project: &[String]) -> Self {
        Self {
            project: project_rules(&root, project),
            root,
            per_dir: HashMap::new(),
        }
    }

    /// Whether `rel`, a `/`-separated path from the workspace root, is
    /// ignored. The caller still keeps tracked paths.
    pub fn is_ignored(&mut self, rel: &str, is_dir: bool) -> bool {
        if always_ignored(rel) {
            return true;
        }
        let parts: Vec<&str> = rel.split('/').collect();
        let mut ignored = self
            .project
            .as_ref()
            .is_some_and(|rules| rules.matched_path_or_any_parents(rel, is_dir).is_ignore());
        for depth in 0..parts.len() {
            let dir = parts[..depth].join("/");
            let rest = parts[depth..].join("/");
            let Some(rules) = self.rules_for(&dir) else {
                continue;
            };
            match rules.matched_path_or_any_parents(&rest, is_dir) {
                Match::Ignore(_) => ignored = true,
                Match::Whitelist(_) => ignored = false,
                Match::None => {}
            }
        }
        ignored
    }

    /// Drops every parsed `.gitignore`; called when one of them changes.
    pub fn forget(&mut self) {
        self.per_dir.clear();
    }

    fn rules_for(&mut self, dir: &str) -> Option<&Gitignore> {
        if !self.per_dir.contains_key(dir) {
            let parsed = self.parse(dir);
            self.per_dir.insert(dir.to_owned(), parsed);
        }
        self.per_dir.get(dir).and_then(Option::as_ref)
    }

    fn parse(&self, dir: &str) -> Option<Gitignore> {
        let dir_path = if dir.is_empty() {
            self.root.clone()
        } else {
            self.root.join(dir)
        };
        let file = dir_path.join(".gitignore");
        if !file.is_file() {
            return None;
        }
        let mut builder = GitignoreBuilder::new(&dir_path);
        // A malformed line is skipped, as git skips it; the rest still apply.
        let _ = builder.add(&file);
        builder.build().ok()
    }
}

fn project_rules(root: &std::path::Path, lines: &[String]) -> Option<Gitignore> {
    if lines.is_empty() {
        return None;
    }
    let mut builder = GitignoreBuilder::new(root);
    for line in lines {
        // A malformed line is skipped, as git skips it; the rest still apply.
        let _ = builder.add_line(None, line);
    }
    builder.build().ok()
}

/// `.git` anywhere (a nested repository's metadata) and the workspace's own
/// state directory are never part of a change.
pub fn always_ignored(rel: &str) -> bool {
    let mut parts = rel.split('/');
    parts.next() == Some(STATE_DIR) || rel.split('/').any(|part| part == ".git")
}

/// Whether a change to this path can change the ignore rules.
pub fn is_ignore_file(rel: &str) -> bool {
    rel == ".gitignore" || rel.ends_with("/.gitignore")
}
