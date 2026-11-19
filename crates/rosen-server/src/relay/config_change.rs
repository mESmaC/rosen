//! The relayed `config_change` action (#405): Collapse's settings page
//! saves an edited `rosen.toml` as an ordinary change owned by
//! `<account>@collapse`.
//!
//! It only opens the change. The change then gets its tier, approval and
//! receipt under R7 and R8 like any other, and since the server reads the
//! configuration from the base, it cannot lower its own gates.

use std::sync::Arc;

use rosen_core::{AgentId, ChangeId, ObjectId, ProjectConfig, Snapshot, SnapshotKind, CONFIG_PATH};
use rosen_protocol::{
    ChangeFilter, ChangeRecord, ErrorBody, Outcome, Response, SnapshotRequest, WorkspaceBytes,
    WorkspaceWrite, COLLAPSE_MACHINE,
};
use rosen_store::{LooseObjectStore, ObjectStore};
use serde_json::{json, Value};

use super::action::{done, refusal, Done};
use crate::changes::BaseTip;
use crate::ServerState;

/// The largest `rosen.toml` the action may carry.
pub const MAX_TEXT: usize = 256 << 10;

/// The longest title the change's named snapshot may carry.
const MAX_MESSAGE: usize = 4 << 10;

const COMMAND: &str = "config_change";

/// A checked `config_change` action.
#[derive(Clone, Debug)]
pub struct ConfigChange {
    pub owner: AgentId,
    /// The commit whose file the owner edited, for the record.
    pub base: ObjectId,
    /// That file's blob, or `None` when the base had none.
    pub expected_blob: Option<ObjectId>,
    pub text: String,
    pub message: String,
}

/// Reads the action's arguments: `base`, `expected_blob` (null when the
/// file was absent), `text` and `message`.
pub fn arguments(
    owner: &AgentId,
    args: &serde_json::Map<String, Value>,
) -> Result<ConfigChange, String> {
    let string = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{COMMAND} needs a {key}"))
    };
    let object = |key: &str, value: &str| {
        ObjectId::from_hex(value).map_err(|_| format!("{key} {value:?} is not an object ID"))
    };
    let base = object("base", string("base")?)?;
    let expected_blob = match args.get("expected_blob") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(object("expected_blob", value)?),
        Some(_) => return Err("expected_blob must be a blob ID or null".to_owned()),
    };
    let text = string("text")?;
    if text.len() > MAX_TEXT {
        return Err(format!("text is over {} KiB", MAX_TEXT >> 10));
    }
    let message = string("message")?.trim();
    if message.is_empty() || message.len() > MAX_MESSAGE {
        let limit = MAX_MESSAGE >> 10;
        return Err(format!("message must be 1 byte to {limit} KiB of text"));
    }
    Ok(ConfigChange {
        owner: owner.clone(),
        base,
        expected_blob,
        text: text.to_owned(),
        message: message.to_owned(),
    })
}

/// Refuses a file the server would not accept or an edit of a superseded
/// file, creating nothing; otherwise opens the change.
pub async fn run(state: &Arc<ServerState>, req: ConfigChange) -> Done {
    if let Err(error) = ProjectConfig::parse(&req.text) {
        return refusal(COMMAND, "invalid_config", &error.to_string());
    }
    let opened = match open(state, &req).await {
        Ok(record) => record,
        Err(refused) => return refused,
    };
    finish(state, opened.change.id, &req).await
}

/// Checks the base's file against the one the owner edited and creates the
/// change at the very tip that was checked.
async fn open(state: &Arc<ServerState>, req: &ConfigChange) -> Result<ChangeRecord, Done> {
    let reading = Arc::clone(state);
    let (owner, expected) = (req.owner.clone(), req.expected_blob);
    let proposed = ObjectId::hash_object("blob", req.text.as_bytes());
    let created = tokio::task::spawn_blocking(move || {
        let tip = reading.changes.base_tip().map_err(|e| e.to_string())?;
        let current = base_blob(&reading, &tip)?;
        if current != expected {
            return Ok(Err((
                "stale_config",
                "rosen.toml changed on the base; reload",
            )));
        }
        if current == Some(proposed) {
            return Ok(Err((
                "unchanged",
                "the text is the base's rosen.toml already",
            )));
        }
        let created = reading.changes.create_at(&owner, COLLAPSE_MACHINE, tip);
        created.map(Ok).map_err(|e| e.to_string())
    })
    .await
    .map_err(|join| join.to_string());
    match created.and_then(|inner| inner) {
        Ok(Ok(record)) => Ok(record),
        Ok(Err((code, message))) => Err(refusal(COMMAND, code, message)),
        Err(message) => Err(refusal(COMMAND, "internal", &message)),
    }
}

