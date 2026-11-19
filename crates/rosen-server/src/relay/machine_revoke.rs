//! The relayed `machine_revoke` action (#409): the owner revokes a lost or
//! stolen machine from Collapse, the one place a phone can reach.
//!
//! It runs the function behind `rosen machine revoke`, so the machine's
//! credential is refused on its next request and its changes, claims and
//! workspaces are kept (R14). It only removes access, so a misuse costs a
//! re-enrollment, never data. The server's own machine and `collapse` are
//! refused: revoking `collapse` from the web would leave no way to turn
//! the relay back on from the web.

use std::path::Path;
use std::sync::Arc;

use rosen_core::AgentId;
use rosen_protocol::machine::{MachineRecord, MachineStatus};
use rosen_protocol::{ErrorBody, ErrorCode, COLLAPSE_MACHINE};
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use super::action::{done, refusal, Done};
use crate::machines::signing::SERVER_MACHINE;
use crate::ServerState;

const COMMAND: &str = "machine_revoke";

/// A checked `machine_revoke` action.
#[derive(Clone, Debug)]
pub struct MachineRevoke {
    pub machine: String,
    /// `<account>@collapse`, recorded with the outcome.
    pub by: AgentId,
}

/// Reads the action's one argument, `machine`.
pub fn arguments(
    account: &AgentId,
    args: &serde_json::Map<String, Value>,
) -> Result<MachineRevoke, String> {
    let machine = args
        .get("machine")
        .and_then(Value::as_str)
        .filter(|machine| !machine.is_empty())
        .ok_or_else(|| format!("{COMMAND} needs a machine"))?;
    let by = account
        .on_machine(COLLAPSE_MACHINE)
        .map_err(|e| e.to_string())?;
    Ok(MachineRevoke {
        machine: machine.to_owned(),
        by,
    })
}

/// Refuses a reserved, unknown or already revoked machine, changing
/// nothing; otherwise revokes it.
pub async fn run(state: &Arc<ServerState>, req: MachineRevoke) -> Done {
    if let Some(message) = reserved(&req.machine) {
        return refusal(COMMAND, "refused", message);
    }
    let (dir, machine) = (state.config.data_dir.clone(), req.machine.clone());
    match tokio::task::spawn_blocking(move || revoke_active(&dir, &machine)).await {
        Ok(Ok(record)) => answer(record, &req),
        Ok(Err(ErrorBody { code, message, .. })) => {
            let code = serde_json::to_value(code).unwrap_or(Value::Null);
            refusal(COMMAND, code.as_str().unwrap_or("refused"), &message)
        }
        Err(join) => refusal(COMMAND, "internal", &join.to_string()),
    }
}

fn reserved(machine: &str) -> Option<&'static str> {
    match machine {
        SERVER_MACHINE => Some("the server's own machine cannot be revoked"),
        COLLAPSE_MACHINE => Some(
            "revoking collapse through the relay would leave no way to turn it back on \
             from the web; run `rosen machine revoke collapse` on the server",
        ),
        _ => None,
    }
}

/// Checks the machine and revokes it in one immediate transaction, so a
/// revoke racing this one cannot make both answer that they revoked it.
fn revoke_active(data_dir: &Path, machine: &str) -> Result<MachineRecord, ErrorBody> {
    let internal = |error: rusqlite::Error| ErrorBody::new(ErrorCode::Internal, error.to_string());
    let mut conn = crate::machines::open(data_dir).map_err(internal)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    // As `machine list` does: an expired pending request is gone.
    crate::machines::pending::expire(&tx, crate::machines::now()).map_err(internal)?;
    let status: Option<String> = tx
        .query_row(
            "SELECT status FROM machines WHERE id = ?1",
            [machine],
            |row| row.get(0),
        )
        .optional()
        .map_err(internal)?;
    match status.as_deref() {
        None => {
            let message = format!("no machine {machine:?}");
            return Err(ErrorBody::new(ErrorCode::NotFound, message));
        }
        Some("revoked") => {
            let message = format!("machine {machine:?} is already revoked");
            return Err(ErrorBody::new(ErrorCode::Refused, message));
        }
        Some(_) => {}
    }
    let record = crate::machines::manage::revoke(&tx, machine)?;
    tx.commit().map_err(internal)?;
    Ok(record)
}

