//! Review threads (SPEC R7, D12): comments anchored to a file, a line and the
//! change head, forming threads that are open or resolved.
//!
//! A new thread's anchor is checked against the head's tree in the object
//! store before it is stored: the path must be a file there and the line must
//! exist. That read happens outside the metadata transaction, so the insert
//! re-reads the head and retries if a snapshot moved it in between. Events are
//! published only after the transaction commits.

mod anchor;
mod store;

pub use store::ensure_schema;

use crate::changes::{self, ChangeError};
use crate::ServerState;
use rosen_core::review::{anchor_path, check_body};
use rosen_core::{AgentId, ChangeId, ObjectId};
use rosen_protocol::{
    CommentRequest, CommentTarget, ErrorBody, ErrorCode, EventKind, Outcome, ResolveRequest,
    Response, ReviewThread, ThreadFilter,
};
use rosen_store::LooseObjectStore;
use rusqlite::Connection;
use serde_json::json;
use std::sync::Arc;

/// How often a new thread re-checks its anchor when snapshots keep moving the
/// head between the check and the insert. Snapshots arrive seconds apart
/// (R2), so a second attempt almost always succeeds.
const HEAD_ATTEMPTS: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error(transparent)]
    Change(#[from] ChangeError),
    #[error("no review thread {0}")]
    ThreadNotFound(u64),
    /// A malformed anchor or body, or a thread named with the wrong change.
    #[error("{0}")]
    Invalid(String),
    /// The change's state or head forbids the comment.
    #[error("{0}")]
    Refused(String),
    /// The object store or a stored row could not be read.
    #[error("{0}")]
    Store(String),
}

impl From<rusqlite::Error> for ReviewError {
    fn from(err: rusqlite::Error) -> Self {
        ReviewError::Change(ChangeError::Store(err))
    }
}

impl From<rosen_core::Error> for ReviewError {
    fn from(err: rosen_core::Error) -> Self {
        ReviewError::Invalid(err.to_string())
    }
}

/// One review operation, as received from a client.
pub enum ReviewCall {
    Comment(CommentRequest),
    Resolve(ResolveRequest),
    List(ThreadFilter),
}

/// Runs a review operation on the blocking pool: SQLite and object store I/O
/// must not stall the async workers that carry every other connection.
pub async fn handle(state: &Arc<ServerState>, call: ReviewCall) -> Outcome {
    let state = Arc::clone(state);
    let joined = tokio::task::spawn_blocking(move || run(&state, call)).await;
    match joined {
        Ok(Ok(response)) => Outcome::Ok(response),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(error_code(&err), err.to_string())),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

fn run(state: &ServerState, call: ReviewCall) -> Result<Response, ReviewError> {
    match call {
        ReviewCall::Comment(req) => comment(state, req).map(Response::ReviewThread),
        ReviewCall::Resolve(req) => {
            let (thread, changed) = on_conn(state, |c| store::resolve(c, req.thread, &req.by))?;
            if changed {
                publish(state, &thread, "resolve", &req.by);
            }
            Ok(Response::ReviewThread(thread))
        }
        ReviewCall::List(filter) => {
            let threads = on_conn(state, |c| {
                let id = changes::resolve(c, &filter.change)?;
                store::list(c, id, filter.state)
            })?;
            Ok(Response::ReviewThreads(threads))
        }
    }
}

fn comment(state: &ServerState, req: CommentRequest) -> Result<ReviewThread, ReviewError> {
    check_body(&req.body)?;
    let id = on_conn(state, |c| Ok(changes::resolve(c, &req.change)?))?;
    match req.target {
        CommentTarget::Line { path, line } => {
            let path = anchor_path(&path)?;
            let thread = start_thread(state, id, req.head, &path, line, &req.author, &req.body)?;
            publish(state, &thread, "comment", &req.author);
            Ok(thread)
        }
        CommentTarget::Thread { thread } => {
            let (thread, reopened) = on_conn(state, |c| {
                store::reply(c, id, req.head, thread, &req.author, &req.body)
            })?;
            let action = if reopened { "reopen" } else { "comment" };
            publish(state, &thread, action, &req.author);
            Ok(thread)
        }
    }
}

/// Checks the anchor against the change's current head, then stores the
/// thread if the head is still the same.
fn start_thread(
    state: &ServerState,
    change: ChangeId,
    expected_head: Option<ObjectId>,
    path: &str,
    line: u32,
    author: &AgentId,
    body: &str,
) -> Result<ReviewThread, ReviewError> {
    let objects = LooseObjectStore::open(state.config.objects_dir())
        .map_err(|err| ReviewError::Store(err.to_string()))?;
    for _ in 0..HEAD_ATTEMPTS {
        let head = on_conn(state, |c| {
            let record = changes::get(c, change)?;
            check_reviewable(&record)?;
            Ok(record.change.head)
        })?;
        check_head(expected_head, head)?;
        anchor::check(&objects, head, path, line)?;
        let new = store::NewThread {
            change,
            head,
            path,
            line,
            author,
            body,
        };
        if let Some(thread) = on_conn(state, |c| store::open_thread(c, &new))? {
            return Ok(thread);
        }
    }
    Err(ReviewError::Refused(format!(
        "change {change} kept getting new heads while the comment was checked; try again"
    )))
}

/// Comments go on changes still under review. A landed or abandoned change
/// will never land again from this head, so a new comment could not be acted on.
fn check_reviewable(record: &rosen_protocol::ChangeRecord) -> Result<(), ReviewError> {
    let change = &record.change;
    if change.state.is_closed() {
        return Err(ReviewError::Refused(format!(
            "change {} is {}; only open and ready changes take review comments",
            change.id, change.state
        )));
    }
    Ok(())
}

/// Refuses a comment written against a head the change no longer has: its
/// line numbers may not match the file any more.
fn check_head(expected: Option<ObjectId>, actual: ObjectId) -> Result<(), ReviewError> {
    match expected {
        Some(read) if read != actual => Err(ReviewError::Refused(format!(
            "the change's head is now {actual}, not {read}; read the new head and comment again"
        ))),
        _ => Ok(()),
    }
}

/// A `review` event for the thread's change. R9 delivers it to the change's
/// owner and reviewers.
fn publish(state: &ServerState, thread: &ReviewThread, action: &str, by: &AgentId) {
    let detail = json!({
        "thread": thread.id,
        "action": action,
        "state": thread.state,
        "path": thread.path,
        "line": thread.line,
        "head": thread.head,
        "by": by,
    });
    state
        .events
        .publish(EventKind::Review, Some(thread.change.to_string()), detail);
}

/// Runs review store work on the change registry's connection, which also
/// opens and migrates the metadata store on first use.
fn on_conn<T>(
    state: &ServerState,
    work: impl FnOnce(&mut Connection) -> Result<T, ReviewError>,
) -> Result<T, ReviewError> {
    state.changes.with_conn(|conn| Ok(work(conn)))?
}

fn error_code(err: &ReviewError) -> ErrorCode {
    match err {
        ReviewError::Change(change) => crate::rpc::change_error_code(change),
        ReviewError::ThreadNotFound(_) => ErrorCode::NotFound,
        ReviewError::Invalid(_) => ErrorCode::InvalidArgument,
        ReviewError::Refused(_) => ErrorCode::Refused,
        ReviewError::Store(_) => ErrorCode::Internal,
    }
}
