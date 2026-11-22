//! Durable SQLite storage for run-queue, post-land checkpoint and land phase
//! timings (counts and durations only, #449).

use rosen_core::{ChangeId, ObjectId};
use rosen_protocol::{CheckKind, LandPhaseTimings, RecordPostLandTiming, RecordRunTiming};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS run_timings (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    label       TEXT    NOT NULL,
    class       TEXT    NOT NULL DEFAULT 'build',
    change_id   TEXT,
    queued_at   INTEGER NOT NULL,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER NOT NULL,
    exit_status INTEGER,
    reused      INTEGER NOT NULL CHECK (reused IN (0, 1)),
    machine     TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS run_timings_by_time ON run_timings (queued_at);
CREATE INDEX IF NOT EXISTS run_timings_by_change ON run_timings (change_id);
CREATE INDEX IF NOT EXISTS run_timings_by_label ON run_timings (label, queued_at);

CREATE TABLE IF NOT EXISTS post_land_checkpoints (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    ref_name       TEXT    NOT NULL,
    kind           TEXT    NOT NULL CHECK (kind IN ('build', 'test')),
    platform       TEXT    NOT NULL DEFAULT '',
    commit_id      TEXT    NOT NULL,
    batch_size     INTEGER NOT NULL,
    build_ms       INTEGER,
    lint_ms        INTEGER,
    test_ms        INTEGER,
    bisect_session INTEGER UNIQUE,
    bisect_steps   INTEGER NOT NULL DEFAULT 0,
    outcome        TEXT    NOT NULL CHECK (outcome IN ('passed', 'bisecting', 'culprit', 'flaky', 'known')),
    started_at     INTEGER NOT NULL,
    finished_at    INTEGER
);
CREATE INDEX IF NOT EXISTS post_land_checkpoints_by_time ON post_land_checkpoints (started_at);
CREATE INDEX IF NOT EXISTS post_land_checkpoints_by_commit
    ON post_land_checkpoints (commit_id, kind, platform);

CREATE TABLE IF NOT EXISTS land_timings (
    id                       INTEGER PRIMARY KEY AUTOINCREMENT,
    change_id                TEXT    NOT NULL,
    commit_id                TEXT,
    outcome                  TEXT    NOT NULL,
    attempts                 INTEGER NOT NULL,
    registry_ms              INTEGER NOT NULL,
    engine_ms                INTEGER NOT NULL,
    attempts_ms              INTEGER NOT NULL,
    facts_ms                 INTEGER NOT NULL,
    tiers_ms                 INTEGER NOT NULL,
    merge_ms                 INTEGER NOT NULL,
    sizes_ms                 INTEGER NOT NULL,
    receipt_ms               INTEGER NOT NULL,
    write_ms                 INTEGER NOT NULL,
    cas_ms                   INTEGER NOT NULL,
    write_and_cas_ms         INTEGER NOT NULL,
    restack_ms               INTEGER NOT NULL,
    decision_and_restack_ms  INTEGER NOT NULL,
    answer_ms                INTEGER NOT NULL,
    total_ms                 INTEGER NOT NULL,
    recorded_at              INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS land_timings_by_change ON land_timings (change_id, recorded_at);
CREATE INDEX IF NOT EXISTS land_timings_by_time ON land_timings (recorded_at);

CREATE TABLE IF NOT EXISTS land_timing_layers (
    timing_id INTEGER NOT NULL REFERENCES land_timings(id),
    change_id TEXT    NOT NULL,
    commit_id TEXT    NOT NULL,
    PRIMARY KEY (timing_id, change_id)
);
CREATE INDEX IF NOT EXISTS land_timing_layers_by_change
    ON land_timing_layers (change_id, timing_id);
";

/// Creates the timing tables if they are missing. Safe to call on every open.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)
}

/// Persists one run-queue timing row.
pub fn record_run(
    conn: &Connection,
    record: &RecordRunTiming,
    change_id: Option<&str>,
    machine: &str,
) -> rusqlite::Result<()> {
    ensure_schema(conn)?;
    let label = sanitize_label(&record.label);
    let class = sanitize_label(&record.class);
    conn.execute(
        "INSERT INTO run_timings
         (label, class, change_id, queued_at, started_at, finished_at, exit_status, reused, machine)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            label,
            if class.is_empty() { "build" } else { &class },
            change_id,
            record.queued_at_ms,
            record.started_at_ms.max(record.queued_at_ms),
            record.finished_at_ms.max(record.started_at_ms),
            record.exit_status,
            i64::from(record.reused),
            machine,
        ],
    )?;
    Ok(())
}

