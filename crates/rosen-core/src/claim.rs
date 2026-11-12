//! Path claims (SPEC R6): which paths a change declares it will edit, and the
//! first-claim-wins rule that decides whether a new claim is granted.
//!
//! A claim names a path or a glob. Two claims conflict when some path matches
//! both of them, so `src/*.rs` conflicts with `src/lib.rs` and with `src/**`,
//! but not with `src/*.md`. The test is exact: it never refuses a claim that
//! no path could share, and never grants one that a path could.

use crate::change::ChangeId;
use crate::Error;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

/// A repository-relative path or glob, in normal form: `/` separators, no
/// `.` or empty segments, never absolute and never climbing out with `..`.
///
/// Glob syntax: `?` matches one character other than `/`, `*` any run of
/// them, and a whole `**` segment any number of directories. A trailing `/`
/// claims everything under a directory, as `dir/**` does.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PathPattern(String);

/// Longest pattern accepted. Overlap costs the product of two patterns'
/// lengths, so a bound keeps one claim from stalling the claim transaction.
pub const MAX_PATTERN_LEN: usize = 1024;

impl PathPattern {
    pub fn new(text: &str) -> Result<Self, Error> {
        let invalid = |why: &str| Error::InvalidPath(format!("{text:?}: {why}"));
        let unified = text.trim().replace('\\', "/");
        let is_dir = unified.ends_with('/');
        if unified.starts_with('/') || unified.as_bytes().get(1) == Some(&b':') {
            return Err(invalid("must be relative to the repository root"));
        }
        let mut segments: Vec<&str> = Vec::new();
        for segment in unified.split('/') {
            match segment {
                "" | "." => {}
                ".." => return Err(invalid("must not leave the repository with `..`")),
                _ => segments.push(segment),
            }
        }
        if segments.is_empty() {
            return Err(invalid("names no path"));
        }
        if is_dir && segments.last() != Some(&"**") {
            segments.push("**");
        }
        let normal = segments.join("/");
        if normal.len() > MAX_PATTERN_LEN {
            return Err(invalid("is too long"));
        }
        Ok(PathPattern(normal))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether some path matches both patterns.
    pub fn overlaps(&self, other: &PathPattern) -> bool {
        Matcher::new(&self.0).intersects(&Matcher::new(&other.0))
    }

    /// Whether `path` (a plain path, not a glob) matches this pattern.
    pub fn matches(&self, path: &str) -> bool {
        let literal = Matcher::literal(path);
        Matcher::new(&self.0).intersects(&literal)
    }
}

impl fmt::Display for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for PathPattern {
    type Error = Error;
    fn try_from(text: String) -> Result<Self, Error> {
        PathPattern::new(&text)
    }
}

impl From<PathPattern> for String {
    fn from(pattern: PathPattern) -> String {
        pattern.0
    }
}

/// One character a matcher step accepts.
#[derive(Debug, Clone, Copy)]
enum Class {
    Char(char),
    /// Any character but `/`: what `?` and `*` match.
    NotSlash,
    /// Any character: what `**` matches.
    Any,
}

impl Class {
    /// Whether some single character is in both classes.
    fn meets(self, other: Class) -> bool {
        match (self, other) {
            (Class::Char(a), Class::Char(b)) => a == b,
            (Class::Char(c), Class::NotSlash) | (Class::NotSlash, Class::Char(c)) => c != '/',
            _ => true,
        }
    }
}

/// A pattern compiled to a nondeterministic automaton. The last state accepts.
#[derive(Debug, Default)]
struct Matcher {
    /// Moves that consume no character.
    skips: Vec<Vec<usize>>,
    /// Moves that consume one character of a class.
    steps: Vec<Vec<(Class, usize)>>,
}

impl Matcher {
    fn new(pattern: &str) -> Self {
        let mut matcher = Matcher::default();
        let mut current = matcher.state();
        let segments: Vec<&str> = pattern.split('/').collect();
        for (index, segment) in segments.iter().enumerate() {
            let last = index + 1 == segments.len();
            if *segment == "**" {
                current = matcher.any_dirs(current, last);
                continue;
            }
            for c in segment.chars() {
                current = matcher.char_step(current, c);
            }
            if !last {
                current = matcher.step(current, Class::Char('/'));
            }
        }
        matcher
    }

