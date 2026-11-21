//! `rosen collapse check`: verify a Collapse link end to end in one command (SPEC R1, R10, #435).

use rosen_client::protocol::CollapseCheckReport;
use rosen_client::{Client, Endpoint, Error};
use serde_json::json;

use crate::output::COLLAPSE_CHECK_SCHEMA;
use crate::server::Succeeded;

/// Connects to the running daemon (never starts an empty one implicitly) and
/// runs the read-only Collapse link verification. Exits non-zero if any check
/// fails.
pub async fn check(json: bool, endpoint: &Endpoint) -> Result<Succeeded, Error> {
    let client = Client::connect(endpoint).await?;
    let report = client.collapse_check().await?;
    print_report(json, &report);
    Ok(report.ok)
}

fn print_report(json: bool, report: &CollapseCheckReport) {
    if json {
        let body = json!({
            "schema": COLLAPSE_CHECK_SCHEMA,
            "ok": report.ok,
            "mirror_remote": report.mirror_remote,
            "collapse": report.collapse,
            "ledger_endpoint": report.ledger_endpoint,
            "mirror_owner": report.mirror_owner,
            "mirror_proactive_auth": report.mirror_proactive_auth,
            "checks": report.checks,
        });
        println!("{body}");
        return;
    }
    for item in &report.checks {
        let status = item.status.as_str();
        match &item.hint {
            Some(hint) => println!(
                "{status:<7}  {:<18} {} — hint: {hint}",
                item.name, item.detail
            ),
            None => println!("{status:<7}  {:<18} {}", item.name, item.detail),
        }
    }
}
