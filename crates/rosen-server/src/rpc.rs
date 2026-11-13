//! Request dispatch. Feature work adds one arm per new [`Request`] variant.

use crate::changes::{ChangeError, ChangeService};
use crate::claims::ClaimCall;
use crate::history::HistoryCall;
use crate::reviews::ReviewCall;
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
        Request::PoolSize { size } => state.pool.set_size(size).await,
        Request::PoolTake { tree, dest } => state.pool.take(tree, dest).await,
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
        Request::ChangeRebase { change } => crate::rebase::handle(state, change).await,
        Request::EditRegions { change } => crate::overlaps::edits::handle(state, change).await,
        Request::ChangeAbandon { change } => crate::claims::abandon(state, change).await,
        Request::ClaimPaths(req) => crate::claims::handle(state, ClaimCall::Claim(req)).await,
        Request::SharePaths(req) => crate::claims::handle(state, ClaimCall::Share(req)).await,
        Request::WaitPaths(req) => crate::claims::handle(state, ClaimCall::Wait(req)).await,
        Request::ReleasePaths(req) => crate::claims::handle(state, ClaimCall::Release(req)).await,
        Request::ClaimList(filter) => {
            crate::claims::handle(state, ClaimCall::List(filter.change)).await
        }
        Request::Overlaps(query) => crate::overlaps::handle(state, query).await,
        Request::PutObjects(upload) => crate::snapshots::put_objects(state, upload).await,
        Request::Snapshot(request) => crate::snapshots::snapshot(state, request).await,
        Request::AckEvents { subscriber, seq } => {
            crate::events::acknowledge(state, subscriber, seq).await
        }
        Request::ReviewApprove(req) => {
            crate::reviews::handle(state, ReviewCall::Approve(req)).await
        }
        Request::ReviewApprovals { change } => {
            crate::reviews::handle(state, ReviewCall::Approvals(change)).await
        }
        Request::ReviewComment(req) => {
            crate::reviews::handle(state, ReviewCall::Comment(req)).await
        }
        Request::ReviewResolve(req) => {
            crate::reviews::handle(state, ReviewCall::Resolve(req)).await
        }
        Request::ReviewList(filter) => {
            crate::reviews::handle(state, ReviewCall::List(filter)).await
        }
        Request::ReceiptSubmit(req) => crate::receipts::submit(state, req).await,
        Request::Land { change } => crate::landing::service::land(state, change).await,
        Request::GetObjects(query) => crate::objects::get(state, query).await,
        Request::EnsureObjects(ensure) => crate::objects::ensure(state, ensure).await,
        Request::Log(req) => crate::history::handle(state, HistoryCall::Log(req)).await,
        Request::Blame(req) => crate::history::handle(state, HistoryCall::Blame(req)).await,
        Request::Diff(req) => crate::history::handle(state, HistoryCall::Diff(req)).await,
        Request::MergeBase { a, b } => {
            crate::history::handle(state, HistoryCall::MergeBase { a, b }).await
        }
        Request::ReadFile(req) => crate::history::handle(state, HistoryCall::ReadFile(req)).await,
        Request::MergeResult { change } => {
            crate::history::handle(state, HistoryCall::MergeResult { change }).await
        }
        Request::WorkspaceWrite(req) => crate::workspaces::write(state, req).await,
        Request::ConfigShow(query) => crate::project_config::show(state, query).await,
        Request::Channel(call) => crate::channel::handle(state, call).await,
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
    let network = state.network_status();
    ServerStatus {
        server: SERVER_NAME.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        pid: std::process::id(),
        endpoint: state.config.endpoint.to_string(),
        data_dir: state.config.data_dir.display().to_string(),
        uptime_ms: u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        open_connections: state.open_connections(),
        last_event: state.events.last_seq(),
        listen: network.as_ref().map(|(address, _)| address.to_string()),
        fingerprint: network.map(|(_, fingerprint)| fingerprint),
        upstream: None,
    }
}
