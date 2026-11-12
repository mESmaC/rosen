//! Wire shapes for `rosen change new|list|show|abandon` and the one entry
//! point the protocol dispatcher calls.

use super::error::ChangeError;
use super::registry::{self, ChangeRecord, ListFilter, NewParent};
use rosen_core::change::{AgentId, ChangeState};
use rosen_core::object::ObjectId;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ChangeRequest {
    /// `rosen change new [--on <change>]`. Without `on`, the change starts at
    /// the current tip of the base.
    New {
        owner: AgentId,
        on: Option<String>,
    },
    List {
        owner: Option<AgentId>,
        state: Option<ChangeState>,
    },
    /// `change` is a full ID or a unique prefix.
    Show {
        change: String,
    },
    Abandon {
        change: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ChangeResponse {
    Change(ChangeRecord),
    Changes(Vec<ChangeRecord>),
}

/// The base tip a new unstacked change starts from.
#[derive(Debug, Clone, Copy)]
pub struct BaseTip {
    pub commit: ObjectId,
    pub tree: ObjectId,
}

/// Reads the base ref. The ref and the object store belong to other server
/// modules, so the registry asks through this rather than reading them itself.
pub trait BaseSource {
    fn base_tip(&self) -> Result<BaseTip, String>;
}

pub fn handle(
    conn: &mut Connection,
    base: &dyn BaseSource,
    request: ChangeRequest,
) -> Result<ChangeResponse, ChangeError> {
    let record = match request {
        ChangeRequest::New { owner, on } => {
            let parent = match on {
                Some(text) => NewParent::Change(registry::resolve(conn, &text)?),
                None => {
                    let tip = base.base_tip().map_err(ChangeError::Base)?;
                    NewParent::Base {
                        commit: tip.commit,
                        tree: tip.tree,
                    }
                }
            };
            registry::create(conn, &owner, parent)?
        }
        ChangeRequest::List { owner, state } => {
            let filter = ListFilter { owner, state };
            return Ok(ChangeResponse::Changes(registry::list(conn, &filter)?));
        }
        ChangeRequest::Show { change } => {
            let id = registry::resolve(conn, &change)?;
            registry::get(conn, id)?
        }
        ChangeRequest::Abandon { change } => {
            let id = registry::resolve(conn, &change)?;
            registry::abandon(conn, id)?
        }
    };
    Ok(ChangeResponse::Change(record))
}
