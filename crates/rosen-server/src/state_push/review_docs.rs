//! Review coverage of landed changes (R7) and the configuration in force
//! at the base (§2), as feed documents.

use std::sync::Arc;

use rosen_protocol::{ConfigQuery, LandedFilter, Outcome, Response};
use serde_json::{json, Value};

use super::unit::{Section, Unit};
use crate::reviews::ReviewCall;
use crate::ServerState;

/// Every landed change's reviewed state, one document each.
pub async fn coverage(state: &Arc<ServerState>) -> Result<Vec<Unit>, String> {
    let call = ReviewCall::Landed(LandedFilter { state: None });
    let rows = match crate::reviews::handle(state, call).await {
        Outcome::Ok(Response::LandedReviews(rows)) => rows,
        Outcome::Ok(other) => return Err(format!("unexpected answer {other:?}")),
        Outcome::Error(error) => return Err(error.message),
    };
    Ok(rows
        .iter()
        .map(|row| {
            let document = format!("coverage:{}", row.change);
            Unit::new(document, vec![(Section::Coverage, json!(row))])
        })
        .collect())
}

/// Tells the endpoint a landed change it was sent is no longer listed: a
/// post-land revert reopened it (R12).
pub fn coverage_removed(document: &str) -> Unit {
    let change = document.strip_prefix("coverage:").unwrap_or(document);
    let item = json!({ "change": change, "removed": true });
    Unit::with_basis(
        document.to_owned(),
        vec![(Section::Coverage, item)],
        &json!("removed"),
    )
}

/// The configuration in force at the base: commit, source, error and the
/// parsed file. Fingerprinted without the commit, so only a land that
/// changes what is in force sends it again. `None` before an import.
pub async fn config(state: &Arc<ServerState>) -> Result<Option<Unit>, String> {
    let record = match crate::project_config::show(state, ConfigQuery { at: None }).await {
        Outcome::Ok(Response::Config(record)) => record,
        Outcome::Ok(other) => return Err(format!("unexpected answer {other:?}")),
        // Refused: the project has no base yet, so nothing is in force.
        Outcome::Error(error) if error.code == rosen_protocol::ErrorCode::Refused => {
            return Ok(None)
        }
        Outcome::Error(error) => return Err(error.message),
    };
    let basis = json!({
        "source": record.source,
        "error": record.error,
        "config": record.config,
    });
    let mut item = basis.clone();
    if let Value::Object(map) = &mut item {
        map.insert("commit".into(), json!(record.commit));
        map.insert("base_ref".into(), json!(record.base_ref));
    }
    Ok(Some(Unit::with_basis(
        "config".to_owned(),
        vec![(Section::Config, item)],
        &basis,
    )))
}
