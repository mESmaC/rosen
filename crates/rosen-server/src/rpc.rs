//! Request dispatch. Feature work adds one arm per new [`Request`] variant.

use crate::changes::{ChangeError, ChangeService};
use crate::claims::ClaimCall;
use crate::{ServerState, SERVER_NAME};
use rosen_protocol::{
    ErrorBody, ErrorCode, Outcome, Request, Response, ServerStatus, PROTOCOL_VERSION,
};
use std::sync::Arc;

/// Runs one request to completion.
pub async fn dispatch(state: &Arc<ServerState>, request: Request) -> Outcome {
    if state.is_shutting_down() {
        return Outcome::Error(ErrorBody::new(
            ErrorCode::ShuttingDown,
            "the server is shutting down",
        ));
    }
    match request {
        Request::Ping => Outcome::Ok(Response::Pong),
        Request::Status => Outcome::Ok(Response::Status(status(state))),
        Request::Shutdown => {
            state.request_shutdown();
            Outcome::Ok(Response::ShuttingDown)
        }
        Request::Import {
            source,
            branch,
            mirror,
        } => crate::import::service::handle(state, source, branch, mirror).await,
        Request::ChangeNew(params) => {
            on_changes(state, move |c| c.create(&params).map(Response::Change)).await
        }
        Request::ChangeList(filter) => {
            on_changes(state, move |c| c.list(&filter).map(Response::Changes)).await
        }
        Request::ChangeShow { change } => {
            on_changes(state, move |c| c.show(&change).map(Response::Change)).await
        }
        Request::ChangeAbandon { change } => crate::claims::abandon(state, change).await,
        Request::ClaimPaths(req) => crate::claims::handle(state, ClaimCall::Claim(req)).await,
        Request::SharePaths(req) => crate::claims::handle(state, ClaimCall::Share(req)).await,
        Request::WaitPaths(req) => crate::claims::handle(state, ClaimCall::Wait(req)).await,
        Request::ReleasePaths(req) => crate::claims::handle(state, ClaimCall::Release(req)).await,
        Request::ClaimList(filter) => {
            crate::claims::handle(state, ClaimCall::List(filter.change)).await
        }
        Request::PutObjects(upload) => crate::snapshots::put_objects(state, upload).await,
        Request::Snapshot(request) => crate::snapshots::snapshot(state, request).await,
    }
}

/// Runs a change-registry call on the blocking pool: SQLite I/O must not
/// stall the async workers that carry every other connection.
async fn on_changes(
    state: &Arc<ServerState>,
    work: impl FnOnce(&ChangeService) -> Result<Response, ChangeError> + Send + 'static,
) -> Outcome {
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || work(&state.changes)).await {
        Ok(Ok(response)) => Outcome::Ok(response),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(change_error_code(&err), err.to_string())),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

pub(crate) fn change_error_code(err: &ChangeError) -> ErrorCode {
    match err {
        ChangeError::Refused(rosen_core::Error::InvalidPath(_)) => ErrorCode::InvalidArgument,
        ChangeError::NotFound(_) => ErrorCode::NotFound,
        ChangeError::InvalidId(_) | ChangeError::Ambiguous(_) => ErrorCode::InvalidArgument,
        ChangeError::ParentClosed { .. } | ChangeError::Refused(_) | ChangeError::Base(_) => {
            ErrorCode::Refused
        }
        ChangeError::Randomness(_)
        | ChangeError::Corrupt(_)
        | ChangeError::Io(_)
        | ChangeError::Store(_) => ErrorCode::Internal,
    }
}

fn status(state: &ServerState) -> ServerStatus {
    ServerStatus {
        server: SERVER_NAME.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        pid: std::process::id(),
        endpoint: state.config.endpoint.to_string(),
        data_dir: state.config.data_dir.display().to_string(),
        uptime_ms: u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        open_connections: state.open_connections(),
        last_event: state.events.last_seq(),
    }
}
