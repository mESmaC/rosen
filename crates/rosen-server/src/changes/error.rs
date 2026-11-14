use rosen_core::{ChangeId, ChangeState};

#[derive(Debug, thiserror::Error)]
pub enum ChangeError {
    #[error("{0:?} is not a change ID (change IDs use only the letters k-z)")]
    InvalidId(String),
    #[error("no change matches {0:?}")]
    NotFound(String),
    /// A prefix matched more than one change; the caller must type more of it.
    #[error("{0:?} matches more than one change; give more of the ID")]
    Ambiguous(String),
    /// An owner named with `@`, which could pass for another machine's
    /// agent once the server appends the machine (R14).
    #[error("owner {0} must be a nick without '@'; the server records its machine")]
    QualifiedOwner(String),
    /// A change cannot be stacked on one that is landed or abandoned.
    #[error("cannot stack on change {parent}: it is {state}")]
    ParentClosed {
        parent: ChangeId,
        state: ChangeState,
    },
    /// The change model refused the operation (state machine, closed change,
    /// stack cycle).
    #[error(transparent)]
    Refused(#[from] rosen_core::Error),
    /// The base ref could not be read when creating a change on it.
    #[error("cannot read the base: {0}")]
    Base(String),
    #[error("cannot mint a change ID: {0}")]
    Randomness(String),
    /// A stored row does not parse; only a damaged or hand-edited store does this.
    #[error("metadata store holds an unreadable {0}")]
    Corrupt(String),
    #[error("metadata store directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("metadata store: {0}")]
    Store(#[from] rusqlite::Error),
}
