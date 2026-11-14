//! `receipt submit` (SPEC R8 step 1, D9): checkers run the pre-land check
//! set outside Rosen and submit a receipt. The server validates its binding
//! and stores it in the landing ledger, where a land looks it up.
//!
//! Validation here gives the checker an immediate answer. It is not what
//! keeps a land safe: a land only accepts a receipt for the change's head
//! at land time and a base the base ref has held, so a head that moves
//! between this check and the store leaves a receipt no land will use.
//!
//! Every receipt is signed by its submitter's machine (SPEC.md §2, R14).
//! A remote machine's daemon signs it on the way here, and the server takes
//! the machine from the connection's credential, never from the request. A
//! receipt submitted on the server's own machine is signed with the
//! server's key. The ledger verifies the signature on store and on land.

use std::sync::{Arc, Mutex, PoisonError};

use rosen_core::{Receipt, ReceiptRefusal};
use rosen_protocol::{
    ErrorBody, ErrorCode, Outcome, ReceiptRecord, ReceiptSignature, Response, SubmitReceipt,
};

use crate::landing::{KeyBinding, LandError, Ledger, SignedReceipt};
use crate::machines::signing::{self, MachineKey, SERVER_MACHINE};
use crate::rpc::change_error_code;
use crate::{ServerConfig, ServerState};

/// Stores receipts in the landing ledger's tables of the metadata store.
pub struct ReceiptService {
    config: ServerConfig,
    /// Opened on first use, so a server that never takes a receipt never
    /// creates the database.
    ledger: Mutex<Option<Arc<Ledger>>>,
    /// The server's own machine key, loaded and registered on first use.
    server_key: Mutex<Option<Arc<MachineKey>>>,
}

/// How the submitter reached the server, which decides its machine.
pub enum Origin {
    /// A client on the server's own machine, over the local transport.
    Local,
    /// A machine's daemon over the network, with this credential.
    Machine(String),
}

/// Runs a submit on the blocking pool: SQLite I/O must not stall the async
/// workers that carry every other connection.
pub async fn submit(state: &Arc<ServerState>, request: SubmitReceipt, origin: Origin) -> Outcome {
    let state = Arc::clone(state);
    let work = move || state.receipts.submit(&state, request, origin);
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
            server_key: Mutex::new(None),
        }
    }

    fn submit(
        &self,
        state: &ServerState,
        request: SubmitReceipt,
        origin: Origin,
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
        let signature = request.signature;
        // The check set is the one the base's configuration names, so a
        // change cannot rename the check set its own receipt must cover.
        let (base_ref, in_force) = state
            .project_config
            .at_base()
            .map_err(|why| ErrorBody::new(ErrorCode::Internal, why))?
            .ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::Refused,
                    "the project has no base yet; import a repository with `rosen import`",
                )
            })?;
        let current = in_force.commit;
        let required = &in_force.config.checks.pre_land.name;
        receipt
            .check_binding(&change.head, change.state, required)
            .map_err(refusal_body)?;
        let ledger = self.ledger()?;
        if ledger
            .base_seq(&base_ref, &receipt.base)
            .map_err(land_body)?
            .is_none()
        {
            let base = receipt.base;
            return Err(refusal_body(ReceiptRefusal::UnknownBase { base, base_ref }));
        }
        let signed = self.signed(state, &ledger, receipt, origin, signature)?;
        let stored = ledger.put_receipt(&signed).map_err(land_body)?;
        Ok(ReceiptRecord {
            base_current: stored.base == current,
            receipt: stored,
            base_ref,
        })
    }

    /// Attaches the submitting machine's signature. A remote machine's must
    /// come from its daemon, with the key the machine is bound to.
    fn signed(
        &self,
        state: &ServerState,
        ledger: &Ledger,
        receipt: Receipt,
        origin: Origin,
        signature: Option<ReceiptSignature>,
    ) -> Result<SignedReceipt, ErrorBody> {
        let credential = match origin {
            Origin::Local => {
                let key = self.server_key(ledger)?;
                let signature = key.sign(&receipt.signing_payload()).signature;
                let machine = SERVER_MACHINE.to_owned();
                return Ok(SignedReceipt {
                    receipt,
                    machine,
                    signature,
                });
            }
            Origin::Machine(credential) => credential,
        };
        let refused =
            |why: &str| ErrorBody::new(ErrorCode::Refused, format!("receipt refused: {why}"));
        let machine = crate::machines::machine_of(&state.config.data_dir, &credential)
            .map_err(|err| ErrorBody::new(ErrorCode::Internal, err.to_string()))?
            .ok_or_else(|| refused("the machine credential is not accepted"))?;
        let signature = signature
            .ok_or_else(|| refused("it is not signed; submit it through the machine's daemon"))?;
        // Proof of possession first: a signature its own key does not verify
        // must never bind that key to the machine.
        let payload = receipt.signing_payload();
        if !signing::verify(&signature.public_key, &payload, &signature.signature) {
            return Err(refused("its signature does not verify"));
        }
        let binding = ledger
            .bind_signing_key(&machine, &signature.public_key)
            .map_err(land_body)?;
        if binding == KeyBinding::Conflict {
            return Err(refused("it is signed with a key other than this machine's"));
        }
        Ok(SignedReceipt {
            receipt,
            machine,
            signature: signature.signature,
        })
    }

    fn server_key(&self, ledger: &Ledger) -> Result<Arc<MachineKey>, ErrorBody> {
        let mut slot = self
            .server_key
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(key) = slot.as_ref() {
            return Ok(Arc::clone(key));
        }
        let internal = |why: String| ErrorBody::new(ErrorCode::Internal, why);
        let key = MachineKey::load_or_create(&self.config.data_dir)
            .map_err(|err| internal(err.to_string()))?;
        ledger
            .set_server_key(&key.public_key())
            .map_err(land_body)?;
        Ok(Arc::clone(slot.insert(Arc::new(key))))
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
    match err {
        LandError::InvalidReceipt(_) => ErrorBody::new(ErrorCode::Refused, err.to_string()),
        err => ErrorBody::new(ErrorCode::Internal, err.to_string()),
    }
}
