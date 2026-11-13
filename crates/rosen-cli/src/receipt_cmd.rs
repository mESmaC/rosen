//! `rosen receipt submit` (SPEC R8 step 1, R10).

use crate::commands::ReceiptCommand;
use crate::server::Succeeded;
use rosen_client::protocol::{ReceiptRecord, SubmitReceipt};
use rosen_client::{Endpoint, Error};
use rosen_core::{AgentId, ObjectId};
use serde_json::json;

/// Schema tag for `--json` output (SPEC R10). Bump on any shape change.
const RECEIPT_SCHEMA: &str = "rosen.receipt/v1";

pub async fn run(
    json: bool,
    endpoint: &Endpoint,
    command: &ReceiptCommand,
) -> Result<Succeeded, Error> {
    let ReceiptCommand::Submit {
        change,
        head,
        base,
        check_set,
        submitter,
    } = command;
    // Parse arguments before starting a daemon, so a typo costs nothing.
    let head = object_id("--head", head)?;
    let base = object_id("--base", base)?;
    let submitter = resolve_submitter(submitter.as_deref())?;
    let client = rosen_client::connect_or_start(endpoint).await?;
    let check_set = match check_set {
        Some(name) => name.clone(),
        None => {
            client
                .project_config(None)
                .await?
                .config
                .checks
                .pre_land
                .name
        }
    };
    let request = SubmitReceipt {
        change: change.clone(),
        head,
        base,
        check_set,
        submitter,
    };
    print_receipt(json, &client.submit_receipt(request).await?);
    Ok(true)
}

fn object_id(flag: &str, hex: &str) -> Result<ObjectId, Error> {
    ObjectId::from_hex(hex).map_err(|err| Error::InvalidArgument(format!("{flag} {hex:?}: {err}")))
}

/// The submitter is the checker's identity. Velocity sets ROSEN_AGENT per
/// agent; a human at a terminal falls back to their OS user name.
fn resolve_submitter(explicit: Option<&str>) -> Result<AgentId, Error> {
    let from_env = || {
        ["ROSEN_AGENT", "USERNAME", "USER"]
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
    };
    let name = explicit
        .map(str::to_owned)
        .or_else(from_env)
        .ok_or_else(|| {
            Error::InvalidArgument("no submitter: pass --submitter or set ROSEN_AGENT".into())
        })?;
    AgentId::new(name).map_err(|err| Error::InvalidArgument(err.to_string()))
}

fn print_receipt(json: bool, record: &ReceiptRecord) {
    if json {
        println!("{}", json!({ "schema": RECEIPT_SCHEMA, "receipt": record }));
        return;
    }
    let receipt = &record.receipt;
    println!("receipt stored for change {}", receipt.change);
    println!("check set: {}", receipt.check_set);
    println!("head:      {}", receipt.head);
    println!("base:      {} ({})", receipt.base, record.base_ref);
    println!("submitter: {}", receipt.submitter);
    if !record.base_current {
        println!(
            "note: {} has moved past this base; at land the server decides whether \
             the receipt still counts or a new one is needed",
            record.base_ref
        );
    }
}
