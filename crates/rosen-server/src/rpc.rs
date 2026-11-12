//! Request dispatch. Feature work adds one arm per new [`Request`] variant.

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
    }
}

fn status(state: &ServerState) -> ServerStatus {
    ServerStatus {
        server: SERVER_NAME.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        pid: std::process::id(),
        endpoint: state.endpoint.to_string(),
        uptime_ms: u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        open_connections: state.open_connections(),
        last_event: state.events.last_seq(),
    }
}
