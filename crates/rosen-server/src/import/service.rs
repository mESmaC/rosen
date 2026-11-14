//! The `import` request: runs an import against the server's stores and sets
//! the base.

use std::sync::Arc;

use rosen_protocol::{ErrorBody, ErrorCode, ImportSummary, Outcome, Response};
use rosen_store::LooseObjectStore;

use super::{import_repository, ImportError, ImportReport, ImportRequest};
use crate::landing::{InitBase, Ledger};
use crate::mirror::MirrorStore;
use crate::ServerState;

/// Answers `Request::Import`. Import blocks on git and disk for as long as
/// the repository takes to copy, so it runs on a blocking thread and never
/// stalls the connections the async runtime serves.
pub async fn handle(
    state: &Arc<ServerState>,
    source: String,
    branch: Option<String>,
    mirror: Option<String>,
) -> Outcome {
    let state = Arc::clone(state);
    let request = ImportRequest { source, branch };
    let joined =
        tokio::task::spawn_blocking(move || run(&state, &request, mirror.as_deref())).await;
    match joined {
        Ok(Ok(report)) => Outcome::Ok(Response::Imported(summary(report))),
        Ok(Err(error)) => Outcome::Error(error_body(&error)),
        Err(panic) => Outcome::Error(ErrorBody::new(
            ErrorCode::Internal,
            format!("import stopped unexpectedly: {panic}"),
        )),
    }
}

fn run(
    state: &ServerState,
    request: &ImportRequest,
    mirror: Option<&str>,
) -> Result<ImportReport, ImportError> {
    std::fs::create_dir_all(&state.config.data_dir)?;
    let ledger = Ledger::open(&state.config.metadata_path())
        .map_err(|error| ImportError::Store(error.to_string()))?;
    let store = LooseObjectStore::open(state.config.objects_dir())
        .map_err(|error| ImportError::Store(error.to_string()))?;
    let report = import_repository(request, state.config.import_scratch_dir(), &store)?;
    // Set only after every object is stored, so the base never names a
    // missing commit. A project that already has a base refuses here; the
    // objects copied meanwhile are harmless, since the store is immutable
    // and a rerun of the same import finds them already stored.
    let outcome = ledger
        .init_base(&report.branch, &report.base_commit)
        .map_err(|error| ImportError::Store(error.to_string()))?;
    match outcome {
        InitBase::Created => {
            // Named only once this import owns the base, so a refused
            // import never redirects an existing project's mirror.
            if let Some(remote) = mirror {
                MirrorStore::open(&state.config.metadata_path())
                    .and_then(|store| store.set_remote(remote))
                    .map_err(|error| ImportError::Store(error.to_string()))?;
            }
            // Import publishes no `landed` event, so wake the mirror task
            // directly: the imported history reaches the remote now, not at
            // the first land (R1). This also covers a remote named only by
            // the imported base's rosen.toml.
            state.mirror_wake.notify_one();
            Ok(report)
        }
        InitBase::Exists { name, commit } => Err(ImportError::BaseAlreadySet {
            branch: name,
            base_commit: commit,
        }),
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
        ImportError::BaseAlreadySet { .. } => ErrorCode::Refused,
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
