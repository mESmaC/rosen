//! Typed batch verification, separate from R8's post-land housekeeping.
pub(crate) mod builds;
pub(crate) mod checkers;
pub mod deploy;
pub mod failures;
pub(crate) mod platforms;
pub mod status;
pub mod store;
pub mod submitter;

use crate::{landing::Ledger, ServerState};
use rosen_core::Platform;
use rosen_protocol::{
    CheckKind, CheckPriority, ErrorBody, ErrorCode, EventKind, Outcome, PostLandJob, PostLandWork,
    Response,
};
use rusqlite::Connection;
use std::sync::{Arc, Mutex, PoisonError};

/// A passing run as the server accepted it, with who reported it.
pub type Passed = (PostLandJob, submitter::Submitted);

pub async fn handle(state: &Arc<ServerState>, passed: Option<Passed>, kind: CheckKind) -> Outcome {
    handle_for(state, passed, kind, None).await
}

pub async fn handle_for(
    state: &Arc<ServerState>,
    passed: Option<Passed>,
    kind: CheckKind,
    checker_platform: Option<Platform>,
) -> Outcome {
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || run(&state, passed, kind, checker_platform)).await {
        Ok(Ok(work)) => Outcome::Ok(Response::PostLand(work)),
        Ok(Err(err)) => Outcome::Error(err),
        Err(err) => Outcome::Error(internal(err)),
    }
}

fn open(state: &ServerState) -> Result<Connection, ErrorBody> {
    std::fs::create_dir_all(&state.config.data_dir).map_err(internal)?;
    let conn = crate::metadata::open(&state.config.metadata_path()).map_err(internal)?;
    store::ensure_schema(&conn).map_err(internal)?;
    builds::ensure_schema(&conn).map_err(internal)?;
    platforms::ensure_schema(&conn).map_err(internal)?;
    failures::ensure_schema(&conn).map_err(internal)?;
    deploy::ensure_schema(&conn).map_err(internal)?;
    Ok(conn)
}

fn run(
    state: &ServerState,
    passed: Option<Passed>,
    kind: CheckKind,
    mut checker_platform: Option<Platform>,
) -> Result<PostLandWork, ErrorBody> {
    let mut conn = open(state)?;
    let ledger = Ledger::open(&state.config.metadata_path()).map_err(internal)?;
    let Some((name, _)) = ledger.project_base().map_err(internal)? else {
        return Ok(PostLandWork {
            job: None,
            last_green: None,
        });
    };
    if let Some((job, submitted)) = passed {
        validate(state, &ledger, &name, &job)?;
        if checker_platform.is_none() {
            checker_platform = job
                .checker_platform
                .as_deref()
                .or(job.platform.as_deref())
                .and_then(|raw| Platform::parse(raw).ok());
        }
        record_pass(state, &mut conn, &name, &job, &submitted)?;
    }
    deliver(state, &conn)?;
    state.events.deliver_staged();
    offer_next(state, &conn, &name, kind, checker_platform.as_ref())
}

fn record_pass(
    state: &ServerState,
    conn: &mut Connection,
    name: &str,
    job: &PostLandJob,
    submitted: &submitter::Submitted,
) -> Result<(), ErrorBody> {
    let accepted = match job.platform.as_deref() {
        Some(target) => {
            let required = required_platforms(state, job.kind, &job.commit)?;
            let objects = state.config.objects_dir();
            let ok = platforms::passed_with(
                conn,
                name,
                target,
                job.kind,
                &job.commit,
                &required,
                Some(submitted),
                |tx, prev, end, green_commit| {
                    let resolved = state
                        .project_config
                        .at_commit(green_commit)
                        .map_err(internal)?;
                    deploy::on_green_range(
                        tx,
                        &objects,
                        name,
                        prev,
                        end,
                        green_commit,
                        &resolved.config.deploy,
                    )
                },
            )?;
            if ok {
                state.checkers.bump_version();
            }
            ok
        }
        None => passed_job(state, conn, name, job, submitted)?,
    };
    if !accepted {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgument,
            "commit is not a land",
        ));
    }
    state.stats.bump();
    Ok(())
}

