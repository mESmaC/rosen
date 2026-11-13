//! `rosen import`: loads a git repository into the server (SPEC R1).

use rosen_client::{Endpoint, Error};
use serde_json::json;

use crate::server::Succeeded;

/// Schema tag for `--json` output (SPEC.md R10).
const IMPORT_SCHEMA: &str = "rosen.import/v1";

/// Starts the server if needed, imports `repo` and prints the new base.
pub async fn import(
    json: bool,
    endpoint: &Endpoint,
    repo: &str,
    branch: Option<&str>,
    mirror: Option<&str>,
) -> Result<Succeeded, Error> {
    let client = rosen_client::connect_or_start(endpoint).await?;
    let summary = client.import(repo, branch, mirror).await?;
    if json {
        let body = json!({
            "schema": IMPORT_SCHEMA,
            "branch": summary.branch,
            "base_commit": summary.base_commit,
            "objects_reachable": summary.objects_reachable,
            "objects_written": summary.objects_written,
        });
        println!("{body}");
    } else {
        println!("base: {} at {}", summary.branch, summary.base_commit);
        println!(
            "objects: {} reachable, {} written",
            summary.objects_reachable, summary.objects_written
        );
    }
    Ok(true)
}
