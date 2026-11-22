//! Hourly fleet development-speed metrics for the Collapse C12 state feed
//! (`metrics: {version: 1, hours: [...]}`; SPEC R9, #449, #465).
//!
//! Emits counts and durations only, grouped by `(hour, machine)` over the
//! rolling 7-day window Collapse displays (24h and 7d). Velocity-owned fields
//! (`dispatched_to_land_*`, `idle_slot_seconds`, `tokens`) are omitted so
//! Collapse's `COALESCE` upsert preserves them.

use std::collections::BTreeMap;

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use super::query::percentile;

/// Collapse C12 `metrics` document schema version.
pub const VERSION: u64 = 1;
/// Rolling 7-day window in UTC hours, matching Collapse `readFleetSpeed`.
pub const WINDOW_HOURS: i64 = 168;

const HOUR_SECS: i64 = 3_600;
const HOUR_MS: i64 = 3_600_000;
const MAX_MACHINE_BYTES: usize = 256;

/// One `(hour, machine)` metrics bucket ready for Collapse C12 intake.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HourlyMetrics {
    pub hour: i64,
    pub machine: String,
    pub lands: Option<u64>,
    pub post_land_checks: Option<u64>,
    pub post_land_failures: Option<u64>,
    pub reverts: Option<u64>,
    pub check_queue_wait_ms_sum: Option<u64>,
    pub check_queue_wait_count: Option<u64>,
    pub check_queue_wait_p50_ms: Option<u64>,
}

impl HourlyMetrics {
    /// Encodes this bucket in Collapse's `wireMetricHour` JSON shape, omitting
    /// unset fields so Collapse's `COALESCE` keeps values from other senders.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("hour".into(), json!(self.hour));
        map.insert("machine".into(), json!(self.machine));
        insert_opt(&mut map, "lands", self.lands);
        insert_opt(&mut map, "post_land_checks", self.post_land_checks);
        insert_opt(&mut map, "post_land_failures", self.post_land_failures);
        insert_opt(&mut map, "reverts", self.reverts);
        insert_opt(
            &mut map,
            "check_queue_wait_ms_sum",
            self.check_queue_wait_ms_sum,
        );
        insert_opt(
            &mut map,
            "check_queue_wait_count",
            self.check_queue_wait_count,
        );
        insert_opt(
            &mut map,
            "check_queue_wait_p50_ms",
            self.check_queue_wait_p50_ms,
        );
        Value::Object(map)
    }
}

#[derive(Default)]
struct Bucket {
    lands: u64,
    post_land_checks: u64,
    post_land_failures: u64,
    reverts: u64,
    queue_waits: Vec<u64>,
}

/// Aggregates Rosen's timing tables into sorted `(hour, machine)` buckets over
/// the rolling 7-day window ending at `now_ms`.
pub fn hourly_metrics(conn: &Connection, now_ms: i64) -> rusqlite::Result<Vec<HourlyMetrics>> {
    let Some(since_ms) = window_start_ms(now_ms) else {
        return Ok(Vec::new());
    };
    let mut buckets: BTreeMap<(i64, String), Bucket> = BTreeMap::new();
    collect_event_lands(conn, since_ms, &mut buckets)?;
    collect_fallback_timing_lands(conn, since_ms, &mut buckets)?;
    collect_post_land(conn, since_ms, &mut buckets)?;
    collect_reverts(conn, since_ms, &mut buckets)?;
    collect_run_waits(conn, since_ms, &mut buckets)?;
    Ok(buckets.into_iter().map(finalize_bucket).collect())
}

fn finalize_bucket(((hour, machine), mut bucket): ((i64, String), Bucket)) -> HourlyMetrics {
    let (wait_sum, wait_count, wait_p50) = if bucket.queue_waits.is_empty() {
        (None, None, None)
    } else {
        bucket.queue_waits.sort_unstable();
        let sum = bucket
            .queue_waits
            .iter()
            .copied()
            .fold(0u64, u64::saturating_add);
        let count = bucket.queue_waits.len() as u64;
        let p50 = percentile(&bucket.queue_waits, 50);
        (Some(sum), Some(count), p50)
    };
    HourlyMetrics {
        hour,
        machine,
        lands: (bucket.lands > 0).then_some(bucket.lands),
        post_land_checks: (bucket.post_land_checks > 0).then_some(bucket.post_land_checks),
        post_land_failures: (bucket.post_land_failures > 0
            || bucket.post_land_checks > 0
            || bucket.lands > 0)
            .then_some(bucket.post_land_failures),
        reverts: (bucket.reverts > 0 || bucket.lands > 0).then_some(bucket.reverts),
        check_queue_wait_ms_sum: wait_sum,
        check_queue_wait_count: wait_count,
        check_queue_wait_p50_ms: wait_p50,
    }
}