fn required_platforms(
    state: &ServerState,
    kind: CheckKind,
    commit: &rosen_core::ObjectId,
) -> Result<Vec<Platform>, ErrorBody> {
    let base = state
        .project_config
        .at_base()
        .map_err(internal)?
        .map(|(_, r)| check_set_for(&r.config, kind).platforms.clone())
        .unwrap_or_default();
    if !base.is_empty() {
        return Ok(base);
    }
    let at_commit = state.project_config.at_commit(commit).map_err(internal)?;
    Ok(check_set_for(&at_commit.config, kind).platforms.clone())
}

fn check_set_for(
    config: &rosen_core::ProjectConfig,
    kind: CheckKind,
) -> &rosen_core::project_config::CheckSet {
    match kind {
        CheckKind::Test => &config.checks.post_land,
        CheckKind::Build => &config.checks.pre_land,
    }
}

fn offer_next(
    state: &ServerState,
    conn: &Connection,
    name: &str,
    requested: CheckKind,
    checker_platform: Option<&Platform>,
) -> Result<PostLandWork, ErrorBody> {
    let base_config = state
        .project_config
        .at_base()
        .map_err(internal)?
        .map(|(_, r)| r.config);
    let kind = next_kind_for(
        base_config.as_deref(),
        conn,
        name,
        requested,
        checker_platform,
    )?;
    let job = next_pending_work(base_config.as_deref(), conn, name, kind, checker_platform)?
        .map(|(commit, target)| job_at(state, commit, kind, target.as_ref(), checker_platform))
        .transpose()?;
    Ok(PostLandWork {
        job,
        last_green: last_green(conn, name, kind)?,
    })
}

struct CandidateTarget {
    platform: Platform,
    required: bool,
}

enum OfferTargets {
    Any,
    Platforms(Vec<CandidateTarget>),
    Unsupported,
}

fn resolve_targets(
    config: Option<&rosen_core::ProjectConfig>,
    kind: CheckKind,
    checker_platform: Option<&Platform>,
) -> OfferTargets {
    let Some(config) = config else {
        return OfferTargets::Any;
    };
    let set = check_set_for(config, kind);
    if set.platforms.is_empty() {
        return OfferTargets::Any;
    }
    let Some(checker) = checker_platform else {
        return OfferTargets::Unsupported;
    };
    let targets: Vec<CandidateTarget> = set
        .matching_targets(checker)
        .into_iter()
        .map(|platform| CandidateTarget {
            required: set.is_required_platform(platform),
            platform: platform.clone(),
        })
        .collect();
    if targets.is_empty() {
        OfferTargets::Unsupported
    } else {
        OfferTargets::Platforms(targets)
    }
}

/// Selects the next pending `(commit, target)` a checker on `checker_platform`
/// can run. Across overlapping matching targets (for example `macos` and
/// `macos-arm64`), required targets always precede advisory ones, and within
/// each tier the target with the lowest covered sequence goes first so neither
/// checkpoint can starve under continuous lands (R12, #459).
fn next_pending_work(
    config: Option<&rosen_core::ProjectConfig>,
    conn: &Connection,
    name: &str,
    kind: CheckKind,
    checker_platform: Option<&Platform>,
) -> Result<Option<(rosen_core::ObjectId, Option<Platform>)>, ErrorBody> {
    match resolve_targets(config, kind, checker_platform) {
        OfferTargets::Unsupported => Ok(None),
        OfferTargets::Any => Ok(newest_unscoped(conn, name, kind)?.map(|commit| (commit, None))),
        OfferTargets::Platforms(targets) => {
            let mut best: Option<((bool, i64, usize), rosen_core::ObjectId, Platform)> = None;
            for (index, candidate) in targets.into_iter().enumerate() {
                let Some((commit, floor)) =
                    platforms::newest_with_floor(conn, name, candidate.platform.as_str(), kind)
                        .map_err(internal)?
                else {
                    continue;
                };
                let key = (!candidate.required, floor, index);
                if best.as_ref().is_none_or(|(best_key, _, _)| key < *best_key) {
                    best = Some((key, commit, candidate.platform));
                }
            }
            Ok(best.map(|(_, commit, target)| (commit, Some(target))))
        }
    }
}

