//! Hourly fleet timing metrics as a Collapse C12 state-feed section (R9, #449, #465).
//!
//! Emits per-`(hour, machine)` counts and durations under `"metrics": {"version": 1, "hours": [...]}`
//! over the rolling 7-day window without clock fields, so an idle project never
//! wakes the endpoint.

use std::path::Path;

use super::unit::{Section, Unit};

/// Builds the `metrics` unit for the rolling 7-day window ending at `now_ms`.
/// Returns `Ok(None)` when no timing buckets exist yet.
/// Reads SQLite: call it on the blocking pool.
pub fn build(metadata: &Path, now_ms: i64) -> rusqlite::Result<Option<Unit>> {
    let conn = crate::metadata::open(metadata)?;
    let hours = crate::stats::metrics::hourly_metrics(&conn, now_ms)?;
    if hours.is_empty() {
        return Ok(None);
    }
    let items = hours
        .into_iter()
        .map(|bucket| (Section::Stats, bucket.to_json()))
        .collect();
    Ok(Some(Unit::new("metrics".to_owned(), items)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_push::batch;
    use rosen_core::ObjectId;
    use rosen_protocol::{CheckKind, RecordRunTiming};

    /// Risk (#465, R9/C12): timing aggregates must encode under Collapse's
    /// `metrics: {version: 1, hours: [...]}` contract with aligned UTC hour
    /// boundaries, omit Velocity-owned fields, avoid double-counting failures,
    /// and keep a stable fingerprint when only the clock moves.
    #[test]
    fn metrics_document_matches_collapse_c12_wire_contract() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("metadata.sqlite");
        let conn = crate::metadata::open(&db).unwrap();
        assert!(build(&db, 1_700_000_000_000).unwrap().is_none());

        let now = 1_700_000_000_000i64; // 2023-11-14T22:13:20Z -> hour 1_699_999_200
        let hour = 1_699_999_200i64;
        crate::stats::store::record_run(
            &conn,
            &RecordRunTiming {
                label: "cargo test".into(),
                class: "verify".into(),
                change: None,
                queued_at_ms: now - 2_000,
                started_at_ms: now - 1_900,
                finished_at_ms: now - 1_500,
                exit_status: Some(0),
                reused: false,
            },
            None,
            "agent-1",
        )
        .unwrap();
        crate::stats::store::record_run(
            &conn,
            &RecordRunTiming {
                label: "cargo clippy".into(),
                class: "verify".into(),
                change: None,
                queued_at_ms: now - 1_000,
                started_at_ms: now - 700,
                finished_at_ms: now - 200,
                exit_status: Some(0),
                reused: false,
            },
            None,
            "agent-1",
        )
        .unwrap();
        let commit: ObjectId = "1111111111111111111111111111111111111111".parse().unwrap();
        crate::stats::store::record_checkpoint_passed(
            &conn,
            "main",
            CheckKind::Test,
            None,
            &commit,
            1,
            crate::stats::store::CheckpointTiming::at(now - 500),
        )
        .unwrap();
        drop(conn);

        let a = build(&db, now).unwrap().expect("non-empty metrics unit");
        let b = build(&db, now + 60_000)
            .unwrap()
            .expect("non-empty metrics unit");
        assert_eq!(a.fingerprint, b.fingerprint);

        let batches = batch::split(Some("feed-1"), vec![a], 5, 0);
        assert_eq!(batches.len(), 1);
        let parsed: serde_json::Value = serde_json::from_slice(&batches[0].body).unwrap();
        assert!(parsed.get("stats").is_none(), "must not emit ignored stats");
        assert_eq!(parsed["metrics"]["version"], 1);
        let hours = parsed["metrics"]["hours"].as_array().unwrap();
        assert_eq!(hours.len(), 2);

        // Repo-wide post-land checkpoint bucket (`machine: ""`).
        assert_eq!(hours[0]["hour"], hour);
        assert_eq!(hours[0]["machine"], "");
        assert_eq!(hours[0]["post_land_checks"], 1);
        assert_eq!(hours[0]["post_land_failures"], 0);
        assert!(hours[0].get("tokens").is_none());
        assert!(hours[0].get("idle_slot_seconds").is_none());

        // Per-machine run queue wait bucket (`machine: "agent-1"`).
        assert_eq!(hours[1]["hour"], hour);
        assert_eq!(hours[1]["machine"], "agent-1");
        assert_eq!(hours[1]["check_queue_wait_count"], 2);
        assert_eq!(hours[1]["check_queue_wait_ms_sum"], 400);
        assert_eq!(hours[1]["check_queue_wait_p50_ms"], 100);
        assert!(hours[1].get("seq").is_none());
    }
}
