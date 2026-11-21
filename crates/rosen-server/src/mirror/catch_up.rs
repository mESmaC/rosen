//! Catch the base up to a mirror tip another server landed (SPEC R1, #416).
//!
//! Two servers can serve one project, each mirroring to the same remote.
//! The one that sat idle finds its base behind the mirror, and its next
//! push would be refused as non-fast-forward. When the base is an ancestor
//! of the mirror tip, this stores the new commits under their git IDs and
//! moves the base to the tip by compare-and-swap. Everything else in the
//! metadata store stays, so clients keep their event positions, cursors,
//! machines and channel history: catching up is not a new database.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use rosen_core::{ChangeParent, ObjectId};
use rosen_protocol::{
    CatchUpRebaseError, CatchUpStatus, ChangeFilter, ErrorBody, ErrorCode, EventKind,
    MirrorCatchUp, Outcome, Response,
};
use rosen_store::{LooseObjectStore, ObjectStore};
use rusqlite::{params, TransactionBehavior};
use serde_json::json;

use super::adopt::fetch_branch;
use super::proactive_auth::ProactiveAuth;
use super::rewrite::remote::{name, run_bytes};
use crate::events::{self, Staged};
use crate::import::{copy_objects, filter_missing, GitSource};
use crate::ServerState;

/// Commits named on each side of a refused divergence; the rest are counted.
const LISTED: usize = 20;

pub(crate) async fn handle(state: &Arc<ServerState>) -> Outcome {
    let state = Arc::clone(state);
    // Detached, so an operator who disconnects cannot stop the catch-up
    // between moving the base and rebasing the open changes onto it.
    match tokio::spawn(async move {
        let mut answer = {
            let _paused = state.maintenance.write().await;
            ensure!(!state.is_shutting_down(), "the server is shutting down");
            fast_forward(&state).await?
        };
        if answer.status == CatchUpStatus::FastForwarded {
            // Rebases run as ordinary requests do, so the server serves
            // other requests while a long list of changes moves.
            let _running = state.maintenance.read().await;
            answer = rebase_open(&state, answer).await;
        }
        Ok(answer)
    })
    .await
    {
        Ok(Ok(answer)) => Outcome::Ok(Response::MirrorCaughtUp(answer)),
        Ok(Err(error)) => Outcome::Error(ErrorBody::new(ErrorCode::Refused, error.to_string())),
        Err(error) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, error.to_string())),
    }
}

/// Fetches the mirror branch and moves the base to its tip when that is a
/// fast-forward. Runs with landing and mirror pushes paused.
async fn fast_forward(state: &Arc<ServerState>) -> Result<MirrorCatchUp> {
    let reading = Arc::clone(state);
    let (target, branch, base) = tokio::task::spawn_blocking(move || {
        reading.landing.settle();
        super::read_target(&reading.config)
            .map_err(|error| anyhow::anyhow!("mirror state: {error:?}"))?
            .context("no mirror configured; name one with `rosen import --mirror` or `mirror.remote` in rosen.toml")
    })
    .await??;
    let remote = name(&state.config, &target.remote)?;
    let staging = tempfile::tempdir_in(std::path::absolute(&state.config.data_dir)?)?;
    let repo = staging.path();
    super::push::prepare_staging(repo, &state.config.objects_dir()).await?;
    let auth = &state.mirror_proactive_auth;
    let tip = fetch_branch(repo, &remote, &format!("refs/heads/{branch}"), auth).await?;
    let mut answer = MirrorCatchUp {
        base_ref: branch,
        status: CatchUpStatus::UpToDate,
        old_base: base,
        new_base: base,
        remote_tip: tip,
        commits: Vec::new(),
        rebased: Vec::new(),
        conflicted: Vec::new(),
        rebase_errors: Vec::new(),
    };
    let only_base = count_between(repo, &remote, tip, base, auth).await?;
    let only_remote = count_between(repo, &remote, base, tip, auth).await?;
    match (only_base, only_remote) {
        (_, 0) if tip == base => {}
        // The mirror worker pushes the rest of the base as usual.
        (_, 0) => answer.status = CatchUpStatus::Ahead,
        (0, _) => answer = move_base(state, repo, &remote, &target.remote, answer).await?,
        _ => bail!(diverged(repo, &remote, &answer, only_base, only_remote, auth).await?),
    }
    state.mirror_wake.notify_one();
    Ok(answer)
}

