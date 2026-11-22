//! `rosen stats`: per-change timelines, post-land checkpoints and window
//! aggregates for Velocity and agents (SPEC R10, #449).

use std::time::{SystemTime, UNIX_EPOCH};

use rosen_client::protocol::{
    ChangeStats, PostLandCheckpointStats, StatsQuery, StatsReport, WindowStats,
};
use rosen_client::{Endpoint, Error};
use serde_json::json;

use crate::commands::StatsArgs;
use crate::output::STATS_SCHEMA;
use crate::server::Succeeded;

pub async fn run(json: bool, endpoint: &Endpoint, args: &StatsArgs) -> Result<Succeeded, Error> {
    let now_ms = now_millis();
    let since_ms = args
        .since
        .as_deref()
        .map(|raw| parse_since(raw, now_ms))
        .transpose()
        .map_err(Error::InvalidArgument)?;
    let query = StatsQuery {
        since_ms,
        change: args.change.clone(),
    };
    let client = rosen_client::connect_or_start(endpoint).await?;
    let report = client.stats(query).await?;
    if json {
        let body = json!({
            "schema": STATS_SCHEMA,
            "window": report.window,
            "changes": report.changes,
            "checkpoints": report.checkpoints,
        });
        println!("{body}");
    } else {
        print_text(&report);
    }
    Ok(true)
}

fn print_text(report: &StatsReport) {
    print_window(&report.window);
    if !report.changes.is_empty() {
        println!("\nchanges ({}):", report.changes.len());
        for change in &report.changes {
            print_change(change);
        }
    }
    if !report.checkpoints.is_empty() {
        println!("\npost-land checkpoints ({}):", report.checkpoints.len());
        for cp in &report.checkpoints {
            print_checkpoint(cp);
        }
    }
}

fn print_window(w: &WindowStats) {
    println!(
        "lands: {}  land latency p50/p95: {}/{}  snapshot->land p50/p95: {}/{}",
        w.lands,
        fmt_opt_ms(w.land_latency_p50_ms),
        fmt_opt_ms(w.land_latency_p95_ms),
        fmt_opt_ms(w.snapshot_to_land_p50_ms),
        fmt_opt_ms(w.snapshot_to_land_p95_ms),
    );
    println!(
        "rates: post-land failure {:.1}% ({}/{})  revert {:.1}% ({}/{})  conflict {:.1}% ({}/{})",
        w.post_land_failure_rate * 100.0,
        w.post_land_failures,
        w.lands,
        w.revert_rate * 100.0,
        w.post_land_reverts,
        w.lands,
        w.conflict_rate * 100.0,
        w.conflicted_changes,
        w.total_changes,
    );
    if !w.queue_wait_by_label.is_empty() {
        println!("queue wait by check label:");
        for entry in &w.queue_wait_by_label {
            println!(
                "  {}: {} runs ({} reused, {} failed)  wait p50/p95 {}/{}  run p50/p95 {}/{}",
                entry.label,
                entry.runs,
                entry.reused,
                entry.failures,
                fmt_opt_ms(entry.wait_p50_ms),
                fmt_opt_ms(entry.wait_p95_ms),
                fmt_opt_ms(entry.duration_p50_ms),
                fmt_opt_ms(entry.duration_p95_ms),
            );
        }
    }
}

fn print_change(c: &ChangeStats) {
    let short = &c.change[..c.change.len().min(12)];
    println!(
        "  {short} ({}, {}) snap->land {}  land->green {}  rebases {}  conflicts {}  runs {} (wait {}ms, run {}ms)",
        c.state,
        c.owner,
        fmt_opt_ms(c.timeline.snapshot_to_landed_ms),
        fmt_opt_ms(c.timeline.landed_to_green_ms),
        c.rebases,
        c.stored_conflicts.max(c.conflict_events),
        c.run_count,
        c.run_wait_ms,
        c.run_duration_ms,
    );
}