/// Persists phase totals for one land request.
pub fn record_land(
    conn: &Connection,
    change_id: &str,
    commit_id: Option<&ObjectId>,
    outcome: &str,
    phases: &LandPhaseTimings,
    recorded_at: i64,
) -> rusqlite::Result<()> {
    ensure_schema(conn)?;
    insert_land_row(conn, change_id, commit_id, outcome, phases, recorded_at)?;
    Ok(())
}

/// Persists phase totals for one land request and links any ancestor layers
/// landed atomically in the same request to that single timing row (#462).
/// Window latency queries read `land_timings` directly, so an atomic stack
/// contributes one latency sample while every landed layer reports its phase
/// totals in `rosen stats`.
pub fn record_land_layers(
    conn: &mut Connection,
    change_id: &str,
    commit_id: Option<&ObjectId>,
    ancestors: &[(ChangeId, ObjectId)],
    outcome: &str,
    phases: &LandPhaseTimings,
    recorded_at: i64,
) -> rusqlite::Result<()> {
    ensure_schema(conn)?;
    if ancestors.is_empty() {
        insert_land_row(conn, change_id, commit_id, outcome, phases, recorded_at)?;
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let timing_id = insert_land_row(&tx, change_id, commit_id, outcome, phases, recorded_at)?;
    for (layer_change, layer_commit) in ancestors {
        let layer_id = layer_change.to_string();
        if layer_id == change_id {
            continue;
        }
        tx.execute(
            "INSERT OR IGNORE INTO land_timing_layers (timing_id, change_id, commit_id)
             VALUES (?1, ?2, ?3)",
            params![timing_id, layer_id, layer_commit.to_hex()],
        )?;
    }
    tx.commit()
}

fn insert_land_row(
    conn: &Connection,
    change_id: &str,
    commit_id: Option<&ObjectId>,
    outcome: &str,
    phases: &LandPhaseTimings,
    recorded_at: i64,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO land_timings (
            change_id, commit_id, outcome, attempts,
            registry_ms, engine_ms, attempts_ms, facts_ms, tiers_ms,
            merge_ms, sizes_ms, receipt_ms, write_ms, cas_ms,
            write_and_cas_ms, restack_ms, decision_and_restack_ms,
            answer_ms, total_ms, recorded_at
         ) VALUES (
            ?1, ?2, ?3, ?4,
            ?5, ?6, ?7, ?8, ?9,
            ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17,
            ?18, ?19, ?20
         )",
        params![
            change_id,
            commit_id.map(ObjectId::to_hex),
            outcome,
            i64::from(phases.attempts),
            to_i64(phases.registry_ms),
            to_i64(phases.engine_ms),
            to_i64(phases.attempts_ms),
            to_i64(phases.facts_ms),
            to_i64(phases.tiers_ms),
            to_i64(phases.merge_ms),
            to_i64(phases.sizes_ms),
            to_i64(phases.receipt_ms),
            to_i64(phases.write_ms),
            to_i64(phases.cas_ms),
            to_i64(phases.write_and_cas_ms),
            to_i64(phases.restack_ms),
            to_i64(phases.decision_and_restack_ms),
            to_i64(phases.answer_ms),
            to_i64(phases.total_ms),
            recorded_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Records a passing post-land checkpoint advancement inside the caller's transaction.
pub fn record_checkpoint_passed(
    conn: &Connection,
    ref_name: &str,
    kind: CheckKind,
    platform: Option<&str>,
    commit: &ObjectId,
    batch_size: u64,
    at_ms: i64,
) -> rusqlite::Result<()> {
    ensure_schema(conn)?;
    let platform = platform.unwrap_or("");
    conn.execute(
        "INSERT INTO post_land_checkpoints
         (ref_name, kind, platform, commit_id, batch_size, bisect_steps, outcome, started_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, 'passed', ?6, ?6)",
        params![
            ref_name,
            kind.as_str(),
            platform,
            commit.to_hex(),
            to_i64(batch_size),
            at_ms,
        ],
    )?;
    Ok(())
}

/// Records a failing post-land checkpoint that opened a bisection session.
#[allow(clippy::too_many_arguments)]
pub fn record_checkpoint_bisect_opened(
    conn: &Connection,
    ref_name: &str,
    kind: CheckKind,
    platform: Option<&str>,
    commit: &ObjectId,
    batch_size: u64,
    session_id: i64,
    outcome: &str,
    at_ms: i64,
) -> rusqlite::Result<()> {
    ensure_schema(conn)?;
    let platform = platform.unwrap_or("");
    let finished = (outcome != "bisecting").then_some(at_ms);
    conn.execute(
        "INSERT OR IGNORE INTO post_land_checkpoints
         (ref_name, kind, platform, commit_id, batch_size, bisect_session, bisect_steps,
          outcome, started_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9)",
        params![
            ref_name,
            kind.as_str(),
            platform,
            commit.to_hex(),
            to_i64(batch_size),
            session_id,
            outcome,
            at_ms,
            finished,
        ],
    )?;
    Ok(())
}

/// Updates a bisection checkpoint's step count and outcome when a step is accepted.
pub fn record_checkpoint_bisect_step(
    conn: &Connection,
    session_id: i64,
    steps: u64,
    outcome: &str,
    at_ms: i64,
) -> rusqlite::Result<()> {
    ensure_schema(conn)?;
    let finished = (outcome != "bisecting").then_some(at_ms);
    conn.execute(
        "UPDATE post_land_checkpoints
         SET bisect_steps = ?2, outcome = ?3, finished_at = COALESCE(?4, finished_at)
         WHERE bisect_session = ?1",
        params![session_id, to_i64(steps), outcome, finished],
    )?;
    Ok(())
}

/// Attaches or accumulates check durations on the matching post-land checkpoint.
pub fn record_post_land_durations(
    conn: &Connection,
    record: &RecordPostLandTiming,
) -> rusqlite::Result<()> {
    ensure_schema(conn)?;
    let target_id: Option<i64> = if let Some(session) = record.bisect_session {
        conn.query_row(
            "SELECT id FROM post_land_checkpoints WHERE bisect_session = ?1",
            [session],
            |r| r.get(0),
        )
        .optional()?
    } else {
        let platform = record.platform.as_deref().unwrap_or("");
        conn.query_row(
            "SELECT id FROM post_land_checkpoints
             WHERE commit_id = ?1 AND kind = ?2 AND platform = ?3
             ORDER BY id DESC LIMIT 1",
            params![record.commit.to_hex(), record.kind.as_str(), platform],
            |r| r.get(0),
        )
        .optional()?
    };
    let Some(id) = target_id else {
        return Ok(());
    };
    conn.execute(
        "UPDATE post_land_checkpoints SET
             build_ms = CASE WHEN ?2 IS NULL THEN build_ms ELSE COALESCE(build_ms, 0) + ?2 END,
             lint_ms  = CASE WHEN ?3 IS NULL THEN lint_ms  ELSE COALESCE(lint_ms, 0)  + ?3 END,
             test_ms  = CASE WHEN ?4 IS NULL THEN test_ms  ELSE COALESCE(test_ms, 0)  + ?4 END
         WHERE id = ?1",
        params![
            id,
            record.build_ms.map(to_i64),
            record.lint_ms.map(to_i64),
            record.test_ms.map(to_i64),
        ],
    )?;
    Ok(())
}

/// Counts landed commits in `(prev_seq, end_seq]` for a checkpoint's batch size.
pub fn batch_size_between(
    conn: &Connection,
    ref_name: &str,
    kind: CheckKind,
    prev_seq: i64,
    end_seq: i64,
) -> rusqlite::Result<u64> {
    let sql = match kind {
        CheckKind::Test => {
            "SELECT COUNT(*) FROM base_history
             WHERE ref_name = ?1 AND seq > ?2 AND seq <= ?3 AND change_id IS NOT NULL"
        }
        CheckKind::Build => {
            "SELECT COUNT(*) FROM base_history
             WHERE ref_name = ?1 AND seq > ?2 AND seq <= ?3 AND tier NOT IN ('initial', 'mirror')"
        }
    };
    let count: i64 = conn.query_row(sql, params![ref_name, prev_seq, end_seq], |r| r.get(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn sanitize_label(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(64)
        .collect()
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
