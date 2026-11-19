//! The newlines a project declares for its files in `.gitattributes` (R5,
//! #424). Only the attributes that decide a stored file's newlines are read:
//! `text` (with the `binary` macro and the legacy `crlf`) and `eol`. Files
//! come from the target tree, the root's first and then each directory's
//! down to the path, and the last matching line wins, as git reads them.

use super::{HistoryResult, ObjectId};
use crate::history::trees::file_at;
use rosen_core::{FileMode, PathPattern};
use rosen_store::ObjectStore;
use std::collections::HashMap;
use std::rc::Rc;

/// Largest `.gitattributes` read. A larger one declares nothing, so a
/// merge never reads a large file whole for its attributes.
const LARGEST_ATTRIBUTES: u64 = 1 << 20;

/// A declared newline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Eol {
    Lf,
    Crlf,
}

impl Eol {
    pub(super) fn newline(self) -> &'static [u8] {
        match self {
            Eol::Lf => b"\n",
            Eol::Crlf => b"\r\n",
        }
    }
}

/// What the attributes say about one path's newlines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Declared {
    /// No `eol`: the base's own convention applies.
    Nothing,
    /// `eol=lf` or `eol=crlf`, on a path not marked `-text`.
    Eol(Eol),
    /// `-text` or `binary`: the file's bytes are kept exactly.
    NotText,
}

/// One attributes line. The outer `Option` says whether the line names the
/// attribute; the inner `None` resets it to unspecified (`!text`, `-eol`).
struct Rule {
    pattern: PathPattern,
    text: Option<Option<bool>>,
    eol: Option<Option<Eol>>,
}

impl Rule {
    fn read(&mut self, attribute: &str) {
        match attribute {
            "text" | "text=auto" | "crlf" => self.text = Some(Some(true)),
            "-text" | "binary" | "-crlf" => self.text = Some(Some(false)),
            "!text" | "!crlf" => self.text = Some(None),
            "eol=lf" | "crlf=input" => self.eol = Some(Some(Eol::Lf)),
            "eol=crlf" => self.eol = Some(Some(Eol::Crlf)),
            "-eol" | "!eol" => self.eol = Some(None),
            _ => {}
        }
    }
}

/// The attributes of paths in one target tree, each directory's file read
/// at most once.
pub(super) struct Attributes<'a> {
    store: &'a dyn ObjectStore,
    tree: ObjectId,
    rules: HashMap<String, Rc<[Rule]>>,
}

impl<'a> Attributes<'a> {
    pub(super) fn new(store: &'a dyn ObjectStore, tree: ObjectId) -> Self {
        Self {
            store,
            tree,
            rules: HashMap::new(),
        }
    }

    /// What the target tree's attributes declare for the file at `path`.
    pub(super) fn declared(&mut self, path: &str) -> HistoryResult<Declared> {
        let (mut text, mut eol) = (None, None);
        for dir in directories_above(path) {
            let rules = self.rules_in(&dir)?;
            for rule in rules.iter().filter(|rule| rule.pattern.matches(path)) {
                if let Some(value) = rule.text {
                    text = value;
                }
                if let Some(value) = rule.eol {
                    eol = value;
                }
            }
        }
        Ok(match (text, eol) {
            (Some(false), _) => Declared::NotText,
            (_, Some(eol)) => Declared::Eol(eol),
            _ => Declared::Nothing,
        })
    }

    fn rules_in(&mut self, dir: &str) -> HistoryResult<Rc<[Rule]>> {
        if let Some(rules) = self.rules.get(dir) {
            return Ok(Rc::clone(rules));
        }
        let path = if dir.is_empty() {
            ".gitattributes".to_owned()
        } else {
            format!("{dir}/.gitattributes")
        };
        let text = match file_at(self.store, self.tree, &path)? {
            Some(file)
                if matches!(
                    file.mode,
                    FileMode::Regular | FileMode::Executable | FileMode::Legacy(_)
                ) && self.store.object_size(&file.id)? <= LARGEST_ATTRIBUTES =>
            {
                self.store.read_blob(&file.id)?.data
            }
            _ => Vec::new(),
        };
        let rules: Rc<[Rule]> = String::from_utf8_lossy(&text)
            .lines()
            .filter_map(|line| parse_line(dir, line))
            .collect();
        self.rules.insert(dir.to_owned(), Rc::clone(&rules));
        Ok(rules)
    }
}

/// The directories whose `.gitattributes` can apply to `path`, root first.
fn directories_above(path: &str) -> Vec<String> {
    let mut dirs = vec![String::new()];
    let segments: Vec<&str> = path.split('/').collect();
    for end in 1..segments.len() {
        dirs.push(segments[..end].join("/"));
    }
    dirs
}

/// One line of the `.gitattributes` in `dir`, when it sets `text` or `eol`.
/// Macro definitions other than the built-in `binary` are not read.
fn parse_line(dir: &str, line: &str) -> Option<Rule> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with("[attr]") {
        return None;
    }
    let mut fields = line.split_whitespace();
    let mut rule = Rule {
        pattern: pattern_in(dir, fields.next()?)?,
        text: None,
        eol: None,
    };
    fields.for_each(|attribute| rule.read(attribute));
    (rule.text.is_some() || rule.eol.is_some()).then_some(rule)
}

/// A `.gitattributes` pattern in `dir` as a repository path pattern. Without
/// a `/` it names a file at any depth below `dir`; with one it is anchored
/// at `dir`. A directory pattern (trailing `/`) never applies to files in an
/// attributes file. Patterns [`PathPattern`] cannot express, such as quoted
/// names, character classes and escapes, are skipped, so they declare
/// nothing.
fn pattern_in(dir: &str, pattern: &str) -> Option<PathPattern> {
    if pattern.ends_with('/') || pattern.contains(['"', '[', ']', '\\']) {
        return None;
    }
    let relative = match pattern.strip_prefix('/') {
        Some(anchored) => anchored.to_owned(),
        None if pattern.contains('/') => pattern.to_owned(),
        None => format!("**/{pattern}"),
    };
    let full = if dir.is_empty() {
        relative
    } else {
        format!("{dir}/{relative}")
    };
    PathPattern::new(&full).ok()
}
