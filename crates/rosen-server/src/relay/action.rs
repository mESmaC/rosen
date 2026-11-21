//! One relayed action: the checks Rosen makes before running it, the run
//! itself through the same service calls the RPCs use, and the result
//! Collapse is sent.
//!
//! Nothing here adds a way around a gate. An approval goes through R7's
//! stale-head check, a land through every R8 gate, and a mark through R7's
//! coverage rules; a refusal goes back to Collapse as Rosen gave it. A
//! configuration change only opens a change, which then needs its tier's
//! approval and receipt like any other. A machine revoke only removes
//! access; joining and approving machines are never relayed.

use std::sync::Arc;

use rosen_core::{AgentId, ObjectId, ReviewedState};
use rosen_protocol::{
    ApproveRequest, ErrorBody, LandStatus, LandedFilter, MarkReviewed, Outcome, RelayKind,
    RelaySetting, Response, COLLAPSE_MACHINE,
};
use serde_json::{json, Value};

use super::config_change::{self, ConfigChange};
use super::machine_revoke::{self, MachineRevoke};
use super::remote::{result_body, result_too_large, Fetched, MAX_RESULT};
use crate::reviews::{self, ReviewCall};
use crate::ServerState;

/// What an action came to.
#[derive(Clone, Debug, PartialEq)]
pub struct Done {
    /// `done` or `refused`, as Collapse records it.
    pub state: &'static str,
    pub result: Value,
}

/// A checked action, ready to run.
#[derive(Clone, Debug)]
pub enum Call {
    Approve(ApproveRequest),
    Land(String),
    Mark(MarkReviewed, ObjectId),
    Config(ConfigChange),
    Revoke(MachineRevoke),
}

/// Refuses what must not run: a kind Rosen never relays or the operator
/// did not allow, an expired action, an account holding `@`, and missing
/// or malformed arguments. `now_ms` is Unix milliseconds.
pub fn check(action: &Fetched, setting: &RelaySetting, now_ms: i64) -> Result<Call, Done> {
    let refuse = |code: &str, message: String| refusal(&action.kind, code, &message);
    let Some(kind) = RelayKind::parse_action(&action.kind) else {
        let message = format!("Rosen does not relay {:?} actions", action.kind);
        return Err(refuse("not_allowed", message));
    };
    if !setting.active() || !setting.allow.contains(&kind) {
        let message = format!(
            "this server's operator has not allowed {} actions (rosen server relay --allow)",
            kind.as_str()
        );
        return Err(refuse("not_allowed", message));
    }
    if action.expires_at <= now_ms {
        let message = format!("action {} expired before the server fetched it", action.id);
        return Err(refuse("expired", message));
    }
    let account = AgentId::new(action.account.clone())
        .ok()
        .filter(|account| !account.as_str().contains('@'))
        .ok_or_else(|| {
            let message = format!(
                "account {:?} is not a valid nick without '@'",
                action.account
            );
            refuse("invalid_action", message)
        })?;
    arguments(kind, &account, &action.arguments)
        .map_err(|message| refuse("invalid_action", message))
}

fn arguments(
    kind: RelayKind,
    account: &AgentId,
    args: &serde_json::Map<String, Value>,
) -> Result<Call, String> {
    let text = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{} needs a {key}", kind.as_str()))
    };
    let commit = |key: &str| -> Result<ObjectId, String> {
        let value = text(key)?;
        ObjectId::from_hex(value).map_err(|_| format!("{key} {value:?} is not a commit ID"))
    };
    match kind {
        RelayKind::Config => return config_change::arguments(account, args).map(Call::Config),
        RelayKind::Revoke => return machine_revoke::arguments(account, args).map(Call::Revoke),
        _ => {}
    }
    let change = text("change")?.to_owned();
    Ok(match kind {
        RelayKind::Approve => Call::Approve(ApproveRequest {
            change,
            reviewer: account.clone(),
            head: commit("head")?,
        }),
        RelayKind::Land => Call::Land(change),
        RelayKind::Mark => {
            let state: ReviewedState = text("state")?.parse().map_err(|e| format!("{e}"))?;
            let by = account
                .on_machine(COLLAPSE_MACHINE)
                .map_err(|e| e.to_string())?;
            Call::Mark(MarkReviewed { change, state, by }, commit("commit")?)
        }
        RelayKind::Config | RelayKind::Revoke => unreachable!("read above"),
    })
}