fn collect_event_lands(
    conn: &Connection,
    since_ms: i64,
    buckets: &mut BTreeMap<(i64, String), Bucket>,
) -> rusqlite::Result<()> {
    if !table_exists(conn, "events")? {
        return Ok(());
    }
    let sql = if table_exists(conn, "changes")? {
        "SELECT e.created_at, COALESCE(c.owner_machine, '')
         FROM events e
         LEFT JOIN changes c ON c.id = e.change_id
         WHERE e.kind = 'landed' AND e.created_at >= ?1"
    } else {
        "SELECT created_at, '' FROM events WHERE kind = 'landed' AND created_at >= ?1"
    };
    for (at_ms, machine) in query_time_machine(conn, sql, since_ms)? {
        if let Some(hour) = floor_hour_secs(at_ms) {
            buckets.entry((hour, machine)).or_default().lands += 1;
        }
    }
    Ok(())
}

fn collect_fallback_timing_lands(
    conn: &Connection,
    since_ms: i64,
    buckets: &mut BTreeMap<(i64, String), Bucket>,
) -> rusqlite::Result<()> {
    if !table_exists(conn, "land_timings")? {
        return Ok(());
    }
    let has_events = table_exists(conn, "events")?;
    let has_changes = table_exists(conn, "changes")?;
    let sql = match (has_events, has_changes) {
        (true, true) => {
            "SELECT l.recorded_at, COALESCE(c.owner_machine, '')
             FROM land_timings l
             LEFT JOIN changes c ON c.id = l.change_id
             WHERE l.outcome = 'landed' AND l.recorded_at >= ?1
               AND NOT EXISTS (
                   SELECT 1 FROM events e
                   WHERE e.kind = 'landed' AND e.change_id = l.change_id
                     AND e.created_at >= ?1
               )"
        }
        (true, false) => {
            "SELECT l.recorded_at, ''
             FROM land_timings l
             WHERE l.outcome = 'landed' AND l.recorded_at >= ?1
               AND NOT EXISTS (
                   SELECT 1 FROM events e
                   WHERE e.kind = 'landed' AND e.change_id = l.change_id
                     AND e.created_at >= ?1
               )"
        }
        (false, true) => {
            "SELECT l.recorded_at, COALESCE(c.owner_machine, '')
             FROM land_timings l
             LEFT JOIN changes c ON c.id = l.change_id
             WHERE l.outcome = 'landed' AND l.recorded_at >= ?1"
        }
        (false, false) => {
            "SELECT recorded_at, '' FROM land_timings
             WHERE outcome = 'landed' AND recorded_at >= ?1"
        }
    };
    for (at_ms, machine) in query_time_machine(conn, sql, since_ms)? {
        if let Some(hour) = floor_hour_secs(at_ms) {
            buckets.entry((hour, machine)).or_default().lands += 1;
        }
    }
    Ok(())
}

fn collect_post_land(
    conn: &Connection,
    since_ms: i64,
    buckets: &mut BTreeMap<(i64, String), Bucket>,
) -> rusqlite::Result<()> {
    let event_failures = collect_failure_events(conn, since_ms, buckets)?;
    let failed_checkpoints = collect_checkpoints(conn, since_ms, buckets)?;
    // Attribute only excess checkpoint failures (e.g. bisecting/flaky with no
    // culprit event yet) to the repo-wide bucket so combined totals never
    // double-count a failure already attributed to a culprit's machine.
    for (hour, failed_cp) in failed_checkpoints {
        let ev = event_failures.get(&hour).copied().unwrap_or(0);
        if failed_cp > ev {
            buckets
                .entry((hour, String::new()))
                .or_default()
                .post_land_failures += failed_cp - ev;
        }
    }
    Ok(())
}

fn collect_failure_events(
    conn: &Connection,
    since_ms: i64,
    buckets: &mut BTreeMap<(i64, String), Bucket>,
) -> rusqlite::Result<BTreeMap<i64, u64>> {
    let mut per_hour: BTreeMap<i64, u64> = BTreeMap::new();
    if !table_exists(conn, "events")? {
        return Ok(per_hour);
    }
    let sql = if table_exists(conn, "changes")? {
        "SELECT e.created_at, COALESCE(c.owner_machine, '')
         FROM events e
         LEFT JOIN changes c ON c.id = e.change_id
         WHERE e.kind = 'post_land_failure' AND e.created_at >= ?1"
    } else {
        "SELECT created_at, '' FROM events
         WHERE kind = 'post_land_failure' AND created_at >= ?1"
    };
    for (at_ms, machine) in query_time_machine(conn, sql, since_ms)? {
        if let Some(hour) = floor_hour_secs(at_ms) {
            buckets
                .entry((hour, machine))
                .or_default()
                .post_land_failures += 1;
            *per_hour.entry(hour).or_default() += 1;
        }
    }
    Ok(per_hour)
}

