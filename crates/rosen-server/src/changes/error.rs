use rosen_core::change::{ChangeId, ChangeState};
use std::fmt;

#[derive(Debug)]
pub enum ChangeError {
    /// The text is not a change ID or a prefix of one.
    InvalidId(String),
    NotFound(String),
    /// A prefix matched more than one change; the caller must type more of it.
    Ambiguous(String),
    InvalidTransition {
        change: ChangeId,
        from: ChangeState,
        to: ChangeState,
    },
    /// The change is landed or abandoned, so its work can no longer move.
    Closed {
        change: ChangeId,
        state: ChangeState,
    },
    /// A stack would loop back on itself.
    Cycle {
        change: ChangeId,
        parent: ChangeId,
    },
    /// The base ref could not be read when creating a change on it.
    Base(String),
    /// The OS could not supply random bytes for a new change ID.
    Randomness(String),
    Store(rusqlite::Error),
}

impl fmt::Display for ChangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChangeError::InvalidId(text) => {
                write!(
                    f,
                    "{text:?} is not a change ID (IDs use only the letters k-z)"
                )
            }
            ChangeError::NotFound(text) => write!(f, "no change matches {text:?}"),
            ChangeError::Ambiguous(text) => {
                write!(
                    f,
                    "{text:?} matches more than one change; give more of the ID"
                )
            }
            ChangeError::InvalidTransition { change, from, to } => write!(
                f,
                "change {change} is {} and cannot become {}",
                from.as_str(),
                to.as_str()
            ),
            ChangeError::Closed { change, state } => {
                write!(f, "change {change} is {}", state.as_str())
            }
            ChangeError::Cycle { change, parent } => {
                write!(f, "stacking {change} on {parent} would make a cycle")
            }
            ChangeError::Base(reason) => write!(f, "cannot read the base: {reason}"),
            ChangeError::Randomness(reason) => write!(f, "cannot mint a change ID: {reason}"),
            ChangeError::Store(err) => write!(f, "metadata store: {err}"),
        }
    }
}

impl std::error::Error for ChangeError {}

impl From<rusqlite::Error> for ChangeError {
    fn from(err: rusqlite::Error) -> Self {
        ChangeError::Store(err)
    }
}
