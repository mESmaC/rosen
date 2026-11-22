//! Window timing aggregates as a Collapse state feed document (R9, #449).
//!
//! Sends only counts, durations, percentiles and rates over the rolling 24-hour
//! window, fingerprinted without its clock fields so the passage of time alone
//! never wakes the endpoint.

use std::path::Path;

use serde_json::json;

use super::unit::{Section, Unit};

/// Rolling window covered by state-feed aggregates (24 hours in milliseconds).
pub const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// Builds the `stats` document for the rolling window ending at `now_ms`.
/// Reads SQLite: call it on the blocking pool.
pub fn build(metadata: &Path, now_ms: i64) -> rusqlite::Result<Unit> {
    let conn = crate::metadata::open(metadata)?;
    let since_ms = now_ms.saturating_sub(WINDOW_MS);
    let stats = crate::stats::query::window_aggregates(&conn, Some(since_ms), now_ms)?;
    let basis = json!({
        "window_ms": WINDOW_MS,
        "lands": stats.lands,
        "land_latency_p50_ms": stats.land_latency_p50_ms,
        "land_latency_p95_ms": stats.land_latency_p95_ms,
        "snapshot_to_land_p50_ms": stats.snapshot_to_land_p50_ms,
        "snapshot_to_land_p95_ms": stats.snapshot_to_land_p95_ms,
        "named_to_land_p50_ms": stats.named_to_land_p50_ms,
        "named_to_land_p95_ms": stats.named_to_land_p95_ms,
        "queue_wait_by_label": stats.queue_wait_by_label,
        "post_land_checks": stats.post_land_checks,
        "post_land_failures": stats.post_land_failures,
        "post_land_reverts": stats.post_land_reverts,
        "post_land_failure_rate": stats.post_land_failure_rate,
        "revert_rate": stats.revert_rate,
        "conflicted_changes": stats.conflicted_changes,
        "total_changes": stats.total_changes,
        "conflict_rate": stats.conflict_rate,
    });
    let mut item = basis.clone();
    if let serde_json::Value::Object(map) = &mut item {
        map.insert("since_ms".into(), json!(since_ms));
        map.insert("as_of_ms".into(), json!(now_ms));
    }
    Ok(Unit::with_basis(
        "stats".to_owned(),
        vec![(Section::Stats, item)],
        &basis,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_push::batch;

    /// Risk (#449, R9): `stats` must appear on state-feed batches with its
    /// window aggregates, and its fingerprint must ignore `since_ms`/`as_of_ms`
    /// so an idle server never sends repeated batches.
    #[test]
    fn stats_document_encodes_on_batch_and_ignores_clock_in_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("metadata.sqlite");
        let conn = crate::metadata::open(&db).unwrap();
        let now = 1_700_000_000_000i64;
        crate::stats::store::record_run(
            &conn,
            &rosen_protocol::RecordRunTiming {
                label: "cargo test".into(),
                class: "verify".into(),
                change: None,
                queued_at_ms: now - 1_000,
                started_at_ms: now - 900,
                finished_at_ms: now - 500,
                exit_status: Some(0),
                reused: false,
            },
            None,
            "server",
        )
        .unwrap();
        drop(conn);

        let a = build(&db, now).unwrap();
        let b = build(&db, now + 60_000).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);

        let batches = batch::split(Some("feed-1"), vec![a], 5, 0);
        assert_eq!(batches.len(), 1);
        let parsed: serde_json::Value = serde_json::from_slice(&batches[0].body).unwrap();
        assert_eq!(parsed["stats"]["seq"], 5);
        assert_eq!(parsed["stats"]["window_ms"], WINDOW_MS);
        assert_eq!(
            parsed["stats"]["queue_wait_by_label"][0]["label"],
            "cargo test"
        );
        assert_eq!(
            parsed["stats"]["queue_wait_by_label"][0]["wait_p50_ms"],
            100
        );
    }
}