fn collect_checkpoints(
    conn: &Connection,
    since_ms: i64,
    buckets: &mut BTreeMap<(i64, String), Bucket>,
) -> rusqlite::Result<BTreeMap<i64, u64>> {
    let mut failed: BTreeMap<i64, u64> = BTreeMap::new();
    if !table_exists(conn, "post_land_checkpoints")? {
        return Ok(failed);
    }
    let mut stmt = conn
        .prepare("SELECT started_at, outcome FROM post_land_checkpoints WHERE started_at >= ?1")?;
    let rows = stmt.query_map([since_ms], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (started_at, outcome) = row?;
        if let Some(hour) = floor_hour_secs(started_at) {
            buckets
                .entry((hour, String::new()))
                .or_default()
                .post_land_checks += 1;
            if matches!(outcome.as_str(), "culprit" | "bisecting" | "flaky") {
                *failed.entry(hour).or_default() += 1;
            }
        }
    }
    Ok(failed)
}

fn collect_reverts(
    conn: &Connection,
    since_ms: i64,
    buckets: &mut BTreeMap<(i64, String), Bucket>,
) -> rusqlite::Result<()> {
    if !table_exists(conn, "events")? {
        return Ok(());
    }
    let sql = if table_exists(conn, "changes")? {
        "SELECT e.created_at, COALESCE(c.owner_machine, '')
         FROM events e
         LEFT JOIN changes c ON c.id = e.change_id
         WHERE e.kind = 'reverted' AND e.created_at >= ?1"
    } else {
        "SELECT created_at, '' FROM events WHERE kind = 'reverted' AND created_at >= ?1"
    };
    for (at_ms, machine) in query_time_machine(conn, sql, since_ms)? {
        if let Some(hour) = floor_hour_secs(at_ms) {
            buckets.entry((hour, machine)).or_default().reverts += 1;
        }
    }
    Ok(())
}

fn collect_run_waits(
    conn: &Connection,
    since_ms: i64,
    buckets: &mut BTreeMap<(i64, String), Bucket>,
) -> rusqlite::Result<()> {
    if !table_exists(conn, "run_timings")? {
        return Ok(());
    }
    let mut stmt = conn
        .prepare("SELECT machine, queued_at, started_at FROM run_timings WHERE queued_at >= ?1")?;
    let rows = stmt.query_map([since_ms], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (machine, queued_at, started_at) = row?;
        if let Some(hour) = floor_hour_secs(queued_at) {
            let wait = u64::try_from(started_at.saturating_sub(queued_at)).unwrap_or(0);
            buckets
                .entry((hour, sanitize_machine(&machine)))
                .or_default()
                .queue_waits
                .push(wait);
        }
    }
    Ok(())
}

fn query_time_machine(
    conn: &Connection,
    sql: &str,
    since_ms: i64,
) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![since_ms], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            sanitize_machine(&r.get::<_, String>(1)?),
        ))
    })?;
    rows.collect()
}

fn window_start_ms(now_ms: i64) -> Option<i64> {
    let current_hour = floor_hour_secs(now_ms)?;
    let start_hour = current_hour
        .saturating_sub((WINDOW_HOURS - 1) * HOUR_SECS)
        .max(HOUR_SECS);
    Some(start_hour.saturating_mul(1_000))
}

/// Floors Unix milliseconds to a positive UTC hour boundary in Unix seconds.
pub fn floor_hour_secs(ms: i64) -> Option<i64> {
    let hour = ms.div_euclid(HOUR_MS).saturating_mul(HOUR_SECS);
    (hour > 0).then_some(hour)
}

fn sanitize_machine(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if matches!(ch, '\r' | '\n' | '\0') {
            continue;
        }
        if out.len() + ch.len_utf8() > MAX_MACHINE_BYTES {
            break;
        }
        out.push(ch);
    }
    out
}

fn insert_opt(map: &mut Map<String, Value>, key: &str, value: Option<u64>) {
    if let Some(v) = value {
        map.insert(key.into(), json!(v));
    }
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |r| r.get(0),
    )
}
