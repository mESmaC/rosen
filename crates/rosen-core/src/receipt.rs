//! Receipts (SPEC.md §2, R8 step 1, D9): a record that a check set passed on
//! one exact merge result, bound to `(change head, base commit, check set)`
//! and carrying the identity of the checker that submitted it.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{AgentId, ChangeId, ChangeState, ObjectId};

/// The pre-land check set (build and lint, D20) every non-prose land needs a
/// receipt for, until project configuration names its own.
pub const PRE_LAND_CHECK_SET: &str = "pre-land";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub change: ChangeId,
    /// The change head the checks ran on.
    pub head: ObjectId,
    /// The base commit the head was merged onto for the checks.
    pub base: ObjectId,
    pub check_set: String,
    /// Who ran the checks. It is written into the landed commit's
    /// `Rosen-Receipt` trailer, so [`AgentId`] keeps it to one line.
    pub submitter: AgentId,
}

/// Why the server refuses to store a receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiptRefusal {
    /// Landed and abandoned changes never land again, so a receipt for one
    /// could never count.
    ChangeClosed {
        change: ChangeId,
        state: ChangeState,
    },
    /// The change has moved on since the checks ran: the checked head is not
    /// the one a land would use.
    StaleHead {
        submitted: ObjectId,
        current: ObjectId,
    },
    /// The name is empty or holds whitespace, which would let it forge
    /// trailers in the landed commit (issue #18).
    MalformedCheckSet(String),
    WrongCheckSet {
        submitted: String,
        required: String,
    },
    /// The base ref has never held this commit, so it cannot be a merge
    /// result the server computed.
    UnknownBase {
        base: ObjectId,
        base_ref: String,
    },
}

impl fmt::Display for ReceiptRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReceiptRefusal::ChangeClosed { change, state } => {
                write!(f, "change {change} is {state} and will not land again")
            }
            ReceiptRefusal::StaleHead { submitted, current } => write!(
                f,
                "head {submitted} is not the change's current head {current}; \
                 check the current head and submit again"
            ),
            ReceiptRefusal::MalformedCheckSet(name) => {
                write!(f, "check set {name:?} is empty or contains whitespace")
            }
            ReceiptRefusal::WrongCheckSet {
                submitted,
                required,
            } => write!(
                f,
                "check set {submitted} is not the pre-land check set {required}"
            ),
            ReceiptRefusal::UnknownBase { base, base_ref } => {
                write!(f, "base {base} was never held by the base ref {base_ref}")
            }
        }
    }
}

impl std::error::Error for ReceiptRefusal {}

impl Receipt {
    /// Checks the parts of the binding the change itself decides: the change
    /// can still land, the receipt names its current head, and the check set
    /// is the one landing requires. The base is checked by the caller against
    /// the base history, which this crate cannot read.
    pub fn check_binding(
        &self,
        current_head: &ObjectId,
        state: ChangeState,
        required_check_set: &str,
    ) -> Result<(), ReceiptRefusal> {
        if state.is_closed() {
            return Err(ReceiptRefusal::ChangeClosed {
                change: self.change,
                state,
            });
        }
        let name = &self.check_set;
        if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ReceiptRefusal::MalformedCheckSet(name.clone()));
        }
        if name != required_check_set {
            return Err(ReceiptRefusal::WrongCheckSet {
                submitted: name.clone(),
                required: required_check_set.to_owned(),
            });
        }
        if self.head != *current_head {
            return Err(ReceiptRefusal::StaleHead {
                submitted: self.head,
                current: *current_head,
            });
        }
        Ok(())
    }
}