/// Stores the mirror's new commits and moves the base onto its tip.
async fn move_base(
    state: &Arc<ServerState>,
    repo: &Path,
    remote: &str,
    stored_remote: &str,
    mut answer: MirrorCatchUp,
) -> Result<MirrorCatchUp> {
    let (base, tip) = (answer.old_base, answer.remote_tip);
    answer.commits =
        first_parent_line(repo, remote, base, tip, &state.mirror_proactive_auth).await?;
    let (source, objects) = (repo.to_path_buf(), state.config.objects_dir());
    tokio::task::spawn_blocking(move || store_objects(&source, &objects, base, tip)).await??;
    answer.status = CatchUpStatus::FastForwarded;
    answer.new_base = tip;
    let publishing = Arc::clone(state);
    let stored_remote = stored_remote.to_owned();
    tokio::task::spawn_blocking(move || {
        publish(&publishing, &answer, &stored_remote)?;
        publishing.events.deliver_staged();
        Ok(answer)
    })
    .await?
}

/// How many commits `to` has that `from` lacks.
pub(super) async fn count_between(
    repo: &Path,
    remote: &str,
    from: ObjectId,
    to: ObjectId,
    auth: &ProactiveAuth,
) -> Result<u64> {
    let range = format!("{}..{}", from.to_hex(), to.to_hex());
    let output = run_bytes(repo, remote, &["rev-list", "--count", &range], auth).await?;
    Ok(std::str::from_utf8(&output)?.trim().parse()?)
}

/// The commits the base passes through on its way to `tip`, oldest first:
/// the positions the mirror branch held, which base history records.
async fn first_parent_line(
    repo: &Path,
    remote: &str,
    base: ObjectId,
    tip: ObjectId,
    auth: &ProactiveAuth,
) -> Result<Vec<ObjectId>> {
    let range = format!("{}..{}", base.to_hex(), tip.to_hex());
    let output = run_bytes(
        repo,
        remote,
        &["rev-list", "--first-parent", "--reverse", &range],
        auth,
    )
    .await?;
    let commits = std::str::from_utf8(&output)?
        .lines()
        .map(|line| Ok(line.trim().parse()?))
        .collect::<Result<Vec<ObjectId>>>()?;
    // The base ref's history ends at the commit it holds.
    ensure!(
        commits.last() == Some(&tip),
        "mirror history does not end at its tip {tip}; retry"
    );
    Ok(commits)
}

/// Copies every object the mirror tip reaches and the base does not into
/// the store, unchanged, so the commits keep their git IDs. Objects are
/// durable before the metadata moves the base onto them.
fn store_objects(repo: &Path, objects: &Path, base: ObjectId, tip: ObjectId) -> Result<()> {
    let store = LooseObjectStore::open(objects)?;
    let source = GitSource::open_local(repo)?;
    let ids = source.reachable_objects_excluding(&tip, Some(&base))?;
    let missing = filter_missing(&ids, &store)?;
    copy_objects(&source, &missing, &store)?;
    store
        .read_commit(&tip)
        .with_context(|| format!("mirror tip {tip} was not stored"))?;
    Ok(())
}

/// Moves the base and records the imported commits in one transaction, so
/// a crash leaves either the old base or the whole catch-up.
fn publish(state: &ServerState, answer: &MirrorCatchUp, stored_remote: &str) -> Result<()> {
    let mut conn = crate::metadata::open(&state.config.metadata_path())?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let updated = tx.execute(
        "UPDATE base_refs SET target=?3 WHERE name=?1 AND target=?2",
        params![
            answer.base_ref,
            answer.old_base.to_hex(),
            answer.new_base.to_hex()
        ],
    )?;
    ensure!(updated == 1, "Rosen base moved during catch-up; retry");
    // Imported like an adoption's entry: no change, no post-land work, and
    // left out of landed-change checks, landed reviews and bisection, since
    // another server landed and checked these commits.
    for commit in &answer.commits {
        tx.execute(
            "INSERT INTO base_history (ref_name,commit_id,tier,post_land_done) VALUES (?1,?2,'mirror',1)",
            params![answer.base_ref, commit.to_hex()],
        )?;
    }
    // The mirror already holds the tip; the worker has nothing to push
    // until this server lands.
    tx.execute(
        "UPDATE mirror SET pushed=?1 WHERE id=1 AND remote=?2",
        params![answer.new_base.to_hex(), stored_remote],
    )?;
    let detail = json!({
        "status": "caught_up",
        "base_ref": answer.base_ref,
        "old_base": answer.old_base,
        "new_base": answer.new_base,
        "commits": answer.commits.len(),
    });
    events::stage(
        &tx,
        &Staged {
            kind: EventKind::Mirror,
            change: None,
            detail,
            concern: Default::default(),
        },
    )?;
    tx.commit()?;
    Ok(())
}