fn job_at(
    state: &ServerState,
    commit: rosen_core::ObjectId,
    kind: CheckKind,
    target: Option<&Platform>,
    checker_platform: Option<&Platform>,
) -> Result<PostLandJob, ErrorBody> {
    let config = state.project_config.at_commit(&commit).map_err(internal)?;
    let mut check_set = check_set_for(&config.config, kind).clone();
    check_set.commands = check_set.commands_for(checker_platform.or(target));
    Ok(PostLandJob {
        commit,
        kind,
        priority: if kind == CheckKind::Build
            && config.config.recheck == rosen_core::RecheckMode::Merge
        {
            CheckPriority::Landing
        } else {
            CheckPriority::Verify
        },
        check_set,
        platform: target.map(|p| p.as_str().to_owned()),
        checker_platform: checker_platform.map(|p| p.as_str().to_owned()),
        submitter: None,
    })
}

/// Validates the immutable batch binding, shared with failure submission.
pub fn validate(
    state: &ServerState,
    ledger: &Ledger,
    name: &str,
    job: &PostLandJob,
) -> Result<(), ErrorBody> {
    if ledger
        .base_seq(name, &job.commit)
        .map_err(internal)?
        .is_none()
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgument,
            "commit was never on the base",
        ));
    }
    let config = state
        .project_config
        .at_commit(&job.commit)
        .map_err(internal)?;
    let configured = check_set_for(&config.config, job.kind);
    let target = validate_platform(configured, job.platform.as_deref())?;
    let checker = validate_checker_platform(target.as_ref(), job.checker_platform.as_deref())?;
    let expected = job_at(
        state,
        job.commit,
        job.kind,
        target.as_ref(),
        checker.as_ref(),
    )?;
    if expected.check_set != job.check_set {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgument,
            "wrong post-land check set",
        ));
    }
    Ok(())
}

fn validate_checker_platform(
    target: Option<&Platform>,
    checker_platform: Option<&str>,
) -> Result<Option<Platform>, ErrorBody> {
    let Some(raw) = checker_platform else {
        return Ok(None);
    };
    let parsed =
        Platform::parse(raw).map_err(|err| ErrorBody::new(ErrorCode::InvalidArgument, err))?;
    if let Some(target) = target {
        if !parsed.matches(target) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgument,
                format!(
                    "checker platform {raw:?} does not match target platform {:?}",
                    target.as_str()
                ),
            ));
        }
    }
    Ok(Some(parsed))
}

fn validate_platform(
    configured: &rosen_core::project_config::CheckSet,
    platform: Option<&str>,
) -> Result<Option<Platform>, ErrorBody> {
    if configured.platforms.is_empty() {
        if platform.is_some() {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgument,
                "check set does not configure per-platform jobs",
            ));
        }
        return Ok(None);
    }
    let Some(raw) = platform else {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgument,
            "job must name a configured platform",
        ));
    };
    let parsed =
        Platform::parse(raw).map_err(|err| ErrorBody::new(ErrorCode::InvalidArgument, err))?;
    if !configured.accepts_platform(&parsed) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgument,
            format!("platform {raw:?} is not configured for this check set"),
        ));
    }
    Ok(Some(parsed))
}

/// Crash replay may duplicate a notification, as R9 permits, but cannot lose it.
pub fn recover(state: &ServerState) -> Result<(), ErrorBody> {
    deliver(state, &open(state)?)
}

/// Serialises delivery. Each pass reads the pending rows and deletes a row
/// only after publishing it, so two concurrent passes (two checker requests,
/// or startup recovery racing one) would both publish every row (#149).
static DELIVERY: Mutex<()> = Mutex::new(());

/// Event detail published when a landed change is marked green (R9, R12).
pub(crate) fn green_detail(
    commit: &str,
    submitter: Option<&str>,
    at: Option<i64>,
) -> serde_json::Value {
    serde_json::json!({ "commit": commit, "submitter": submitter, "at": at })
}

