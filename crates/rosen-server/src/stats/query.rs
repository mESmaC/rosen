//! Aggregation queries for `rosen stats` and the Collapse state feed (#449).

use std::collections::{BTreeMap, BTreeSet};

use rosen_core::ChangeState;
use rosen_protocol::{
    ChangeStats, ChangeTimeline, CheckKind, CheckLabelStats, ErrorBody, LandPhaseTimings,
    PostLandCheckpointStats, StatsQuery, StatsReport, WindowStats,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use super::internal;
use crate::rpc::change_error_code;

const SERVER_OWNER: &str = "rosen-server";

/// Returns an empty report when no metadata database exists yet.
pub fn empty_report(since_ms: Option<i64>, now_ms: i64) -> Result<StatsReport, ErrorBody> {
    Ok(StatsReport {
        window: empty_window(since_ms, now_ms),
        changes: Vec::new(),
        checkpoints: Vec::new(),
    })
}

/// Computes per-change timelines, checkpoint records and window aggregates.
pub fn compute(
    conn: &Connection,
    query: &StatsQuery,
    now_ms: i64,
) -> Result<StatsReport, ErrorBody> {
    let change_ids = select_changes(conn, query)?;
    let mut changes = Vec::with_capacity(change_ids.len());
    for id in &change_ids {
        changes.push(load_change_stats(conn, id).map_err(internal)?);
    }
    let window = window_aggregates(conn, query.since_ms, now_ms).map_err(internal)?;
    let checkpoints = load_checkpoints(conn, query.since_ms).map_err(internal)?;
    Ok(StatsReport {
        window,
        changes,
        checkpoints,
    })
}

/// Computes only the window aggregates, shared with the Collapse state feed (R9).
pub fn window_aggregates(
    conn: &Connection,
    since_ms: Option<i64>,
    now_ms: i64,
) -> rusqlite::Result<WindowStats> {
    let mut land_totals = load_land_latencies(conn, since_ms)?;
    let event_lands = count_events(conn, "landed", since_ms)?;
    let lands = (land_totals.len() as u64).max(event_lands);
    land_totals.sort_unstable();

    let (mut snap_to_land, mut named_to_land) = load_lifecycle_latencies(conn, since_ms)?;
    snap_to_land.sort_unstable();
    named_to_land.sort_unstable();

    let queue_wait_by_label = load_queue_wait_by_label(conn, since_ms)?;
    let (post_land_checks, failed_checkpoints) = count_checkpoints(conn, since_ms)?;
    let event_failures = count_events(conn, "post_land_failure", since_ms)?;
    let post_land_failures = event_failures.max(failed_checkpoints);
    let post_land_reverts = count_events(conn, "reverted", since_ms)?;
    let (conflicted_changes, total_changes) = count_conflict_window(conn, since_ms)?;

    let post_land_failure_rate = if lands > 0 {
        post_land_failures as f64 / lands as f64
    } else if post_land_checks > 0 {
        failed_checkpoints as f64 / post_land_checks as f64
    } else {
        0.0
    };
    let revert_rate = ratio(post_land_reverts, lands);
    let conflict_rate = ratio(conflicted_changes, total_changes);

    Ok(WindowStats {
        since_ms,
        as_of_ms: now_ms,
        lands,
        land_latency_p50_ms: percentile(&land_totals, 50),
        land_latency_p95_ms: percentile(&land_totals, 95),
        snapshot_to_land_p50_ms: percentile(&snap_to_land, 50),
        snapshot_to_land_p95_ms: percentile(&snap_to_land, 95),
        named_to_land_p50_ms: percentile(&named_to_land, 50),
        named_to_land_p95_ms: percentile(&named_to_land, 95),
        queue_wait_by_label,
        post_land_checks,
        post_land_failures,
        post_land_reverts,
        post_land_failure_rate,
        revert_rate,
        conflicted_changes,
        total_changes,
        conflict_rate,
    })
}

/// Nearest-rank percentile over a sorted non-decreasing slice.
pub fn percentile(sorted: &[u64], pct: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (sorted.len() * pct).div_ceil(100);
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    Some(sorted[idx])
}

fn empty_window(since_ms: Option<i64>, now_ms: i64) -> WindowStats {
    WindowStats {
        since_ms,
        as_of_ms: now_ms,
        lands: 0,
        land_latency_p50_ms: None,
        land_latency_p95_ms: None,
        snapshot_to_land_p50_ms: None,
        snapshot_to_land_p95_ms: None,
        named_to_land_p50_ms: None,
        named_to_land_p95_ms: None,
        queue_wait_by_label: Vec::new(),
        post_land_checks: 0,
        post_land_failures: 0,
        post_land_reverts: 0,
        post_land_failure_rate: 0.0,
        revert_rate: 0.0,
        conflicted_changes: 0,
        total_changes: 0,
        conflict_rate: 0.0,
    }
}

fn select_changes(conn: &Connection, query: &StatsQuery) -> Result<Vec<String>, ErrorBody> {
    if let Some(prefix) = &query.change {
        let id = crate::changes::resolve(conn, prefix)
            .map_err(|err| ErrorBody::new(change_error_code(&err), err.to_string()))?;
        return Ok(vec![id.to_string()]);
    }
    active_change_ids(conn, query.since_ms).map_err(internal)
}

fn active_change_ids(conn: &Connection, since_ms: Option<i64>) -> rusqlite::Result<Vec<String>> {
    if !table_exists(conn, "changes")? {
        return Ok(Vec::new());
    }
    let Some(since) = since_ms else {
        let mut stmt =
            conn.prepare("SELECT id FROM changes WHERE owner != ?1 ORDER BY created_at, id")?;
        let rows = stmt.query_map([SERVER_OWNER], |r| r.get(0))?;
        return rows.collect();
    };
    let mut ids = BTreeSet::new();
    collect_ids(
        conn,
        "SELECT id FROM changes WHERE owner != ?1 AND (created_at >= ?2 OR updated_at >= ?2)",
        params![SERVER_OWNER, since],
        &mut ids,
    )?;
    if table_exists(conn, "events")? {
        collect_ids(
            conn,
            "SELECT DISTINCT e.change_id FROM events e
             JOIN changes c ON c.id = e.change_id
             WHERE c.owner != ?1 AND e.created_at >= ?2",
            params![SERVER_OWNER, since],
            &mut ids,
        )?;
    }
    if table_exists(conn, "land_timings")? {
        collect_ids(
            conn,
            "SELECT DISTINCT l.change_id FROM land_timings l
             JOIN changes c ON c.id = l.change_id
             WHERE c.owner != ?1 AND l.recorded_at >= ?2",
            params![SERVER_OWNER, since],
            &mut ids,
        )?;
        if table_exists(conn, "land_timing_layers")? {
            collect_ids(
                conn,
                "SELECT DISTINCT tl.change_id FROM land_timing_layers tl
                 JOIN land_timings l ON l.id = tl.timing_id
                 JOIN changes c ON c.id = tl.change_id
                 WHERE c.owner != ?1 AND l.recorded_at >= ?2",
                params![SERVER_OWNER, since],
                &mut ids,
            )?;
        }
    }
    if table_exists(conn, "run_timings")? {
        collect_ids(
            conn,
            "SELECT DISTINCT r.change_id FROM run_timings r
             JOIN changes c ON c.id = r.change_id
             WHERE c.owner != ?1 AND r.queued_at >= ?2",
            params![SERVER_OWNER, since],
            &mut ids,
        )?;
    }
    Ok(ids.into_iter().collect())
}

fn collect_ids(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    out: &mut BTreeSet<String>,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |r| r.get::<_, Option<String>>(0))?;
    for row in rows {
        if let Some(id) = row? {
            out.insert(id);
        }
    }
    Ok(())
}

