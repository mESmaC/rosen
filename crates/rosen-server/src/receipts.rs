//! `receipt submit` (SPEC R8 step 1, D9): checkers run the pre-land check
//! set outside Rosen and submit a receipt. The server validates its binding
//! and stores it in the landing ledger, where a land looks it up.
//!
//! Validation here gives the checker an immediate answer. It is not what
//! keeps a land safe: a land only accepts a receipt for the change's head
//! at land time and a base the base ref has held, so a head that moves
//! between this check and the store leaves a receipt no land will use.

use std::sync::{Arc, Mutex, PoisonError};

use rosen_core::{Receipt, ReceiptRefusal, PRE_LAND_CHECK_SET};
use rosen_protocol::{ErrorBody, ErrorCode, Outcome, ReceiptRecord, Response, SubmitReceipt};

use crate::landing::{LandError, Ledger};
use crate::rpc::change_error_code;
use crate::{ServerConfig, ServerState};

/// Stores receipts in the landing ledger's tables of the metadata store.
pub struct ReceiptService {
    config: ServerConfig,
    /// Opened on first use, so a server that never takes a receipt never
    /// creates the database.
    ledger: Mutex<Option<Arc<Ledger>>>,
}

/// Runs a submit on the blocking pool: SQLite I/O must not stall the async
/// workers that carry every other connection.
pub async fn submit(state: &Arc<ServerState>, request: SubmitReceipt) -> Outcome {
    let state = Arc::clone(state);
    let work = move || state.receipts.submit(&state, request);
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(record)) => Outcome::Ok(Response::Receipt(record)),
        Ok(Err(body)) => Outcome::Error(body),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

impl ReceiptService {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            ledger: Mutex::new(None),
        }
    }

    fn submit(
        &self,
        state: &ServerState,
        request: SubmitReceipt,
    ) -> Result<ReceiptRecord, ErrorBody> {
        let record = state
            .changes
            .show(&request.change)
            .map_err(|err| ErrorBody::new(change_error_code(&err), err.to_string()))?;
        let change = &record.change;
        let receipt = Receipt {
            change: change.id,
            head: request.head,
            base: request.base,
            check_set: request.check_set,
            submitter: request.submitter,
        };
        receipt
            .check_binding(&change.head, change.state, PRE_LAND_CHECK_SET)
            .map_err(refusal_body)?;
        let ledger = self.ledger()?;
        let (base_ref, current) = ledger.project_base().map_err(land_body)?.ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Refused,
                "the project has no base yet; import a repository with `rosen import`",
            )
        })?;
        if ledger
            .base_seq(&base_ref, &receipt.base)
            .map_err(land_body)?
            .is_none()
        {
            let base = receipt.base;
            return Err(refusal_body(ReceiptRefusal::UnknownBase { base, base_ref }));
        }
        let stored = ledger.put_receipt(&receipt).map_err(land_body)?;
        Ok(ReceiptRecord {
            base_current: stored.base == current,
            receipt: stored,
            base_ref,
        })
    }

    fn ledger(&self) -> Result<Arc<Ledger>, ErrorBody> {
        let mut slot = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(ledger) = slot.as_ref() {
            return Ok(Arc::clone(ledger));
        }
        let internal = |why: String| ErrorBody::new(ErrorCode::Internal, why);
        std::fs::create_dir_all(&self.config.data_dir).map_err(|e| internal(e.to_string()))?;
        let ledger = Ledger::open(&self.config.metadata_path()).map_err(land_body)?;
        let ledger = Arc::new(ledger);
        *slot = Some(Arc::clone(&ledger));
        Ok(ledger)
    }
}

fn refusal_body(refusal: ReceiptRefusal) -> ErrorBody {
    let code = match refusal {
        ReceiptRefusal::ChangeClosed { .. } | ReceiptRefusal::StaleHead { .. } => {
            ErrorCode::Refused
        }
        ReceiptRefusal::MalformedCheckSet(_)
        | ReceiptRefusal::WrongCheckSet { .. }
        | ReceiptRefusal::UnknownBase { .. } => ErrorCode::InvalidArgument,
    };
    ErrorBody::new(code, format!("receipt refused: {refusal}"))
}

fn land_body(err: LandError) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, err.to_string())
}
