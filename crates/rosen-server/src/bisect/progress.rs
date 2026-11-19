//! What a bisection has done so far, for the read-only status and the
//! `bisect` event (R12, R9). Nothing here decides a step.
use super::model::Session;
use crate::events::{Concern, Staged};
use crate::post_land::submitter::{self, Submitted};
use rosen_core::ObjectId;
use rosen_protocol::{
    BisectJob, BisectSession, BisectState, BisectStep, ErrorBody, EventKind, StepState,
};
use rusqlite::{params, Connection};

pub(super) fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS bisection_steps (
        session INTEGER NOT NULL, step INTEGER NOT NULL, commit_id TEXT NOT NULL,
        passed INTEGER NOT NULL, submitter TEXT, submitted_at INTEGER,
        PRIMARY KEY(session, step));",
    )?;
    submitter::add_column(conn, "bisections", "opened_by", "TEXT")?;
    submitter::add_column(conn, "bisections", "opened_at", "INTEGER")
}

/// The halving steps for `interval` candidate commits, plus the second
/// failure of the culprit and the fresh run of its parent.
pub fn planned_steps(interval: usize) -> u64 {
    if interval == 0 {
        return 0;
    }
    let halving = usize::BITS - (interval - 1).leading_zeros();
    u64::from(halving) + 2
}

/// Records who opened a new session and stages its `opened` event, in the
/// transaction that inserts it.
pub(super) fn opened(
    conn: &Connection,
    session: &Session,
    failed: ObjectId,
    by: Option<&Submitted>,
) -> Result<(), ErrorBody> {
    conn.execute(
        "UPDATE bisections SET opened_by=?2,opened_at=?3 WHERE id=?1",
        params![session.id, by.map(|s| &s.by), by.map(|s| s.at)],
    )
    .map_err(super::internal)?;
    let (who, at) = submitter::detail(by);
    let detail = serde_json::json!({
        "session": session.id, "kind": session.kind, "progress": "opened",
        "step": null, "commit": failed, "result": "failed",
        "submitter": who, "at": at,
        "lower": session.commits[0].0, "interval": session.commits.len() - 1,
        "planned_steps": planned_steps(session.commits.len() - 1),
    });
    stage(conn, detail)
}

/// Records one accepted result and stages its `bisect` event, in the
/// transaction that advances the session. `after` is the session's state
/// once the result is applied.
pub(super) fn accepted(
    conn: &Connection,
    job: &BisectJob,
    passed: bool,
    by: Option<&Submitted>,
    after: &BisectState,
    planned: u64,
) -> Result<(), ErrorBody> {
    conn.execute(
        "INSERT INTO bisection_steps(session,step,commit_id,passed,submitter,submitted_at)
         VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            job.session,
            job.step,
            job.commit.to_hex(),
            passed,
            by.map(|s| &s.by),
            by.map(|s| s.at)
        ],
    )
    .map_err(super::internal)?;
    let progress = match (&after.culprit, after.flaky) {
        (Some(_), _) => "culprit",
        (None, true) => "flaky",
        (None, false) => "step",
    };
    let (who, at) = submitter::detail(by);
    let detail = serde_json::json!({
        "session": job.session, "kind": job.kind, "progress": progress,
        "step": job.step, "commit": job.commit,
        "result": if passed { "passed" } else { "failed" },
        "submitter": who, "at": at, "planned_steps": planned,
        "culprit": after.culprit.as_ref().map(|c| c.commit),
    });
    stage(conn, detail)
}

/// Bisection progress is for the project view: it concerns no change's
/// owner until a culprit is confirmed, and that has its own event.
fn stage(conn: &Connection, detail: serde_json::Value) -> Result<(), ErrorBody> {
    let event = Staged {
        kind: EventKind::Bisect,
        change: None,
        detail,
        concern: Concern::default(),
    };
    crate::events::stage(conn, &event).map_err(super::internal)
}

/// Accepted results, oldest first, then the current job as `offered` and
/// the rest of the plan as `waiting` with no commit chosen yet.
pub(super) fn steps(conn: &Connection, session: &Session) -> Result<Vec<BisectStep>, ErrorBody> {
    let mut query = conn
        .prepare(
            "SELECT step,commit_id,passed,submitter,submitted_at FROM bisection_steps
             WHERE session=?1 ORDER BY step",
        )
        .map_err(super::internal)?;
    let rows = query
        .query_map([session.id], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(super::internal)?;
    let mut steps = Vec::new();
    for row in rows {
        let (step, commit, passed, submitter, at) = row.map_err(super::internal)?;
        steps.push(BisectStep {
            step,
            commit: Some(commit.parse().map_err(super::internal)?),
            state: if passed {
                StepState::Passed
            } else {
                StepState::Failed
            },
            submitter,
            at,
        });
    }
    if let Some(job) = session.state()?.job {
        steps.push(unsubmitted(job.step, Some(job.commit), StepState::Offered));
        // Sessions from before steps were recorded have fewer rows than the
        // step token; the rest of the plan still counts from the token.
        for step in (job.step + 1)..(job.step + remaining(session)) {
            steps.push(unsubmitted(step, None, StepState::Waiting));
        }
    }
    Ok(steps)
}

/// Steps still to run, the current one included. The search can end a step
/// early when an uneven split leaves the smaller half.
fn remaining(session: &Session) -> u64 {
    match session.phase.as_str() {
        "search" => planned_steps(session.high - session.low),
        "confirm" => 2,
        "parent" => 1,
        _ => 0,
    }
}

fn unsubmitted(step: u64, commit: Option<ObjectId>, state: StepState) -> BisectStep {
    BisectStep {
        step,
        commit,
        state,
        submitter: None,
        at: None,
    }
}

/// Unresolved sessions on `name`: searching or confirming, or stopped as
/// flaky while its failing batch is still above its kind's checkpoint.
/// Reads only, so it runs on a read-only connection.
pub fn sessions(conn: &Connection, name: &str) -> Result<Vec<BisectSession>, ErrorBody> {
    let mut query = conn
        .prepare(
            "SELECT b.id,b.failed,b.opened_by,b.opened_at FROM bisections b
             WHERE b.ref_name=?1 AND (b.phase IN ('search','confirm','parent')
             OR (b.phase='flaky' AND (SELECT seq FROM base_history
                 WHERE ref_name=?1 AND commit_id=b.failed) > COALESCE(
                 (SELECT seq FROM post_land_green WHERE ref_name=?1 AND b.kind='test'),
                 (SELECT seq FROM post_land_build_green WHERE ref_name=?1 AND b.kind='build'),
                 0)))
             ORDER BY b.id",
        )
        .map_err(super::internal)?;
    let rows = query
        .query_map([name], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })
        .map_err(super::internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(super::internal)?;
    let mut sessions = Vec::new();
    for (id, failed, opened_by, opened_at) in rows {
        let session = super::storage::load(conn, id)?;
        let interval = session.commits.len() - 1;
        sessions.push(BisectSession {
            id,
            kind: session.kind,
            lower: session.commits[0].0,
            failing: failed.parse().map_err(super::internal)?,
            interval: interval as u64,
            planned_steps: planned_steps(interval),
            flaky: session.phase == "flaky",
            opened_by,
            opened_at,
            steps: steps(conn, &session)?,
        });
    }
    Ok(sessions)
}
