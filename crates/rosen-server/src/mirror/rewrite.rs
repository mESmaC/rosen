//! Operator-only repair of unmirrored landed history (SPEC R1, #385).
mod metadata;
mod objects;
pub(super) mod remote;

use std::sync::Arc;

use anyhow::{bail, Result};
use rosen_protocol::{ErrorBody, ErrorCode, HistoryRewrite, Outcome, Response};
use rosen_store::LooseObjectStore;

use crate::ServerState;

/// The exclusive permit pauses requests and waits for a push/recovery pass
/// already in flight. Keep it in a detached task, so disconnecting the
/// operator never resumes landing while blocking work is still publishing.
pub async fn handle(state: &Arc<ServerState>, paths: Vec<String>) -> Outcome {
    let state = Arc::clone(state);
    match tokio::spawn(async move {
        let _paused = state.maintenance.write().await;
        repair(&state, paths).await
    })
    .await
    {
        Ok(Ok(answer)) => Outcome::Ok(Response::MirrorRewritten(answer)),
        Ok(Err(error)) => Outcome::Error(ErrorBody::new(ErrorCode::Refused, error.to_string())),
        Err(error) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, error.to_string())),
    }
}

async fn repair(state: &Arc<ServerState>, paths: Vec<String>) -> Result<HistoryRewrite> {
    let paths = objects::paths(paths)?;
    let preparing = Arc::clone(state);
    let (target, branch, base) = tokio::task::spawn_blocking(move || {
        // After-answer work starts before a land returns and is counted until
        // it completes. No new request can start under the exclusive permit.
        preparing.landing.settle();
        let report = preparing.landing.recover(&preparing)
            .map_err(|e| anyhow::anyhow!(e.message))?;
        if !report.is_complete() {
            bail!("post-land recovery is incomplete; resolve its reported errors and retry before rewriting history");
        }
        super::read_target(&preparing.config).map_err(|e| anyhow::anyhow!("mirror state: {e:?}"))?
            .ok_or_else(|| anyhow::anyhow!("no mirror configured; configure and sync a mirror before repairing its history"))
    }).await??;
    // Relative local remotes have always been interpreted by git from the
    // mirror staging repository. Keep that meaning in the inspection repo.
    let remote_name = remote::name(&state.config, &target.remote)?;
    let remote = remote::Remote::inspect(&state.config, &remote_name).await?;
    if target
        .pushed
        .is_some_and(|id| !remote.commits.contains(&id))
    {
        bail!("last pushed commit is no longer on the mirror; restore the expected remote/refs and retry");
    }
    let preparing = Arc::clone(state);
    let boundary = target.pushed;
    let mut rewrite = tokio::task::spawn_blocking(move || {
        let store = LooseObjectStore::open(preparing.config.objects_dir())?;
        objects::Rewrite::prepare(&store, base, boundary, paths)
    })
    .await??;
    let published: Vec<_> = rewrite
        .commits
        .iter()
        .filter(|(old, _)| remote.commits.contains(old))
        .map(|(old, _)| old.to_hex())
        .collect();
    if !published.is_empty() {
        bail!("refusing to rewrite commits already on the mirror: {}. Use a normal deletion change instead; \
            removing published content requires a separately coordinated mirror history rewrite and force-push. \
            For a secret, rotate/revoke it immediately; this command cannot unpublish it", published.join(", "));
    }
    remote.unchanged(&remote_name).await?;
    publish(state, branch, base, boundary, &mut rewrite).await
}

async fn publish(
    state: &Arc<ServerState>,
    branch: String,
    base: rosen_core::ObjectId,
    boundary: Option<rosen_core::ObjectId>,
    rewrite: &mut objects::Rewrite,
) -> Result<HistoryRewrite> {
    let mut answer = HistoryRewrite {
        base_ref: branch,
        old_base: base,
        new_base: rewrite
            .commits
            .iter()
            .find(|(old, _)| *old == base)
            .map_or(base, |(_, new)| *new),
        mirror_boundary: boundary,
        rewritten_commits: rewrite.commits.len(),
        updated_changes: 0,
        paths: rewrite.paths().to_vec(),
    };
    // All object writes precede the one metadata commit. A crash before it
    // leaves only unreachable objects; a crash after it sees the whole move.
    let publishing = Arc::clone(state);
    let mut rewrite = std::mem::take(rewrite);
    answer = tokio::task::spawn_blocking(move || {
        metadata::apply(&publishing, &mut answer, &mut rewrite)?;
        publishing.events.deliver_staged();
        Ok::<_, anyhow::Error>(answer)
    })
    .await??;
    state.mirror_wake.notify_one();
    Ok(answer)
}
