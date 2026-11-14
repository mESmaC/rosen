//! Wire shapes for `rosen receipt submit` (SPEC R8 step 1, D9, R10).

use rosen_core::{AgentId, ObjectId, Receipt};
use serde::{Deserialize, Serialize};

/// Parameters of [`crate::Request::ReceiptSubmit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitReceipt {
    /// A change ID or unique prefix of one.
    pub change: String,
    /// The change head the checks ran on.
    pub head: ObjectId,
    /// The base commit the head was merged onto for the checks.
    pub base: ObjectId,
    pub check_set: String,
    /// The checker's identity, recorded on the receipt and in the landed
    /// commit's `Rosen-Receipt` trailer.
    pub submitter: AgentId,
    /// The submitting machine's signature over the receipt. A client leaves
    /// it empty: its machine's daemon signs the receipt on the way to the
    /// server, and the server signs one submitted on its own machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<ReceiptSignature>,
}

/// A machine's Ed25519 signature over [`Receipt::signing_payload`], with
/// the public key that checks it, both in lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptSignature {
    pub public_key: String,
    pub signature: String,
}

/// The answer to [`crate::Request::ReceiptSubmit`]: the receipt as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptRecord {
    /// The stored receipt. Resubmitting an identical receipt keeps the first
    /// one, so `submitter` names whoever submitted it first.
    #[serde(flatten)]
    pub receipt: Receipt,
    /// The base ref the receipt's base commit was held by, for example `main`.
    pub base_ref: String,
    /// The base ref still points at the receipt's base. When false, a land
    /// re-checks whether the receipt still counts (SPEC R8 step 2).
    pub base_current: bool,
}
