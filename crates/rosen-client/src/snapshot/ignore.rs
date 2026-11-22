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
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct IgnoreRules {
    root: PathBuf,
    /// The project's ignore list, rooted at the workspace root.
    project: Option<Gitignore>,
    /// Stored `.gitignore` bytes by workspace-relative directory, used as a
    /// fallback when resuming deletion of a staged directory whose `.gitignore`
    /// was already removed on disk (#430).
    stored: HashMap<String, Vec<u8>>,
    /// Parsed `.gitignore` per workspace-relative directory, or `None` where
    /// the directory has none. Filled lazily as paths are checked.
    per_dir: HashMap<String, Option<Gitignore>>,
}

impl IgnoreRules {
    /// `project` is the project configuration's ignore list (SPEC R2).
    pub fn new(root: PathBuf, project: &[String]) -> Self {
        Self::with_stored(root, project, HashMap::new())
    }

    pub fn with_stored(
        root: PathBuf,
        project: &[String],
        stored: HashMap<String, Vec<u8>>,
    ) -> Self {
        Self {
            project: project_rules(&root, project),
            root,
            stored,
            per_dir: HashMap::new(),
        }
    }

    /// Whether `rel`, a `/`-separated path from the workspace root, is
    /// ignored. The caller still keeps tracked paths.
    pub fn is_ignored(&mut self, rel: &str, is_dir: bool) -> bool {
        if always_ignored(rel) || self.ignores_parent(rel) {
            return true;
        }
        self.match_path(rel, is_dir)
    }

    /// Stops before reading rules inside an excluded directory, just as a
    /// full scan stops before entering it. Besides matching git's rules,
    /// this avoids opening handles in build trees being renamed on Windows.
    pub fn ignores_parent(&mut self, rel: &str) -> bool {
        rel.match_indices('/')
            .any(|(end, _)| self.match_path(&rel[..end], true))
    }

    fn match_path(&mut self, rel: &str, is_dir: bool) -> bool {
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
        if file.is_file() {
            match File::open(&file) {
                Ok(opened) => return parse_gitignore(&dir_path, &file, BufReader::new(opened)),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return None,
            }
        }
        let stored = self.stored.get(dir)?;
        parse_gitignore(&dir_path, &file, stored.as_slice())
    }
}

/// Parses `.gitignore` bytes the same way `GitignoreBuilder::add` reads a live
/// file, so live files and stored fallbacks apply identical rules (#456).
fn parse_gitignore(dir_path: &Path, file: &Path, reader: impl BufRead) -> Option<Gitignore> {
    const UTF8_BOM: &str = "\u{feff}";
    let mut builder = GitignoreBuilder::new(dir_path);
    for (i, line) in reader.lines().enumerate() {
        let Ok(line) = line else {
            break;
        };
        let line = if i == 0 {
            line.trim_start_matches(UTF8_BOM)
        } else {
            &line
        };
        // A malformed line is skipped, as git skips it; the rest still apply.
        let _ = builder.add_line(Some(file.to_path_buf()), line);
    }
    builder.build().ok()
}

fn project_rules(root: &Path, lines: &[String]) -> Option<Gitignore> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_and_stored_parse_utf8_bom_identically() {
        // R2/R4 (#456): stored fallback must match live BOM and UTF-8 parsing.
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"\xef\xbb\xbfcache/\n# non-utf8 \xff\nlater/\n".to_vec();
        let path = dir.path().join(".gitignore");
        std::fs::write(&path, &bytes).unwrap();
        let mut live = IgnoreRules::new(dir.path().to_path_buf(), &[]);
        assert!(live.is_ignored("cache/a", false) && !live.is_ignored("later/a", false));
        std::fs::remove_file(&path).unwrap();
        let stored_map = HashMap::from([(String::new(), bytes)]);
        let mut stored = IgnoreRules::with_stored(dir.path().to_path_buf(), &[], stored_map);
        assert!(stored.is_ignored("cache/a", false) && !stored.is_ignored("later/a", false));
    }
}
