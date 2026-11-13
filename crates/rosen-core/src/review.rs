//! Review threads (SPEC R7): comments anchored to a file, a line and the
//! change head they were written against, grouped into threads that are open
//! or resolved.
//!
//! The anchor is fixed when a thread starts, so a later snapshot never moves a
//! comment onto a line it was not written about. Each comment also records
//! the head it was written at, so a reader can tell which replies came after
//! the author changed the code.

use crate::claim::PathPattern;
use crate::Error;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Longest comment body accepted. Bodies are stored in the metadata store
/// and sent in every thread listing, so one comment must not bloat both.
pub const MAX_COMMENT_LEN: usize = 64 * 1024;

/// Whether a thread still needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    Open,
    Resolved,
}

impl ThreadState {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadState::Open => "open",
            ThreadState::Resolved => "resolved",
        }
    }

    /// The state after a reply. A reply to a resolved thread reopens it: the
    /// replier is saying the point still stands, and a resolved thread would
    /// hide that from the author.
    pub fn after_reply(self) -> ThreadState {
        ThreadState::Open
    }
}

impl fmt::Display for ThreadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ThreadState {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Error> {
        match text {
            "open" => Ok(ThreadState::Open),
            "resolved" => Ok(ThreadState::Resolved),
            other => Err(Error::InvalidReview(format!(
                "unknown thread state {other:?}; use open or resolved"
            ))),
        }
    }
}

/// Normalises the file a comment anchors to: a repository-relative path to
/// one file, with `/` separators. Globs and directories name no single line,
/// so they are refused.
pub fn anchor_path(text: &str) -> Result<String, Error> {
    let trimmed = text.trim();
    if trimmed.ends_with('/') || trimmed.ends_with('\\') {
        return Err(Error::InvalidReview(format!(
            "{text:?} is a directory; anchor a comment to a file"
        )));
    }
    if trimmed.contains(['*', '?']) {
        return Err(Error::InvalidReview(format!(
            "{text:?} is a glob; anchor a comment to one file"
        )));
    }
    Ok(PathPattern::new(trimmed)?.as_str().to_owned())
}

/// Checks that `line` (1-based) exists in a file with contents `data`. An
/// empty file still has line 1, so a reviewer can say it should not be empty.
pub fn check_line(data: &[u8], line: u32) -> Result<(), Error> {
    let lines = line_count(data).max(1);
    if line == 0 || u64::from(line) > lines {
        return Err(Error::InvalidReview(format!(
            "line {line} is outside the file, which has {lines} line(s)"
        )));
    }
    Ok(())
}

/// Lines as an editor numbers them: a final line without a newline still
/// counts, and a trailing newline does not start a new one.
fn line_count(data: &[u8]) -> u64 {
    let newlines = data.iter().filter(|&&b| b == b'\n').count() as u64;
    let unterminated = u64::from(data.last().is_some_and(|&b| b != b'\n'));
    newlines + unterminated
}

/// Checks a comment body: not blank, and within [`MAX_COMMENT_LEN`].
pub fn check_body(body: &str) -> Result<(), Error> {
    if body.trim().is_empty() {
        return Err(Error::InvalidReview("a comment needs text".into()));
    }
    if body.len() > MAX_COMMENT_LEN {
        return Err(Error::InvalidReview(format!(
            "a comment is limited to {MAX_COMMENT_LEN} bytes"
        )));
    }
    Ok(())
}
