//! Per-platform post-land checkpoints and checker coverage as a Collapse state
//! feed document (R9, R12, #440).

use std::sync::Arc;

use serde_json::json;

use super::unit::{Section, Unit};
use crate::ServerState;

/// Builds the `platforms` document when the base configures platform checks;
/// returns `None` for projects with no configured platforms so their state feed
/// batches stay unchanged.
pub async fn build(state: &Arc<ServerState>) -> Result<Option<Unit>, String> {
    let Some(item) = build_item(state).await? else {
        return Ok(None);
    };
    Ok(Some(Unit::new(
        "platforms".to_owned(),
        vec![(Section::Platforms, item)],
    )))
}

/// Builds the JSON payload of the `platforms` Collapse state-feed document.
pub(crate) async fn build_item(
    state: &Arc<ServerState>,
) -> Result<Option<serde_json::Value>, String> {
    let status = crate::post_land::status::snapshot(state)
        .await
        .map_err(|error| error.message)?;
    if status.platforms.is_empty() {
        return Ok(None);
    }
    let mut waiting = Vec::new();
    let platforms: Vec<_> = status
        .platforms
        .iter()
        .map(|p| {
            let mut entry = json!({
                "platform": p.platform,
                "last_built": p.last_built,
                "last_green": p.last_green,
                "required_build": p.required_build,
                "required_test": p.required_test,
                "checker_connected": !p.checkers.is_empty(),
                "checkers": p.checkers,
            });
            if let Some(since) = p.waiting_no_checker_since {
                entry["waiting_no_checker_since"] = json!(since);
            }
            if let Some(message) = p.waiting_no_checker_message() {
                entry["waiting_message"] = json!(message);
                waiting.push(message);
            }
            entry
        })
        .collect();
    let mut item = json!({
        "base": status.base,
        "last_built": status.last_built,
        "last_green": status.last_green,
        "platforms": platforms,
    });
    if !waiting.is_empty() {
        item["waiting"] = json!(waiting);
    }
    Ok(Some(item))
}