/// The refusal for diverged histories: both tips and the commits on each
/// side, so a person or `rosen mirror adopt` can decide.
async fn diverged(
    repo: &Path,
    remote: &str,
    answer: &MirrorCatchUp,
    only_base: u64,
    only_remote: u64,
    auth: &ProactiveAuth,
) -> Result<String> {
    let (base, tip) = (answer.old_base, answer.remote_tip);
    let base_side = listing(repo, remote, tip, base, only_base, auth).await?;
    let remote_side = listing(repo, remote, base, tip, only_remote, auth).await?;
    Ok(format!(
        "base {} and mirror branch {} have diverged; nothing was moved.\n\
         Rosen base {base} has {only_base} commit(s) the mirror lacks:\n{base_side}\
         Mirror tip {tip} has {only_remote} commit(s) the base lacks:\n{remote_side}\
         Land the mirror's edits in Rosen and run `rosen mirror adopt {tip}`, or reconcile the histories by hand.",
        answer.base_ref, answer.base_ref
    ))
}

/// Up to [`LISTED`] commits `to` has that `from` lacks, newest first, one
/// `  <id> <subject>` line each.
pub(super) async fn listing(
    repo: &Path,
    remote: &str,
    from: ObjectId,
    to: ObjectId,
    total: u64,
    auth: &ProactiveAuth,
) -> Result<String> {
    let range = format!("{}..{}", from.to_hex(), to.to_hex());
    let limit = format!("--max-count={LISTED}");
    let output = run_bytes(
        repo,
        remote,
        &["log", "--no-decorate", "--format=  %H %s", &limit, &range],
        auth,
    )
    .await?;
    let mut text = String::from_utf8_lossy(&output).into_owned();
    if total > LISTED as u64 {
        text.push_str(&format!("  … and {} more\n", total - LISTED as u64));
    }
    Ok(text)
}

/// Rebases every open change built on an older base onto the new one, as
/// `rosen rebase` would, with its `rebased` and `conflict` events. A change
/// that cannot move is reported and rebases when it lands.
async fn rebase_open(state: &Arc<ServerState>, mut answer: MirrorCatchUp) -> MirrorCatchUp {
    let Ok(permit) = Arc::clone(&state.workspace_io).acquire_owned().await else {
        answer.rebase_errors.push(CatchUpRebaseError {
            change: String::new(),
            error: "the server is shutting down; open changes rebase when they land".into(),
        });
        return answer;
    };
    let state = Arc::clone(state);
    let fallback = answer.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        rebase_each(&state, &mut answer);
        answer
    })
    .await
    .unwrap_or_else(|join| {
        let mut answer = fallback;
        answer.rebase_errors.push(CatchUpRebaseError {
            change: String::new(),
            error: format!("rebasing open changes stopped: {join}"),
        });
        answer
    })
}

fn rebase_each(state: &ServerState, answer: &mut MirrorCatchUp) {
    let records = match state.changes.list(&ChangeFilter::default()) {
        Ok(records) => records,
        Err(error) => {
            answer.rebase_errors.push(CatchUpRebaseError {
                change: String::new(),
                error: format!("listing open changes: {error}"),
            });
            return;
        }
    };
    for record in records {
        let change = record.change;
        // A stacked change follows its parent: the stack restack moves it
        // after the parent's rebase.
        let ChangeParent::Base(on) = change.parent else {
            continue;
        };
        if change.state.is_closed() || on == answer.new_base {
            continue;
        }
        let id = change.id.to_string();
        match crate::rebase::rebase(state, &id) {
            Ok(moved) => {
                if !moved.change.conflicts.is_empty() {
                    answer.conflicted.push(id.clone());
                }
                answer.rebased.push(id);
            }
            Err(error) => answer.rebase_errors.push(CatchUpRebaseError {
                change: id,
                error: error.to_string(),
            }),
        }
    }
}
