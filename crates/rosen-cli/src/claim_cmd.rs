//! `rosen claim|share|wait|release` (SPEC R6, R10).

use crate::commands::ClaimArgs;
use crate::server::Succeeded;
use rosen_client::protocol::{ClaimHolder, ClaimOutcome, ClaimRequest};
use rosen_client::{Client, Endpoint, Error};
use serde_json::json;

/// Schema tags for `--json` output (SPEC R10). Bump on any shape change.
const CLAIM_SCHEMA: &str = "rosen.claim/v1";
const SHARE_SCHEMA: &str = "rosen.share/v1";
const WAIT_SCHEMA: &str = "rosen.wait/v1";
const RELEASE_SCHEMA: &str = "rosen.release/v1";

#[derive(Clone, Copy)]
pub enum ClaimVerb {
    Claim,
    Share,
    Wait,
    Release,
}

pub async fn run(
    json: bool,
    endpoint: &Endpoint,
    verb: ClaimVerb,
    args: &ClaimArgs,
) -> Result<Succeeded, Error> {
    let request = ClaimRequest {
        change: change_of(args)?,
        paths: args.paths.clone(),
    };
    let client = rosen_client::connect_or_start(endpoint).await?;
    match verb {
        ClaimVerb::Claim => Ok(print_claim(json, &client.claim_paths(request).await?)),
        ClaimVerb::Share => share(json, &client, request).await,
        ClaimVerb::Wait => wait(json, &client, request).await,
        ClaimVerb::Release => release(json, &client, request).await,
    }
}

async fn share(json: bool, client: &Client, request: ClaimRequest) -> Result<Succeeded, Error> {
    let outcome = client.share_paths(request).await?;
    if json {
        println!("{}", json!({ "schema": SHARE_SCHEMA, "share": outcome }));
    } else {
        let paths = outcome.paths.join(" ");
        println!("shared {paths} for change {}", outcome.change);
        print_holders("claimed by", &outcome.holders);
    }
    Ok(true)
}

async fn wait(json: bool, client: &Client, request: ClaimRequest) -> Result<Succeeded, Error> {
    let paths = request.paths.join(" ");
    if !json {
        eprintln!("waiting for the claims on {paths} to be released");
    }
    let outcome = client.wait_until_free(request).await?;
    if json {
        println!("{}", json!({ "schema": WAIT_SCHEMA, "wait": outcome }));
    } else {
        println!("free: {paths}");
    }
    Ok(true)
}

async fn release(json: bool, client: &Client, request: ClaimRequest) -> Result<Succeeded, Error> {
    let outcome = client.release_paths(request).await?;
    if json {
        let body = json!({ "schema": RELEASE_SCHEMA, "release": outcome });
        println!("{body}");
        return Ok(true);
    }
    for record in &outcome.released {
        println!("released {:?} {}", record.kind, record.path);
    }
    for woken in &outcome.woken {
        let (change, owner, path) = (&woken.change, &woken.owner, &woken.path);
        println!("woke {change} ({owner}) waiting on {path}");
    }
    Ok(true)
}

/// The change comes from `--change`, then $ROSEN_CHANGE, which Velocity sets
/// for each agent thread.
fn change_of(args: &ClaimArgs) -> Result<String, Error> {
    args.change
        .clone()
        .or_else(|| std::env::var("ROSEN_CHANGE").ok().filter(|v| !v.is_empty()))
        .ok_or_else(|| {
            Error::InvalidArgument("no change: pass --change or set ROSEN_CHANGE".into())
        })
}

/// Prints the outcome; a refusal exits with status 1.
fn print_claim(json: bool, outcome: &ClaimOutcome) -> Succeeded {
    if json {
        println!("{}", json!({ "schema": CLAIM_SCHEMA, "claim": outcome }));
    }
    match outcome {
        ClaimOutcome::Granted { change, paths } => {
            if !json {
                println!("claimed {} for change {change}", paths.join(" "));
            }
            true
        }
        ClaimOutcome::Refused { holders } => {
            if !json {
                println!("refused: the first claim wins");
                print_holders("held by", holders);
            }
            false
        }
    }
}

fn print_holders(label: &str, holders: &[ClaimHolder]) {
    for holder in holders {
        let held = &holder.held;
        println!(
            "  {} {label} change {} ({}) as {}",
            holder.requested, held.change, held.owner, held.path
        );
    }
}
