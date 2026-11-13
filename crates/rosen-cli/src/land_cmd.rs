//! `rosen land` (SPEC R8 step 2, R10).

use crate::server::Succeeded;
use rosen_client::protocol::{LandAnswer, LandStatus};
use rosen_client::{Endpoint, Error};
use serde_json::json;

use crate::output::LAND_SCHEMA;

/// Exits unsuccessfully when the land is refused, so scripts can branch on
/// the exit code alone.
pub async fn run(json: bool, endpoint: &Endpoint, change: &str) -> Result<Succeeded, Error> {
    let answer = rosen_client::land_reconnecting(endpoint, change).await?;
    print_answer(json, &answer);
    Ok(!matches!(answer.outcome, LandStatus::Refused { .. }))
}

fn print_answer(json: bool, answer: &LandAnswer) {
    if json {
        println!("{}", json!({ "schema": LAND_SCHEMA, "land": answer }));
        return;
    }
    let base_ref = &answer.base_ref;
    match &answer.outcome {
        LandStatus::Landed { commit, attempts } => {
            println!("landed change {} as {commit} on {base_ref}", answer.change);
            if *attempts > 1 {
                println!("({attempts} attempts: other lands moved {base_ref} first)");
            }
        }
        LandStatus::AlreadyLanded { commit } => {
            println!("change {} already landed as {commit}", answer.change);
        }
        LandStatus::Refused {
            reason, message, ..
        } => {
            eprintln!("land refused ({reason}): {message}");
            eprintln!("{base_ref} is at {}", answer.base);
        }
    }
}
