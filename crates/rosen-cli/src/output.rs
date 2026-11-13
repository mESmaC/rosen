//! Versioned CLI output contracts (SPEC R10).

use serde::Serialize;

/// Offline JSON Schema catalog for every emitted JSON line.
pub const CATALOG: &str = include_str!("../schemas/v1.json");

pub const CHANGE_LIST_SCHEMA: &str = "rosen.change.list/v1";
pub const CHANGE_SCHEMA: &str = "rosen.change/v1";
pub const CHANGE_WORKSPACE_SCHEMA: &str = "rosen.change.workspace/v2";
pub const CLAIM_SCHEMA: &str = "rosen.claim/v1";
pub const CONFIG_SCHEMA: &str = "rosen.config/v2";
pub const DIFF_SCHEMA: &str = "rosen.diff/v1";
pub const ERROR_SCHEMA: &str = "rosen.error/v1";
pub const EVENT_SCHEMA: &str = "rosen.event/v1";
pub const IMPORT_SCHEMA: &str = "rosen.import/v1";
pub const LAND_SCHEMA: &str = "rosen.land/v1";
pub const LOG_SCHEMA: &str = "rosen.log/v2";
pub const OVERLAPS_SCHEMA: &str = "rosen.overlaps/v1";
pub const RECEIPT_SCHEMA: &str = "rosen.receipt/v1";
pub const RELEASE_SCHEMA: &str = "rosen.release/v1";
pub const REVIEW_APPROVALS_SCHEMA: &str = "rosen.review.approvals/v1";
pub const REVIEW_APPROVAL_SCHEMA: &str = "rosen.review.approval/v1";
pub const REVIEW_LIST_SCHEMA: &str = "rosen.review.list/v1";
pub const REVIEW_THREAD_SCHEMA: &str = "rosen.review.thread/v1";
pub const SERVER_STATUS_SCHEMA: &str = "rosen.server.status/v1";
pub const SHARE_SCHEMA: &str = "rosen.share/v1";
pub const SNAPSHOT_SCHEMA: &str = "rosen.snapshot/v1";
pub const STATUS_SCHEMA: &str = "rosen.status/v1";
pub const WAIT_SCHEMA: &str = "rosen.wait/v1";
pub const WORKSPACE_DELETE_SCHEMA: &str = "rosen.workspace.delete/v1";
pub const WORKSPACE_LIST_SCHEMA: &str = "rosen.workspace.list/v1";

/// All failures share one envelope, including recoverable watcher failures.
/// Keeping it typed prevents success payloads and errors sharing a schema ID.
#[derive(Serialize)]
struct ErrorOutput<'a> {
    schema: &'static str,
    command: Option<&'a str>,
    error: &'a str,
}

pub fn print_error(command: Option<&str>, error: &str) {
    let body = ErrorOutput {
        schema: ERROR_SCHEMA,
        command,
        error,
    };
    println!(
        "{}",
        serde_json::to_string(&body).expect("error strings serialize")
    );
}