fn print_checkpoint(cp: &PostLandCheckpointStats) {
    let platform = cp
        .platform
        .as_deref()
        .map_or_else(String::new, |p| format!("/{p}"));
    println!(
        "  {}{} {} batch={} outcome={} bisect_steps={} build={} lint={} test={}",
        cp.kind.as_str(),
        platform,
        &cp.commit.to_hex()[..12],
        cp.batch_size,
        cp.outcome,
        cp.bisect_steps,
        fmt_opt_ms(cp.build_ms),
        fmt_opt_ms(cp.lint_ms),
        fmt_opt_ms(cp.test_ms),
    );
}

fn fmt_opt_ms(ms: Option<u64>) -> String {
    ms.map_or_else(|| "-".to_owned(), |v| format!("{v}ms"))
}

/// Parses `--since`: relative durations (`30m`, `1h`, `7d`, `1h30m`, `500ms`),
/// Unix milliseconds, or UTC ISO-8601 (`YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SSZ`).
pub(crate) fn parse_since(raw: &str, now_ms: i64) -> Result<i64, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("empty --since value".to_owned());
    }
    if let Ok(ms) = text.parse::<i64>() {
        if ms >= 0 {
            return Ok(ms);
        }
    }
    if let Some(duration_ms) = parse_relative_ms(text) {
        return Ok(now_ms.saturating_sub(duration_ms));
    }
    if let Some(ts_ms) = parse_iso8601_utc(text) {
        return Ok(ts_ms);
    }
    Err(format!(
        "invalid --since {raw:?}: use a relative duration (30m, 1h, 7d), Unix ms, or YYYY-MM-DD[THH:MM:SSZ]"
    ))
}

fn parse_relative_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut total: i64 = 0;
    let mut parts = 0usize;
    while i < bytes.len() {
        let num_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == num_start || i >= bytes.len() {
            return None;
        }
        let amount: i64 = text[num_start..i].parse().ok()?;
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit_ms: i64 = match &text[unit_start..i] {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            "w" => 7 * 86_400_000,
            _ => return None,
        };
        total = total.checked_add(amount.checked_mul(unit_ms)?)?;
        parts += 1;
    }
    (parts > 0).then_some(total)
}

fn parse_iso8601_utc(text: &str) -> Option<i64> {
    let (date, time) = match text.split_once('T').or_else(|| text.split_once('t')) {
        Some((d, t)) => (
            d,
            Some(t.strip_suffix('Z').or_else(|| t.strip_suffix('z'))?),
        ),
        None => (text, None),
    };
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (hour, min, sec) = match time {
        None => (0i64, 0i64, 0i64),
        Some(t) => {
            let core = t.split('.').next()?;
            let mut parts = core.split(':');
            let h: i64 = parts.next()?.parse().ok()?;
            let m: i64 = parts.next()?.parse().ok()?;
            let s: i64 = parts.next()?.parse().ok()?;
            if parts.next().is_some() || h > 23 || m > 59 || s > 59 {
                return None;
            }
            (h, m, s)
        }
    };
    let days = days_from_civil(year, month, day)?;
    let secs = days * 86_400 + hour * 3_600 + min * 60 + sec;
    secs.checked_mul(1_000)
}

/// Howard Hinnant's civil-from-days inverse (UTC days since 1970-01-01).
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let d = i64::from(day);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::parse_since;

    #[test]
    fn parses_relative_iso_and_unix_since_values() {
        let now = 1_700_000_000_000;
        assert_eq!(parse_since("1h", now).unwrap(), now - 3_600_000);
        assert_eq!(parse_since("1h30m", now).unwrap(), now - 5_400_000);
        assert_eq!(parse_since("500ms", now).unwrap(), now - 500);
        assert_eq!(
            parse_since("1700000000000", now).unwrap(),
            1_700_000_000_000
        );
        assert_eq!(
            parse_since("2023-11-14T22:13:20Z", now).unwrap(),
            1_700_000_000_000
        );
        assert!(parse_since("bogus", now).is_err());
    }
}
