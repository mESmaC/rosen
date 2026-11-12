//! Typed call for submitting receipts (SPEC R8 step 1, D9).

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{ReceiptRecord, Request, Response, SubmitReceipt};

impl Client {
    /// Records that a check set passed on a change's merge result onto one
    /// base commit. The server refuses a receipt for a head that is no longer
    /// the change's head, a check set other than the pre-land one, or a base
    /// the base ref has never held.
    pub async fn submit_receipt(&self, request: SubmitReceipt) -> Result<ReceiptRecord, Error> {
        match self.request(Request::ReceiptSubmit(request)).await? {
            Response::Receipt(record) => Ok(record),
            other => Err(unexpected(other)),
        }
    }
}