/// The blob `rosen.toml` names in the tip's tree, if the file is there.
fn base_blob(state: &ServerState, tip: &BaseTip) -> Result<Option<ObjectId>, String> {
    let store = LooseObjectStore::open(state.config.objects_dir()).map_err(|e| e.to_string())?;
    let tree = store.read_tree(&tip.tree).map_err(|e| e.to_string())?;
    Ok(tree.get(CONFIG_PATH.as_bytes()).map(|entry| entry.id))
}

/// Writes the file through a server-side workspace and names the result.
/// Each step is a no-op when already done, so a resumed action ends with
/// the same one change.
async fn finish(state: &Arc<ServerState>, change: ChangeId, req: &ConfigChange) -> Done {
    let write = WorkspaceWrite {
        change: change.to_string(),
        path: CONFIG_PATH.to_owned(),
        data: Some(WorkspaceBytes(req.text.clone().into_bytes())),
    };
    let written = match crate::workspaces::write(state, write).await {
        Outcome::Ok(Response::Snapshot(outcome)) => outcome,
        other => return failed(other),
    };
    let record = written.change.change;
    if let Some(named) = named_as(&record.snapshots, &req.message) {
        return answer(change, record.head, named, req);
    }
    let name = SnapshotRequest {
        change: change.to_string(),
        tree: record.head,
        from: Some(record.head),
        message: Some(req.message.clone()),
    };
    match crate::snapshots::snapshot(state, name).await {
        Outcome::Ok(Response::Snapshot(outcome)) => match outcome.commit {
            Some(commit) => answer(change, outcome.change.change.head, commit, req),
            None => refusal(COMMAND, "internal", "the named snapshot recorded no commit"),
        },
        other => failed(other),
    }
}

/// The commit of a snapshot already named `message`.
fn named_as(snapshots: &[Snapshot], message: &str) -> Option<ObjectId> {
    snapshots
        .iter()
        .rev()
        .find_map(|snapshot| match &snapshot.kind {
            SnapshotKind::Named(named) if named.trim_end() == message => Some(snapshot.commit),
            _ => None,
        })
}

fn answer(change: ChangeId, head: ObjectId, commit: ObjectId, req: &ConfigChange) -> Done {
    done(json!({
        "change": change,
        "head": head,
        "commit": commit,
        "owner": format!("{}@{COLLAPSE_MACHINE}", req.owner),
        "message": req.message,
    }))
}

fn failed(outcome: Outcome) -> Done {
    match outcome {
        Outcome::Error(ErrorBody { code, message, .. }) => {
            let code = serde_json::to_value(code).unwrap_or(Value::Null);
            refusal(COMMAND, code.as_str().unwrap_or("refused"), &message)
        }
        Outcome::Ok(other) => refusal(COMMAND, "internal", &format!("unexpected answer {other:?}")),
    }
}

/// An action a stopped server left running: the change it created, if it
/// got that far, carried to the end. The relay runs one action at a time
/// and nothing else records changes on the `collapse` machine, so the
/// owner's change there since `started_at` is this action's.
pub async fn recover(
    state: &Arc<ServerState>,
    req: &ConfigChange,
    started_at: i64,
) -> Option<Done> {
    let reading = Arc::clone(state);
    let filter = ChangeFilter {
        owner: Some(req.owner.clone()),
        state: None,
    };
    let listed = tokio::task::spawn_blocking(move || reading.changes.list(&filter))
        .await
        .ok()?
        .ok()?;
    let created = listed.into_iter().find(|record| {
        record.change.owner_machine.as_deref() == Some(COLLAPSE_MACHINE)
            && record.created_at >= started_at
            && !record.change.state.is_closed()
    })?;
    Some(finish(state, created.change.id, req).await)
}