/// The revoked machine as `machine revoke` prints it, with who revoked it.
fn answer(record: MachineRecord, req: &MachineRevoke) -> Done {
    let mut result = json!(record);
    if let Some(fields) = result.as_object_mut() {
        fields.insert("by".to_owned(), Value::String(req.by.to_string()));
    }
    done(result)
}

/// An action a stopped server left running: done if the machine is revoked
/// now. The relay runs one action at a time, so a revoked machine means
/// this action took effect, or another revoke already did what it asked.
pub async fn recover(state: &Arc<ServerState>, req: &MachineRevoke) -> Option<Done> {
    let (dir, machine) = (state.config.data_dir.clone(), req.machine.clone());
    let record = tokio::task::spawn_blocking(move || {
        let conn = crate::machines::open(&dir).ok()?;
        let listed = crate::machines::manage::list(&conn).ok()?;
        listed.into_iter().find(|record| record.id == machine)
    })
    .await
    .ok()??;
    (record.status == MachineStatus::Revoked).then(|| answer(record, req))
}

#[cfg(test)]
mod tests {
    use rosen_protocol::{Endpoint, RelayKind, RelaySetting};

    use super::super::action::{check, run as run_action};
    use super::super::remote::Fetched;
    use super::super::store::RelayStore;
    use super::*;

    fn fetched(kind: &str, machine: &str) -> Fetched {
        let arguments = json!({ "machine": machine });
        Fetched {
            id: 1,
            kind: kind.to_owned(),
            account: "owner".to_owned(),
            expires_at: i64::MAX,
            arguments: arguments.as_object().unwrap().clone(),
        }
    }

    async fn revoke(state: &Arc<ServerState>, setting: &RelaySetting, machine: &str) -> Done {
        let call = check(&fetched(COMMAND, machine), setting, 0).unwrap();
        run_action(state, call).await
    }

    // Risk (R14, #409): a revoke the operator never allowed, a relayed
    // join or approval, or a revoke of `server` or `collapse` would let a
    // web session cut off the server or leave the relay unrecoverable.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_relayed_revoke_needs_its_kind_and_spares_reserved_machines() {
        let dir = tempfile::tempdir().unwrap();
        let state = ServerState::new(crate::ServerConfig {
            data_dir: dir.path().into(),
            endpoint: Endpoint::new("relay-revoke-contract"),
            warm_pool_dir: None,
        });
        let insert =
            "INSERT INTO machines (id, name, role, status, last_contact, credential_hash, \
                      expires_at) VALUES ('lost', 'laptop', 'agent', 'active', 0, 'x', 0)";
        let conn = crate::machines::open(dir.path()).unwrap();
        conn.execute(insert, []).unwrap();
        let mut store = RelayStore::open(dir.path()).unwrap();
        let others = [RelayKind::Approve, RelayKind::Land, RelayKind::Mark];
        let others = store.set(&others, 0).unwrap();
        let refused = check(&fetched(COMMAND, "lost"), &others, 0).unwrap_err();
        assert_eq!(refused.result["code"], "not_allowed");

        let all = store.set(&RelayKind::ALL, 0).unwrap();
        for kind in ["machine_approve", "join_key_create", "machine_key_create"] {
            let refused = check(&fetched(kind, "lost"), &all, 0).unwrap_err();
            assert_eq!(refused.result["code"], "not_allowed");
        }
        let code = |done: &Done| done.result["code"].as_str().unwrap_or("").to_owned();
        assert_eq!(code(&revoke(&state, &all, SERVER_MACHINE).await), "refused");
        assert_eq!(
            code(&revoke(&state, &all, COLLAPSE_MACHINE).await),
            "refused"
        );
        assert!(store.setting().unwrap().active(), "collapse stays enrolled");
        assert_eq!(code(&revoke(&state, &all, "ghost").await), "not_found");

        let done = revoke(&state, &all, "lost").await;
        assert_eq!(done.state, "done", "{done:?}");
        assert_eq!(done.result["by"], "owner@collapse");
        let status: String = conn
            .query_row("SELECT status FROM machines WHERE id = 'lost'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "revoked");
        assert_eq!(code(&revoke(&state, &all, "lost").await), "refused");
    }
}
