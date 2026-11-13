//! `rosen resolve` (SPEC R5, R10): marks the stored conflicts in some files of
//! a change resolved, so the landing gate (R8) stops refusing it.
//!
//! Resolving trusts the owner's edit; it does not rewrite the change's head.
//! The next land or parent update merges the head again, and a region that
//! still fails to merge is stored open again (`ConflictSet::record_merge`),
//! so resolving without fixing the file can never land a silent pick.

use std::sync::Arc;

use rosen_core::ChangeId;
use rosen_protocol::{
    ConflictsResolved, ErrorBody, ErrorCode, Outcome, ResolveConflicts, ResolvedPath, Response,
};
use rusqlite::{Connection, TransactionBehavior};

use crate::changes::{self, ChangeError};
use crate::rpc::change_error_code;
use crate::{conflicts, ServerState};

/// Runs on the blocking pool: SQLite I/O must not stall the async workers.
pub async fn handle(state: &Arc<ServerState>, request: ResolveConflicts) -> Outcome {
    let state = Arc::clone(state);
    let work = move || {
        let answer = resolve(&state, request)?;
        if answer.resolved.iter().any(|path| path.conflicts > 0) {
            // Parent updates skip a stacked change while it has open
            // conflicts (R8); resume them now rather than at the next snapshot.
            crate::stacks::refresh(&state);
        }
        Ok(answer)
    };
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(answer)) => Outcome::Ok(Response::ConflictsResolved(answer)),
        Ok(Err(body)) => Outcome::Error(body),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

fn resolve(state: &ServerState, request: ResolveConflicts) -> Result<ConflictsResolved, ErrorBody> {
    let paths = normalized(&request.paths)?;
    let outcome = state
        .changes
        .with_conn(|conn| resolve_in(conn, &request.change, &paths));
    match outcome {
        Ok(answer) => answer,
        Err(err) => Err(ErrorBody::new(change_error_code(&err), err.to_string())),
    }
}

/// The outer error is the metadata store's; the inner one is a refusal that
/// rolled the transaction back.
fn resolve_in(
    conn: &mut Connection,
    change: &str,
    paths: &[String],
) -> Result<Result<ConflictsResolved, ErrorBody>, ChangeError> {
    // Immediate: the landing ledger writes conflicts on its own connection,
    // and a deferred read-then-write here could lose that race as SQLITE_BUSY.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = changes::resolve(&tx, change)?;
    let state = changes::get(&tx, id)?.change.state;
    if state.is_closed() {
        let why = format!(
            "change {id} is {state}; only an open or ready change has conflicts to resolve"
        );
        return Ok(Err(ErrorBody::new(ErrorCode::Refused, why)));
    }
    let mut set = conflicts::load(&tx, &id.to_string()).map_err(corrupt)?;
    let unknown: Vec<&str> = paths
        .iter()
        .filter(|path| !set.all().iter().any(|c| &c.path == *path))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Ok(Err(no_conflicts(id, &unknown, &set)));
    }
    let resolved = paths
        .iter()
        .map(|path| ResolvedPath {
            conflicts: set.resolve_path(path),
            path: path.clone(),
        })
        .collect();
    conflicts::save(&tx, &id.to_string(), &set).map_err(corrupt)?;
    tx.commit()?;
    Ok(Ok(ConflictsResolved {
        change: id,
        resolved,
        open_paths: set.open_paths().into_iter().map(str::to_owned).collect(),
    }))
}

/// Accepts `\` separators and a leading `./`, as a Windows shell types them,
/// and drops repeats so each path is answered once.
fn normalized(paths: &[String]) -> Result<Vec<String>, ErrorBody> {
    let mut out: Vec<String> = Vec::with_capacity(paths.len());
    for raw in paths {
        let slashed = raw.trim().replace('\\', "/");
        let mut path = slashed.as_str();
        while let Some(rest) = path.strip_prefix("./") {
            path = rest;
        }
        if path.is_empty() {
            let why = format!("{raw:?} is not a file path");
            return Err(ErrorBody::new(ErrorCode::InvalidArgument, why));
        }
        if !out.iter().any(|seen| seen == path) {
            out.push(path.to_owned());
        }
    }
    if out.is_empty() {
        let why = "name at least one file whose conflicts are resolved";
        return Err(ErrorBody::new(ErrorCode::InvalidArgument, why));
    }
    Ok(out)
}

fn no_conflicts(id: ChangeId, unknown: &[&str], set: &rosen_core::ConflictSet) -> ErrorBody {
    let open = set.open_paths();
    let hint = if open.is_empty() {
        "it has no open conflicts".to_owned()
    } else {
        format!("its open conflicts are in: {}", open.join(", "))
    };
    let why = format!(
        "change {id} has no conflict stored in {}; {hint}",
        unknown.join(", ")
    );
    ErrorBody::new(ErrorCode::NotFound, why)
}

fn corrupt(err: conflicts::ConflictStoreError) -> ChangeError {
    match err {
        conflicts::ConflictStoreError::Sql(sql) => ChangeError::Store(sql),
        other => ChangeError::Corrupt(other.to_string()),
    }
}