fn load_change_stats(conn: &Connection, change_id: &str) -> rusqlite::Result<ChangeStats> {
    let (owner, machine, state_raw, created_at): (String, String, String, i64) = conn.query_row(
        "SELECT owner, owner_machine, state, created_at FROM changes WHERE id = ?1",
        [change_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let owner = if machine.is_empty() {
        owner
    } else {
        format!("{owner}@{machine}")
    };
    let state = parse_state(&state_raw);
    let (mut timeline, rebases, conflict_events) = load_event_summary(conn, change_id, created_at)?;
    let (stored_conflicts, open_conflicts) = load_conflict_counts(conn, change_id)?;
    let (run_count, run_wait_ms, run_duration_ms) = load_change_runs(conn, change_id)?;
    let (land_phases, landed_recorded_at) = load_change_land_phases(conn, change_id)?;
    if timeline.landed_at_ms.is_none() {
        timeline.landed_at_ms = landed_recorded_at;
    }
    fill_timeline_durations(&mut timeline);
    Ok(ChangeStats {
        change: change_id.to_owned(),
        owner,
        state,
        timeline,
        rebases,
        stored_conflicts,
        open_conflicts,
        conflict_events,
        run_count,
        run_wait_ms,
        run_duration_ms,
        land_phases,
    })
}

fn load_event_summary(
    conn: &Connection,
    change_id: &str,
    created_at: i64,
) -> rusqlite::Result<(ChangeTimeline, u64, u64)> {
    let mut timeline = ChangeTimeline {
        created_at_ms: created_at,
        ..ChangeTimeline::default()
    };
    let (mut rebases, mut conflict_events) = (0u64, 0u64);
    if !table_exists(conn, "events")? {
        return Ok((timeline, rebases, conflict_events));
    }
    let mut stmt = conn.prepare_cached(
        "SELECT kind, detail, created_at FROM events WHERE change_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map([change_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (kind, detail, at) = row?;
        match kind.as_str() {
            "snapshot" => {
                timeline.snapshot_at_ms.get_or_insert(at);
                if is_named_snapshot(&detail) {
                    timeline.named_at_ms = Some(at);
                }
            }
            "landed" => timeline.landed_at_ms = Some(at),
            "green" => timeline.green_at_ms = Some(at),
            "reverted" => timeline.reverted_at_ms = Some(at),
            "rebased" => rebases += 1,
            "conflict" => conflict_events += 1,
            _ => {}
        }
    }
    Ok((timeline, rebases, conflict_events))
}

fn fill_timeline_durations(t: &mut ChangeTimeline) {
    t.snapshot_to_named_ms = elapsed_ms(t.snapshot_at_ms, t.named_at_ms);
    t.named_to_landed_ms = elapsed_ms(t.named_at_ms, t.landed_at_ms);
    t.snapshot_to_landed_ms = elapsed_ms(t.snapshot_at_ms, t.landed_at_ms);
    t.landed_to_green_ms = elapsed_ms(t.landed_at_ms, t.green_at_ms);
    t.landed_to_reverted_ms = elapsed_ms(t.landed_at_ms, t.reverted_at_ms);
}

fn elapsed_ms(from: Option<i64>, to: Option<i64>) -> Option<u64> {
    let (from, to) = (from?, to?);
    (to >= from).then(|| u64::try_from(to - from).unwrap_or(u64::MAX))
}

fn is_named_snapshot(detail: &str) -> bool {
    serde_json::from_str::<Value>(detail)
        .ok()
        .and_then(|v| v.get("named").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn load_conflict_counts(conn: &Connection, change_id: &str) -> rusqlite::Result<(u64, u64)> {
    if !table_exists(conn, "conflicts")? {
        return Ok((0, 0));
    }
    let (total, open): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(CASE WHEN state = 'open' THEN 1 ELSE 0 END), 0)
         FROM conflicts WHERE change_id = ?1",
        [change_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((to_u64(total), to_u64(open)))
}

fn load_change_runs(conn: &Connection, change_id: &str) -> rusqlite::Result<(u64, u64, u64)> {
    if !table_exists(conn, "run_timings")? {
        return Ok((0, 0, 0));
    }
    let (count, wait, run): (i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(MAX(0, started_at - queued_at)), 0),
                COALESCE(SUM(MAX(0, finished_at - started_at)), 0)
         FROM run_timings WHERE change_id = ?1",
        [change_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok((to_u64(count), to_u64(wait), to_u64(run)))
}

fn load_change_land_phases(
    conn: &Connection,
    change_id: &str,
) -> rusqlite::Result<(Option<LandPhaseTimings>, Option<i64>)> {
    if !table_exists(conn, "land_timings")? {
        return Ok((None, None));
    }
    let sql = if table_exists(conn, "land_timing_layers")? {
        "SELECT outcome, attempts,
                registry_ms, engine_ms, attempts_ms, facts_ms, tiers_ms,
                merge_ms, sizes_ms, receipt_ms, write_ms, cas_ms,
                write_and_cas_ms, restack_ms, decision_and_restack_ms,
                answer_ms, total_ms, recorded_at
         FROM (
             SELECT outcome, attempts,
                    registry_ms, engine_ms, attempts_ms, facts_ms, tiers_ms,
                    merge_ms, sizes_ms, receipt_ms, write_ms, cas_ms,
                    write_and_cas_ms, restack_ms, decision_and_restack_ms,
                    answer_ms, total_ms, recorded_at, id
             FROM land_timings WHERE change_id = ?1
             UNION ALL
             SELECT l.outcome, l.attempts,
                    l.registry_ms, l.engine_ms, l.attempts_ms, l.facts_ms, l.tiers_ms,
                    l.merge_ms, l.sizes_ms, l.receipt_ms, l.write_ms, l.cas_ms,
                    l.write_and_cas_ms, l.restack_ms, l.decision_and_restack_ms,
                    l.answer_ms, l.total_ms, l.recorded_at, l.id
             FROM land_timing_layers tl
             JOIN land_timings l ON l.id = tl.timing_id
             WHERE tl.change_id = ?1
         )
         ORDER BY CASE WHEN outcome = 'landed' THEN 0 ELSE 1 END, recorded_at DESC, id DESC
         LIMIT 1"
    } else {
        "SELECT outcome, attempts,
                registry_ms, engine_ms, attempts_ms, facts_ms, tiers_ms,
                merge_ms, sizes_ms, receipt_ms, write_ms, cas_ms,
                write_and_cas_ms, restack_ms, decision_and_restack_ms,
                answer_ms, total_ms, recorded_at
         FROM land_timings WHERE change_id = ?1
         ORDER BY CASE WHEN outcome = 'landed' THEN 0 ELSE 1 END, recorded_at DESC, id DESC
         LIMIT 1"
    };
    let row = conn
        .query_row(sql, [change_id], decode_land_phase_row)
        .optional()?;
    Ok(match row {
        Some((outcome, phases, at)) => {
            let landed_at = (outcome == "landed").then_some(at);
            (Some(phases), landed_at)
        }
        None => (None, None),
    })
}

fn decode_land_phase_row(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, LandPhaseTimings, i64)> {
    Ok((
        r.get::<_, String>(0)?,
        LandPhaseTimings {
            attempts: u32::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
            registry_ms: to_u64(r.get(2)?),
            engine_ms: to_u64(r.get(3)?),
            attempts_ms: to_u64(r.get(4)?),
            facts_ms: to_u64(r.get(5)?),
            tiers_ms: to_u64(r.get(6)?),
            merge_ms: to_u64(r.get(7)?),
            sizes_ms: to_u64(r.get(8)?),
            receipt_ms: to_u64(r.get(9)?),
            write_ms: to_u64(r.get(10)?),
            cas_ms: to_u64(r.get(11)?),
            write_and_cas_ms: to_u64(r.get(12)?),
            restack_ms: to_u64(r.get(13)?),
            decision_and_restack_ms: to_u64(r.get(14)?),
            answer_ms: to_u64(r.get(15)?),
            total_ms: to_u64(r.get(16)?),
        },
        r.get::<_, i64>(17)?,
    ))
}

fn load_land_latencies(conn: &Connection, since_ms: Option<i64>) -> rusqlite::Result<Vec<u64>> {
    if !table_exists(conn, "land_timings")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT total_ms FROM land_timings
         WHERE outcome = 'landed' AND (?1 IS NULL OR recorded_at >= ?1)",
    )?;
    let rows = stmt.query_map([since_ms], |r| Ok(to_u64(r.get(0)?)))?;
    rows.collect()
}

fn load_lifecycle_latencies(
    conn: &Connection,
    since_ms: Option<i64>,
) -> rusqlite::Result<(Vec<u64>, Vec<u64>)> {
    let ids = active_change_ids(conn, since_ms)?;
    let (mut snap_to_land, mut named_to_land) = (Vec::new(), Vec::new());
    for id in ids {
        let stats = load_change_stats(conn, &id)?;
        let Some(landed_at) = stats.timeline.landed_at_ms else {
            continue;
        };
        if since_ms.is_some_and(|since| landed_at < since) {
            continue;
        }
        if let Some(ms) = stats.timeline.snapshot_to_landed_ms {
            snap_to_land.push(ms);
        }
        if let Some(ms) = stats.timeline.named_to_landed_ms {
            named_to_land.push(ms);
        }
    }
    Ok((snap_to_land, named_to_land))
}

fn load_queue_wait_by_label(
    conn: &Connection,
    since_ms: Option<i64>,
) -> rusqlite::Result<Vec<CheckLabelStats>> {
    if !table_exists(conn, "run_timings")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT label, queued_at, started_at, finished_at, exit_status, reused
         FROM run_timings WHERE (?1 IS NULL OR queued_at >= ?1)",
    )?;
    type Bucket = (u64, u64, u64, Vec<u64>, Vec<u64>);
    let mut grouped: BTreeMap<String, Bucket> = BTreeMap::new();
    let rows = stmt.query_map([since_ms], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<i32>>(4)?,
            r.get::<_, i64>(5)? != 0,
        ))
    })?;
    for row in rows {
        let (label, queued, started, finished, exit_status, reused) = row?;
        let entry = grouped.entry(label).or_default();
        entry.0 += 1;
        if reused {
            entry.1 += 1;
        }
        if exit_status.is_some_and(|code| code != 0) {
            entry.2 += 1;
        }
        entry.3.push(to_u64(started.saturating_sub(queued)));
        entry.4.push(to_u64(finished.saturating_sub(started)));
    }
    Ok(grouped
        .into_iter()
        .map(
            |(label, (runs, reused, failures, mut waits, mut durations))| {
                waits.sort_unstable();
                durations.sort_unstable();
                CheckLabelStats {
                    label,
                    runs,
                    reused,
                    failures,
                    wait_p50_ms: percentile(&waits, 50),
                    wait_p95_ms: percentile(&waits, 95),
                    duration_p50_ms: percentile(&durations, 50),
                    duration_p95_ms: percentile(&durations, 95),
                }
            },
        )
        .collect())
}