fn deliver(state: &ServerState, conn: &Connection) -> Result<(), ErrorBody> {
    // A pass that panicked left no partial state the next pass cannot redo.
    let _turn = DELIVERY.lock().unwrap_or_else(PoisonError::into_inner);
    for row in store::pending(conn).map_err(internal)? {
        let published = state.events.publish(
            EventKind::Green,
            Some(row.change),
            green_detail(&row.commit, row.submitter.as_deref(), row.at),
        );
        if published == 0 {
            break;
        }
        conn.execute("DELETE FROM post_land_delivery WHERE seq=?1", [row.seq])
            .map_err(internal)?;
    }
    Ok(())
}

fn internal(err: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, err.to_string())
}

fn passed_job(
    state: &ServerState,
    conn: &mut Connection,
    name: &str,
    job: &PostLandJob,
    submitted: &submitter::Submitted,
) -> Result<bool, ErrorBody> {
    match job.kind {
        CheckKind::Test => {
            let resolved = state
                .project_config
                .at_commit(&job.commit)
                .map_err(internal)?;
            let objects = state.config.objects_dir();
            store::passed_by_with(conn, name, &job.commit, Some(submitted), |tx, prev, end| {
                deploy::on_green_range(
                    tx,
                    &objects,
                    name,
                    prev,
                    end,
                    &job.commit,
                    &resolved.config.deploy,
                )
            })
        }
        CheckKind::Build => {
            builds::passed(conn, name, &job.commit, Some(submitted)).map_err(internal)
        }
    }
}

pub fn last_green(
    conn: &Connection,
    name: &str,
    kind: CheckKind,
) -> Result<Option<rosen_core::ObjectId>, ErrorBody> {
    last_green_for(conn, name, kind, None)
}

pub fn last_green_for(
    conn: &Connection,
    name: &str,
    kind: CheckKind,
    platform: Option<&str>,
) -> Result<Option<rosen_core::ObjectId>, ErrorBody> {
    if let Some(platform) = platform {
        platforms::ensure_schema(conn).map_err(internal)?;
        return platforms::last_green(conn, name, platform, kind).map_err(internal);
    }
    match kind {
        CheckKind::Test => store::last_green(conn, name),
        CheckKind::Build => {
            builds::ensure_schema(conn).map_err(internal)?;
            builds::last_green(conn, name)
        }
    }
    .map_err(internal)
}

fn newest_unscoped(
    conn: &Connection,
    name: &str,
    kind: CheckKind,
) -> Result<Option<rosen_core::ObjectId>, ErrorBody> {
    match kind {
        CheckKind::Test => store::newest(conn, name),
        CheckKind::Build => builds::newest(conn, name),
    }
    .map_err(internal)
}

/// Merge-mode projects offer urgent build work before the full-test queue.
fn next_kind_for(
    config: Option<&rosen_core::ProjectConfig>,
    conn: &Connection,
    name: &str,
    requested: CheckKind,
    checker_platform: Option<&Platform>,
) -> Result<CheckKind, ErrorBody> {
    let merge = config.is_some_and(|c| c.recheck == rosen_core::RecheckMode::Merge);
    if requested != CheckKind::Test || !merge {
        return Ok(requested);
    }
    let has_build =
        next_pending_work(config, conn, name, CheckKind::Build, checker_platform)?.is_some();
    Ok(if has_build {
        CheckKind::Build
    } else {
        requested
    })
}

/// Keep new work off unbuilt tips. Until the first build passes, use the
/// imported starting point; it is not advertised as a verified green prefix.
pub(crate) fn workspace_base(
    config: &crate::ServerConfig,
    name: &str,
    resolved: &crate::project_config::Resolved,
) -> Result<rosen_core::ObjectId, String> {
    if resolved.config.recheck != rosen_core::RecheckMode::Merge {
        return Ok(resolved.commit);
    }
    let conn = crate::metadata::open(&config.metadata_path()).map_err(|e| e.to_string())?;
    builds::ensure_schema(&conn).map_err(|e| e.to_string())?;
    if let Some(built) = builds::last_green(&conn, name).map_err(|e| e.to_string())? {
        return Ok(built);
    }
    let initial: String = conn
        .query_row(
            "SELECT commit_id FROM base_history WHERE ref_name=?1 ORDER BY seq LIMIT 1",
            [name],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    initial
        .parse()
        .map_err(|e: rosen_core::Error| e.to_string())
}