/// Runs a checked action as an RPC from the reserved `collapse` machine.
pub async fn run(state: &Arc<ServerState>, call: Call) -> Done {
    let command = command(&call);
    let outcome = match call {
        Call::Approve(req) => {
            let machine = COLLAPSE_MACHINE.to_owned();
            reviews::handle(state, ReviewCall::Approve(req, machine)).await
        }
        Call::Land(change) => crate::landing::service::land(state, change).await,
        Call::Mark(req, commit) => reviews::handle(state, ReviewCall::MarkAt(req, commit)).await,
        Call::Config(req) => return config_change::run(state, req).await,
        Call::Revoke(req) => return machine_revoke::run(state, req).await,
    };
    match outcome {
        Outcome::Ok(Response::Land(answer)) => {
            let refused = matches!(answer.outcome, LandStatus::Refused { .. });
            let state = if refused { "refused" } else { "done" };
            Done {
                state,
                result: fit(state, json!(answer)),
            }
        }
        Outcome::Ok(Response::Approval(approval)) => done(json!(approval)),
        Outcome::Ok(Response::LandedReview(review)) => done(json!(review)),
        Outcome::Ok(other) => refusal(command, "internal", &format!("unexpected answer {other:?}")),
        Outcome::Error(ErrorBody { code, message, .. }) => {
            let code = serde_json::to_value(code).unwrap_or(Value::Null);
            refusal(command, code.as_str().unwrap_or("refused"), &message)
        }
    }
}

/// What an action a stopped server left running did, when its effect is
/// stored: the approval or mark it recorded since `started_at`. `None`
/// means it never took effect, so running it now runs it once. A land is
/// never looked up: landing a landed change answers `already_landed` (R8).
pub async fn recover(state: &Arc<ServerState>, call: &Call, started_at: i64) -> Option<Done> {
    match call {
        Call::Approve(req) => {
            let call = ReviewCall::Approvals(req.change.clone());
            let Outcome::Ok(Response::Approvals(approvals)) = reviews::handle(state, call).await
            else {
                return None;
            };
            approvals
                .into_iter()
                .find(|a| {
                    a.reviewer == req.reviewer
                        && a.machine.as_deref() == Some(COLLAPSE_MACHINE)
                        && a.head == req.head
                        && a.created_at >= started_at
                })
                .map(|approval| done(json!(approval)))
        }
        Call::Mark(req, commit) => {
            let call = ReviewCall::Landed(LandedFilter { state: None });
            let Outcome::Ok(Response::LandedReviews(landed)) = reviews::handle(state, call).await
            else {
                return None;
            };
            landed
                .into_iter()
                .find(|r| {
                    r.change.to_string() == req.change
                        && r.commit == *commit
                        && r.state == req.state
                        && r.by.as_ref() == Some(&req.by)
                        && r.updated_at.is_some_and(|at| at >= started_at)
                })
                .map(|review| done(json!(review)))
        }
        Call::Land(_) => None,
        Call::Config(req) => config_change::recover(state, req, started_at).await,
        Call::Revoke(req) => machine_revoke::recover(state, req).await,
    }
}

fn command(call: &Call) -> &'static str {
    match call {
        Call::Approve(_) => "approve",
        Call::Land(_) => "land",
        Call::Mark(..) => "mark",
        Call::Config(_) => "config_change",
        Call::Revoke(_) => "machine_revoke",
    }
}

pub fn done(result: Value) -> Done {
    Done {
        state: "done",
        result: fit("done", result),
    }
}

/// A refusal shaped like the CLI's `rosen.error/v1` object, with the code
/// Collapse can branch on.
pub fn refusal(command: &str, code: &str, message: &str) -> Done {
    let result = json!({
        "schema": "rosen.error/v1",
        "command": command,
        "code": code,
        "error": message,
    });
    Done {
        state: "refused",
        result: fit("refused", result),
    }
}