fn load_checkpoints(
    conn: &Connection,
    since_ms: Option<i64>,
) -> rusqlite::Result<Vec<PostLandCheckpointStats>> {
    if !table_exists(conn, "post_land_checkpoints")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT kind, platform, commit_id, batch_size, build_ms, lint_ms, test_ms,
                bisect_steps, outcome, started_at, finished_at
         FROM post_land_checkpoints
         WHERE (?1 IS NULL OR started_at >= ?1)
         ORDER BY started_at, id",
    )?;
    let rows = stmt.query_map([since_ms], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, String>(8)?,
            r.get::<_, i64>(9)?,
            r.get::<_, Option<i64>>(10)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (kind, platform, commit, batch, build, lint, test, steps, outcome, started, finished) =
            row?;
        let Ok(commit) = commit.parse() else {
            continue;
        };
        out.push(PostLandCheckpointStats {
            kind: if kind == "build" {
                CheckKind::Build
            } else {
                CheckKind::Test
            },
            platform: (!platform.is_empty()).then_some(platform),
            commit,
            batch_size: to_u64(batch),
            build_ms: build.map(to_u64),
            lint_ms: lint.map(to_u64),
            test_ms: test.map(to_u64),
            bisect_steps: to_u64(steps),
            outcome,
            started_at_ms: started,
            finished_at_ms: finished,
        });
    }
    Ok(out)
}

