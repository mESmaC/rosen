//! `rosen overlaps` (SPEC R6, R10): which live changes overlap a change or a
//! set of paths.

use crate::commands::OverlapArgs;
use crate::server::Succeeded;
use rosen_client::protocol::{OverlapAnswer, OverlapQuery};
use rosen_client::{Endpoint, Error};
use serde_json::json;

/// Schema tag for `--json` output (SPEC R10). Bump on any shape change.
const OVERLAPS_SCHEMA: &str = "rosen.overlaps/v1";

pub async fn run(json: bool, endpoint: &Endpoint, args: &OverlapArgs) -> Result<Succeeded, Error> {
    // Paths alone are a path query, so only a bare `rosen overlaps` falls
    // back to the current workspace's change.
    let change = match (&args.change, args.paths.is_empty()) {
        (Some(change), _) => Some(change.clone()),
        (None, true) => Some(crate::history_cmd::change_of(None)?),
        (None, false) => None,
    };
    let query = OverlapQuery {
        change,
        paths: args.paths.clone(),
    };
    let client = rosen_client::connect_or_start(endpoint).await?;
    let answer = client.overlaps(query).await?;
    if json {
        println!(
            "{}",
            json!({ "schema": OVERLAPS_SCHEMA, "overlaps": answer })
        );
    } else {
        print_answer(&answer);
    }
    Ok(true)
}

fn print_answer(answer: &OverlapAnswer) {
    if answer.overlaps.is_empty() {
        println!("no overlapping changes");
    }
    for overlap in &answer.overlaps {
        println!("{} ({})", overlap.change, overlap.owner);
        for path in &overlap.paths {
            println!("  {path}");
        }
    }
}
