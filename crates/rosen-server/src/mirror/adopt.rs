//! Attach a directly pushed Git history after its content has landed in Rosen.
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use rosen_core::{Commit, ObjectId, Signature};
use rosen_protocol::{ErrorBody, ErrorCode, EventKind, MirrorAdoption, Outcome, Response};
use rosen_store::{LooseObjectStore, ObjectStore};
use rusqlite::{params, TransactionBehavior};
use serde_json::json;

use super::proactive_auth::ProactiveAuth;
use super::rewrite::remote::{name, run_bytes};
use crate::events::{self, Staged};
use crate::import::{copy_objects, filter_missing, GitSource};
use crate::ServerState;

pub(crate) async fn handle(state: &Arc<ServerState>, commit: ObjectId) -> Outcome {
    let state = Arc::clone(state);
    match tokio::spawn(async move {
        let _paused = state.maintenance.write().await;
        ensure!(!state.is_shutting_down(), "the server is shutting down");
        adopt(&state, commit).await
    })
    .await
    {
        Ok(Ok(answer)) => Outcome::Ok(Response::MirrorAdopted(answer)),
        Ok(Err(error)) => Outcome::Error(ErrorBody::new(ErrorCode::Refused, error.to_string())),
        Err(error) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, error.to_string())),
    }
}

async fn adopt(state: &Arc<ServerState>, remote_tip: ObjectId) -> Result<MirrorAdoption> {
    let reading = Arc::clone(state);
    let (target, branch, base) = tokio::task::spawn_blocking(move || {
        reading.landing.settle();
        super::read_target(&reading.config)
            .map_err(|error| anyhow::anyhow!("mirror state: {error:?}"))?
            .context("no mirror configured")
    })
    .await??;
    let remote = name(&state.config, &target.remote)?;
    let ref_name = format!("refs/heads/{branch}");
    let staging = tempfile::tempdir_in(std::path::absolute(&state.config.data_dir)?)?;
    super::push::prepare_staging(staging.path(), &state.config.objects_dir()).await?;
    let auth = &state.mirror_proactive_auth;
    fetch_tip(staging.path(), &remote, &ref_name, remote_tip, auth).await?;

    let ancestor = common_ancestor(staging.path(), &remote, base, remote_tip, auth).await?;
    let mut answer = MirrorAdoption {
        base_ref: branch,
        old_base: base,
        new_base: base,
        remote_tip,
    };
    if ancestor != remote_tip {
        verify_ported(staging.path(), &remote, ancestor, base, remote_tip, auth).await?;
        let objects = state.config.objects_dir();
        let repository = staging.path().to_path_buf();
        answer.new_base = tokio::task::spawn_blocking(move || {
            store_merge(&repository, &objects, ancestor, base, remote_tip)
        })
        .await??;
        verify_remote_tip(staging.path(), &remote, &ref_name, remote_tip, auth).await?;
        let publishing = Arc::clone(state);
        answer = tokio::task::spawn_blocking(move || {
            publish(&publishing, &answer)?;
            publishing.events.deliver_staged();
            Ok::<_, anyhow::Error>(answer)
        })
        .await??;
    }
    state.mirror_wake.notify_one();
    Ok(answer)
}

async fn fetch_tip(
    staging: &Path,
    remote: &str,
    branch: &str,
    expected: ObjectId,
    auth: &ProactiveAuth,
) -> Result<()> {
    let actual = fetch_branch(staging, remote, branch, auth).await?;
    ensure!(
        actual == expected,
        "mirror branch {branch} is at {actual}, expected {expected}; inspect the current tip and retry"
    );
    Ok(())
}

/// Fetches the mirror's `branch` (a full ref name) into the staging
/// repository and returns its tip.
pub(super) async fn fetch_branch(
    staging: &Path,
    remote: &str,
    branch: &str,
    auth: &ProactiveAuth,
) -> Result<ObjectId> {
    const FETCHED: &str = "refs/rosen-mirror-tip";
    let refspec = format!("{branch}:{FETCHED}");
    run_bytes(
        staging,
        remote,
        &[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-auto-maintenance",
            "--no-recurse-submodules",
            "--",
            remote,
            &refspec,
        ],
        auth,
    )
    .await?;
    let output = run_bytes(staging, remote, &["rev-parse", "--verify", FETCHED], auth).await?;
    Ok(std::str::from_utf8(&output)?.trim().parse()?)
}