fn count_checkpoints(conn: &Connection, since_ms: Option<i64>) -> rusqlite::Result<(u64, u64)> {
    if !table_exists(conn, "post_land_checkpoints")? {
        return Ok((0, 0));
    }
    let (total, failed): (i64, i64) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN outcome IN ('culprit', 'bisecting', 'flaky') THEN 1 ELSE 0 END), 0)
         FROM post_land_checkpoints WHERE (?1 IS NULL OR started_at >= ?1)",
        [since_ms],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((to_u64(total), to_u64(failed)))
}

fn count_events(conn: &Connection, kind: &str, since_ms: Option<i64>) -> rusqlite::Result<u64> {
    if !table_exists(conn, "events")? {
        return Ok(0);
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events
         WHERE kind = ?1 AND (?2 IS NULL OR created_at >= ?2)",
        params![kind, since_ms],
        |r| r.get(0),
    )?;
    Ok(to_u64(count))
}

fn count_conflict_window(conn: &Connection, since_ms: Option<i64>) -> rusqlite::Result<(u64, u64)> {
    let ids = active_change_ids(conn, since_ms)?;
    let total = ids.len() as u64;
    if total == 0 {
        return Ok((0, 0));
    }
    let mut conflicted = 0u64;
    for id in &ids {
        let has_event = table_exists(conn, "events")?
            && conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM events
                 WHERE change_id = ?1 AND kind = 'conflict'
                   AND (?2 IS NULL OR created_at >= ?2))",
                params![id, since_ms],
                |r| r.get::<_, bool>(0),
            )?;
        let has_stored = if has_event {
            true
        } else {
            let (stored, _) = load_conflict_counts(conn, id)?;
            stored > 0
        };
        if has_stored {
            conflicted += 1;
        }
    }
    Ok((conflicted, total))
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |r| r.get(0),
    )
}

fn parse_state(raw: &str) -> ChangeState {
    match raw {
        "ready" => ChangeState::Ready,
        "landed" => ChangeState::Landed,
        "abandoned" => ChangeState::Abandoned,
        _ => ChangeState::Open,
    }
}

fn ratio(num: u64, denom: u64) -> f64 {
    if denom == 0 {
        0.0
    } else {
        num as f64 / denom as f64
    }
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}