const MAX_COMMAND: usize = 128;

fn body_fits(state: &str, result: &Value) -> bool {
    result_body(state, result).len() <= MAX_RESULT
}

/// Keeps the whole serialized `{"state","result"}` POST body within Collapse's
/// limit (#443): land answers drop lists, logs and maps before trimming their
/// message; refusals shorten `error` (and oversized `command`).
fn fit(state: &str, mut result: Value) -> Value {
    if body_fits(state, &result) {
        return result;
    }
    if fit_outcome(state, &mut result) || fit_fields(state, &mut result) {
        return result;
    }
    if result.get("schema").and_then(Value::as_str) == Some("rosen.error/v1") {
        let command = result
            .get("command")
            .and_then(Value::as_str)
            .filter(|command| command.len() <= MAX_COMMAND)
            .unwrap_or("");
        return result_too_large(command);
    }
    json!({ "truncated": true })
}

fn fit_outcome(state: &str, result: &mut Value) -> bool {
    let Some(outcome) = result.get_mut("outcome").and_then(Value::as_object_mut) else {
        return false;
    };
    outcome.retain(|_, value| !value.is_array());
    outcome.insert("truncated".into(), Value::Bool(true));
    if body_fits(state, result) {
        return true;
    }
    if let Some(outcome) = result.get_mut("outcome").and_then(Value::as_object_mut) {
        outcome.remove("log");
        outcome.retain(|_, value| !value.is_object());
    }
    if body_fits(state, result) {
        return true;
    }
    shorten_nested(state, result, "outcome", "message");
    body_fits(state, result)
}

fn fit_fields(state: &str, result: &mut Value) -> bool {
    if let Some(command) = result
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| command.len() > MAX_COMMAND)
        .map(str::to_owned)
    {
        let short = shorten_text(&command, |candidate| candidate.len() <= MAX_COMMAND);
        if let Some(map) = result.as_object_mut() {
            map.insert("command".into(), Value::String(short));
        }
        if body_fits(state, result) {
            return true;
        }
    }
    for key in ["error", "message"] {
        shorten_top(state, result, key);
        if body_fits(state, result) {
            return true;
        }
    }
    false
}

fn shorten_top(state: &str, result: &mut Value, key: &str) {
    let Some(original) = result.get(key).and_then(Value::as_str).map(str::to_owned) else {
        return;
    };
    let short = shorten_text(&original, |candidate| {
        if let Some(map) = result.as_object_mut() {
            map.insert(key.into(), Value::String(candidate.to_owned()));
        }
        body_fits(state, result)
    });
    if let Some(map) = result.as_object_mut() {
        map.insert(key.into(), Value::String(short));
    }
}

fn shorten_nested(state: &str, result: &mut Value, parent: &str, key: &str) {
    let Some(original) = result
        .get(parent)
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let short = shorten_text(&original, |candidate| {
        if let Some(map) = result.get_mut(parent).and_then(Value::as_object_mut) {
            map.insert(key.into(), Value::String(candidate.to_owned()));
        }
        body_fits(state, result)
    });
    if let Some(map) = result.get_mut(parent).and_then(Value::as_object_mut) {
        map.insert(key.into(), Value::String(short));
    }
}

/// Longest UTF-8 prefix of `text`, suffixed with `"..."`, accepted by `fits`.
fn shorten_text(text: &str, mut fits: impl FnMut(&str) -> bool) -> String {
    const ELLIPSIS: &str = "...";
    if !fits(ELLIPSIS) {
        return String::new();
    }
    let mut end = text.len().min(MAX_RESULT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = &text[..end];
    let cuts: Vec<usize> = prefix
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(prefix.len()))
        .collect();
    let mut lo = 0usize;
    let mut hi = cuts.len() - 1;
    let mut candidate = String::new();
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        candidate.clear();
        candidate.push_str(&prefix[..cuts[mid]]);
        candidate.push_str(ELLIPSIS);
        if fits(&candidate) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    candidate.clear();
    candidate.push_str(&prefix[..cuts[lo]]);
    candidate.push_str(ELLIPSIS);
    candidate
}
