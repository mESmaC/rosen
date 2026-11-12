//! The `import` request: runs an import against the server's stores and sets
//! the base.

use std::sync::Arc;

use rosen_core::ObjectId;
use rosen_protocol::{ErrorBody, ErrorCode, ImportSummary, Outcome, Response};
use rosen_store::LooseObjectStore;

use super::{import_repository, ImportError, ImportReport, ImportRequest};
use crate::ServerState;

/// The project's base ref, as import needs it.
pub trait BaseRef {
    /// The base's name and commit, if the project has one.
    fn current(&self) -> Result<Option<(String, ObjectId)>, String>;
    /// Sets the base in one transaction, only if the project has none yet.
    /// Returns the base that already exists when it refuses.
    fn initialise(
        &self,
        branch: &str,
        commit: &ObjectId,
    ) -> Result<Option<(String, ObjectId)>, String>;
}

/// Answers `Request::Import`. Import blocks on git and disk for as long as
/// the repository takes to copy, so it runs on a blocking thread and never
/// stalls the connections the async runtime serves.
pub async fn handle(state: &Arc<ServerState>, source: String, branch: Option<String>) -> Outcome {
    let state = Arc::clone(state);
    let request = ImportRequest { source, branch };
    let joined = tokio::task::spawn_blocking(move || run(&state, &request)).await;
    match joined {
        Ok(Ok(report)) => Outcome::Ok(Response::Imported(summary(report))),
        Ok(Err(error)) => Outcome::Error(error_body(&error)),
        Err(panic) => Outcome::Error(ErrorBody::new(
            ErrorCode::Internal,
            format!("import stopped unexpectedly: {panic}"),
        )),
    }
}

fn run(state: &ServerState, request: &ImportRequest) -> Result<ImportReport, ImportError> {
    let base = open_base_ref(state)?;
    // Checked first so a project with a base is refused before any copying;
    // `initialise` checks again, atomically, in case another import won.
    refuse_if_set(base.current().map_err(ImportError::Store)?)?;
    let store = LooseObjectStore::open(state.config.objects_dir())
        .map_err(|error| ImportError::Store(error.to_string()))?;
    let report = import_repository(request, state.config.import_scratch_dir(), &store)?;
    let existing = base
        .initialise(&report.branch, &report.base_commit)
        .map_err(ImportError::Store)?;
    refuse_if_set(existing)?;
    Ok(report)
}

fn refuse_if_set(base: Option<(String, ObjectId)>) -> Result<(), ImportError> {
    match base {
        Some((branch, base_commit)) => Err(ImportError::BaseAlreadySet {
            branch,
            base_commit,
        }),
        None => Ok(()),
    }
}

fn summary(report: ImportReport) -> ImportSummary {
    ImportSummary {
        branch: report.branch,
        base_commit: report.base_commit.to_hex(),
        objects_reachable: report.objects_reachable as u64,
        objects_written: report.objects_written as u64,
    }
}

fn error_body(error: &ImportError) -> ErrorBody {
    let code = match error {
        ImportError::BaseAlreadySet { .. } => ErrorCode::BaseAlreadySet,
        ImportError::Git { .. }
        | ImportError::UnsupportedObjectFormat(_)
        | ImportError::NoDefaultBranch
        | ImportError::BranchNotFound(_) => ErrorCode::InvalidArgument,
        ImportError::Protocol(_)
        | ImportError::IdMismatch { .. }
        | ImportError::Store(_)
        | ImportError::Io(_) => ErrorCode::Internal,
    };
    ErrorBody::new(code, error.to_string())
}
