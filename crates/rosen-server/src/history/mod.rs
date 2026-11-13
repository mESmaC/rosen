//! History queries (SPEC R11): log, path history, blame, diff, merge base,
//! file read and a change's merge result, answered from the object store
//! with no checkout.
//!
//! Commits are immutable, so everything derived from one commit is cached
//! for good in the metadata store ([`index`]): each commit's parents and
//! summary, the files it changed against its first parent, and the blame of
//! each file it changed. A query fills the cache for the commits it walks,
//! so the next query over the same history reads rows instead of objects.
//! A change is not cached: its head moves, so it is read fresh and laid
//! over the cached history of the base commit its stack sits on.

mod blame;
mod diff;
mod index;
mod log;
mod merge;
mod revision;
mod trees;

pub(crate) use merge::merge_for_land;

use crate::changes::ChangeError;
use crate::ServerState;
use index::Index;
use revision::Revision;
use rosen_protocol::{
    BlameRequest, DiffRequest, ErrorBody, ErrorCode, LogRequest, MergeBaseAnswer, Outcome,
    ReadRequest, Response,
};
use rosen_store::{LooseObjectStore, StoreError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error(transparent)]
    Change(#[from] ChangeError),
    /// The revision, path or object named does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The request is malformed, such as a log of a bare tree.
    #[error("{0}")]
    Invalid(String),
    /// The project has no base yet, or the change cannot be merged.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("history index: {0}")]
    Index(#[from] rusqlite::Error),
    #[error(transparent)]
    Core(#[from] rosen_core::Error),
}

impl HistoryError {
    fn code(&self) -> ErrorCode {
        match self {
            HistoryError::Change(err) => crate::rpc::change_error_code(err),
            HistoryError::NotFound(_) | HistoryError::Store(StoreError::NotFound(_)) => {
                ErrorCode::NotFound
            }
            HistoryError::Invalid(_) | HistoryError::Store(StoreError::WrongKind { .. }) => {
                ErrorCode::InvalidArgument
            }
            HistoryError::Refused(_) => ErrorCode::Refused,
            HistoryError::Store(_) | HistoryError::Index(_) | HistoryError::Core(_) => {
                ErrorCode::Internal
            }
        }
    }
}

pub type HistoryResult<T> = Result<T, HistoryError>;

/// One history query, as received from a client.
pub enum HistoryCall {
    Log(LogRequest),
    Blame(BlameRequest),
    Diff(DiffRequest),
    MergeBase { a: String, b: String },
    ReadFile(ReadRequest),
    MergeResult { change: String },
}

/// Server-wide history state: whether the cache tables exist yet.
#[derive(Default)]
pub struct HistoryService {
    schema_ready: AtomicBool,
}

impl HistoryService {
    pub fn new() -> Self {
        Self::default()
    }
}

/// What every query reads from: the object store, the cache, and the change
/// registry through the server state.
pub(crate) struct Ctx<'a> {
    pub state: &'a ServerState,
    pub store: LooseObjectStore,
    pub index: Index,
}

/// Runs a query on the blocking pool: object and SQLite reads must not stall
/// the async workers that carry every other connection.
pub async fn handle(state: &Arc<ServerState>, call: HistoryCall) -> Outcome {
    let permit = match Arc::clone(&state.workspace_io).acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return Outcome::Error(ErrorBody::new(
                ErrorCode::ShuttingDown,
                "workspace I/O closed",
            ))
        }
    };
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        run(&state, call)
    })
    .await
    {
        Ok(Ok(response)) => Outcome::Ok(response),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(err.code(), err.to_string())),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

fn run(state: &ServerState, call: HistoryCall) -> HistoryResult<Response> {
    let ctx = open(state)?;
    Ok(match call {
        HistoryCall::Log(req) => Response::Log(log::log(&ctx, &req)?),
        HistoryCall::Blame(req) => Response::Blame(blame::blame(&ctx, &req)?),
        HistoryCall::Diff(req) => Response::Diff(diff::diff(&ctx, &req)?),
        HistoryCall::MergeBase { a, b } => Response::MergeBase(merge_base(&ctx, &a, &b)?),
        HistoryCall::ReadFile(req) => {
            let tree = Revision::resolve(&ctx, req.at.as_deref())?.tree(&ctx)?;
            Response::File(trees::read(&ctx.store, tree, &req.path)?)
        }
        HistoryCall::MergeResult { change } => {
            Response::MergeResult(merge::merge_result(&ctx, &change)?)
        }
    })
}

fn open(state: &ServerState) -> HistoryResult<Ctx<'_>> {
    let store = LooseObjectStore::open(state.config.objects_dir())?;
    let fresh = !state.history.schema_ready.load(Ordering::Acquire);
    let index = Index::open(&state.config.metadata_path(), fresh)?;
    state.history.schema_ready.store(true, Ordering::Release);
    Ok(Ctx {
        state,
        store,
        index,
    })
}

/// A change counts from the base commit its stack sits on: its own
/// snapshots are not on the base's line, and after a server rebase they
/// no longer lead to the commit its head was merged onto.
fn merge_base(ctx: &Ctx, a: &str, b: &str) -> HistoryResult<MergeBaseAnswer> {
    let a = Revision::resolve(ctx, Some(a))?.history_commit(ctx)?;
    let b = Revision::resolve(ctx, Some(b))?.history_commit(ctx)?;
    let graph = index::Graph::new(ctx);
    let all = rosen_core::history::merge_bases(&graph, a, b)
        .map_err(|err| graph.take_failure().unwrap_or(err.into()))?;
    Ok(MergeBaseAnswer {
        merge_base: all.first().copied(),
        all,
    })
}
