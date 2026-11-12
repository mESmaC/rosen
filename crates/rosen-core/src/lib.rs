//! Rosen's core data model (SPEC.md §2, §3): git-format objects, changes,
//! stacks, landed history and conflicts.
//!
//! This crate is pure logic and performs no I/O, so the store, the server,
//! the client and the proofs' reference models can all share it.

pub mod change;
pub mod claim;
pub mod conflict;
pub mod hash;
pub mod history;
pub mod object;
pub mod stack;

pub use change::{AgentId, Change, ChangeId, ChangeParent, ChangeState, Snapshot, SnapshotKind};
pub use claim::{blockers, Blocker, HeldClaim, PathPattern};
pub use conflict::{Conflict, ConflictId, ConflictSet};
pub use hash::ObjectId;
pub use history::{CommitGraph, LandedCommit, LandedTrailers};
pub use object::{Blob, Commit, FileMode, Object, ObjectKind, Signature, Tree, TreeEntry};
pub use stack::Stack;

use std::fmt;

/// Every way a core operation can reject its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A hex object ID or change ID was malformed.
    InvalidId(String),
    /// A serialized object did not follow git's format.
    MalformedObject(String),
    /// A tree entry name or mode is not allowed in a git tree.
    InvalidTreeEntry(String),
    /// An agent identity was empty or contained characters a trailer cannot hold.
    InvalidAgentId(String),
    /// A change state name that is not one of the four states.
    InvalidState(String),
    /// A change state transition that the state machine forbids.
    InvalidTransition {
        from: ChangeState,
        action: &'static str,
    },
    /// A change that is landed or abandoned cannot take new work.
    ChangeClosed(ChangeId),
    /// A stack's parent links form a cycle through this change.
    StackCycle(ChangeId),
    /// A change names a parent change that is not known.
    UnknownChange(ChangeId),
    /// A commit needed by a history walk is missing from the graph.
    MissingCommit(ObjectId),
    /// A claimed path or glob is empty, absolute or leaves the repository.
    InvalidPath(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidId(s) => write!(f, "invalid id: {s}"),
            Error::MalformedObject(s) => write!(f, "malformed object: {s}"),
            Error::InvalidTreeEntry(s) => write!(f, "invalid tree entry: {s}"),
            Error::InvalidAgentId(s) => write!(f, "invalid agent id: {s:?}"),
            Error::InvalidState(s) => write!(f, "unknown change state {s:?}"),
            Error::InvalidTransition { from, action } => {
                write!(f, "cannot {action} a change that is {from}")
            }
            Error::ChangeClosed(id) => write!(f, "change {id} is closed"),
            Error::StackCycle(id) => write!(f, "stack cycle through change {id}"),
            Error::UnknownChange(id) => write!(f, "unknown change {id}"),
            Error::MissingCommit(id) => write!(f, "commit {id} not found"),
            Error::InvalidPath(s) => write!(f, "invalid path {s}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