async fn common_ancestor(
    staging: &Path,
    remote: &str,
    base: ObjectId,
    tip: ObjectId,
    auth: &ProactiveAuth,
) -> Result<ObjectId> {
    let output = run_bytes(
        staging,
        remote,
        &["merge-base", "--all", &base.to_hex(), &tip.to_hex()],
        auth,
    )
    .await
    .context("mirror and Rosen histories must share a common ancestor")?;
    std::str::from_utf8(&output)?
        .trim()
        .parse()
        .context("mirror and Rosen histories must have a single best common ancestor")
}

async fn verify_ported(
    staging: &Path,
    remote: &str,
    ancestor: ObjectId,
    base: ObjectId,
    tip: ObjectId,
    auth: &ProactiveAuth,
) -> Result<()> {
    // Compare raw paths, including non-UTF-8 names. A path that differs in
    // both comparisons still needs to be reconciled through a normal land.
    let changed = diff_paths(staging, remote, ancestor, tip, auth).await?;
    let diverged = diff_paths(staging, remote, base, tip, auth).await?;
    let diverged: HashSet<_> = diverged.split(|byte| *byte == 0).collect();
    if let Some(path) = changed
        .split(|byte| *byte == 0)
        .find(|path| !path.is_empty() && diverged.contains(path))
    {
        bail!(
            "mirror change to {:?} is not present in the Rosen base; land the remote edits in Rosen and retry",
            String::from_utf8_lossy(path)
        );
    }
    Ok(())
}

async fn diff_paths(
    staging: &Path,
    remote: &str,
    from: ObjectId,
    to: ObjectId,
    auth: &ProactiveAuth,
) -> Result<Vec<u8>> {
    run_bytes(
        staging,
        remote,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-commit-id",
            "--name-only",
            "--no-renames",
            "--ignore-submodules=none",
            &from.to_hex(),
            &to.to_hex(),
        ],
        auth,
    )
    .await
}

fn store_merge(
    staging: &Path,
    objects: &Path,
    ancestor: ObjectId,
    base: ObjectId,
    remote_tip: ObjectId,
) -> Result<ObjectId> {
    let store = LooseObjectStore::open(objects)?;
    let source = GitSource::open_local(staging)?;
    let ids = source.reachable_objects_excluding(&remote_tip, Some(&ancestor))?;
    let missing = filter_missing(&ids, &store)?;
    copy_objects(&source, &missing, &store)?;

    let tree = store.read_commit(&base)?.tree;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let who = Signature::new("rosen-server", "server@rosen", now as i64);
    Ok(store.put_commit(&Commit {
        tree,
        parents: vec![base, remote_tip],
        author: who.clone(),
        committer: who,
        extra_headers: Vec::new(),
        message: format!("Adopt Git mirror commit {remote_tip}\n").into_bytes(),
    })?)
}

async fn verify_remote_tip(
    staging: &Path,
    remote: &str,
    branch: &str,
    expected: ObjectId,
    auth: &ProactiveAuth,
) -> Result<()> {
    let output = run_bytes(
        staging,
        remote,
        &["ls-remote", "--refs", "--exit-code", "--", remote, branch],
        auth,
    )
    .await?;
    ensure!(
        std::str::from_utf8(&output)?.trim_end() == format!("{expected}\t{branch}"),
        "mirror branch {branch} moved during adoption; inspect the current tip and retry"
    );
    Ok(())
}

fn publish(state: &ServerState, answer: &MirrorAdoption) -> Result<()> {
    let mut conn = crate::metadata::open(&state.config.metadata_path())?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let old_base = answer.old_base.to_hex();
    let new_base = answer.new_base.to_hex();
    let updated = tx.execute(
        "UPDATE base_refs SET target=?3 WHERE name=?1 AND target=?2",
        params![answer.base_ref, old_base, new_base],
    )?;
    ensure!(updated == 1, "Rosen base moved during adoption; retry");
    // Keep a position for the new base without scheduling a content check or
    // recording an operator action as one of the branches' landed changes.
    tx.execute(
        "INSERT INTO base_history (ref_name,commit_id,tier,post_land_done) VALUES (?1,?2,'mirror',1)",
        params![answer.base_ref, new_base],
    )?;
    events::stage(
        &tx,
        &Staged {
            kind: EventKind::Mirror,
            change: None,
            detail: json!({"status": "adopted", "adoption": answer}),
            concern: Default::default(),
        },
    )?;
    tx.commit()?;
    Ok(())
}