    fn literal(path: &str) -> Self {
        let mut matcher = Matcher::default();
        let mut current = matcher.state();
        for c in path.chars() {
            current = matcher.step(current, Class::Char(c));
        }
        matcher
    }

    fn state(&mut self) -> usize {
        self.skips.push(Vec::new());
        self.steps.push(Vec::new());
        self.skips.len() - 1
    }

    fn step(&mut self, from: usize, class: Class) -> usize {
        let to = self.state();
        self.steps[from].push((class, to));
        to
    }

    fn char_step(&mut self, from: usize, c: char) -> usize {
        match c {
            '?' => self.step(from, Class::NotSlash),
            // `*` loops on non-slash characters, then moves on without consuming.
            '*' => {
                self.steps[from].push((Class::NotSlash, from));
                let to = self.state();
                self.skips[from].push(to);
                to
            }
            _ => self.step(from, Class::Char(c)),
        }
    }

    /// A `**` segment. Last, it matches any rest of the path. Elsewhere, with
    /// the `/` after it, it matches nothing or any run that ends in `/`.
    fn any_dirs(&mut self, from: usize, last: bool) -> usize {
        let inside = self.state();
        self.steps[from].push((Class::Any, inside));
        self.steps[inside].push((Class::Any, inside));
        if last {
            let to = self.state();
            self.skips[from].push(to);
            self.skips[inside].push(to);
            return to;
        }
        let to = self.state();
        self.skips[from].push(to);
        self.steps[inside].push((Class::Char('/'), to));
        to
    }

    fn accept(&self) -> usize {
        self.skips.len() - 1
    }

    /// Searches the product automaton for a path both matchers accept. Each
    /// pair of states is visited once, so the cost is at most the product of
    /// the two patterns' sizes.
    fn intersects(&self, other: &Matcher) -> bool {
        let mut seen = HashSet::new();
        let mut pending = vec![(0usize, 0usize)];
        while let Some((a, b)) = pending.pop() {
            if !seen.insert((a, b)) {
                continue;
            }
            if a == self.accept() && b == other.accept() {
                return true;
            }
            pending.extend(self.skips[a].iter().map(|&next| (next, b)));
            pending.extend(other.skips[b].iter().map(|&next| (a, next)));
            for &(class_a, next_a) in &self.steps[a] {
                for &(class_b, next_b) in &other.steps[b] {
                    if class_a.meets(class_b) {
                        pending.push((next_a, next_b));
                    }
                }
            }
        }
        false
    }
}

/// A live claim: a change that holds a pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldClaim {
    pub change: ChangeId,
    pub pattern: PathPattern,
}

/// One reason a claim is refused: the index of a requested pattern, and the
/// index of the claim on another change that it overlaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blocker {
    pub requested: usize,
    pub held: usize,
}

/// The first-claim-wins rule. A claim by `claimant` on `requested` is granted
/// exactly when no pattern in it overlaps a claim another change holds; the
/// claimant's own claims never block it. Returns every blocker, so a refusal
/// can name each holder. Granting is all or nothing: one blocker refuses the
/// whole request, so a change never holds half of what it asked for.
pub fn blockers(claimant: ChangeId, requested: &[PathPattern], held: &[HeldClaim]) -> Vec<Blocker> {
    let mut found = Vec::new();
    for (r, pattern) in requested.iter().enumerate() {
        for (h, claim) in held.iter().enumerate() {
            if claim.change != claimant && claim.pattern.overlaps(pattern) {
                found.push(Blocker {
                    requested: r,
                    held: h,
                });
            }
        }
    }
    found
}
