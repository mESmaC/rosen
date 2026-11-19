//! The three Collapse routes the relay calls, all under the project's
//! `collapse` root and authenticated with the push token git's credential
//! helper holds, as the state feed is:
//!
//! - `GET <collapse>/rosen/doorbell`, answered from Cloudflare's edge as
//!   `{"generation":N}` without waking Collapse's Machine;
//! - `GET <collapse>/rosen/actions?after=<id>`, `{"actions":[...]}`;
//! - `POST <collapse>/rosen/actions/<id>/result`, `{"state","result"}`.

use std::path::PathBuf;

use serde_json::Value;

use super::RelayError;
use crate::ledger_push::send::{self, Accepted, Credential, SendFailure};

/// Collapse refuses a result larger than this.
pub const MAX_RESULT: usize = 16 << 10;

/// One endpoint and what reaching it needs.
pub struct Remote {
    /// The project's `collapse` root.
    pub root: String,
    pub credential: Option<Credential>,
    pub scratch: PathBuf,
}

/// One queued owner request, as Collapse hands it out.
#[derive(Clone, Debug)]
pub struct Fetched {
    pub id: i64,
    pub kind: String,
    /// The Collapse account that asked.
    pub account: String,
    /// Unix milliseconds after which it must not run.
    pub expires_at: i64,
    /// `change`, `head`, `commit` and `state`, as the kind needs.
    pub arguments: serde_json::Map<String, Value>,
}

impl Fetched {
    /// Reads one entry of the action list; `None` when a field is missing.
    pub fn parse(raw: &Value) -> Option<Self> {
        let text = |key: &str| raw.get(key)?.as_str().map(str::to_owned);
        Some(Self {
            id: raw.get("id")?.as_i64()?,
            kind: text("kind")?,
            account: text("account")?,
            expires_at: raw.get("expires_at")?.as_i64()?,
            arguments: match raw.get("arguments") {
                None | Some(Value::Null) => serde_json::Map::new(),
                Some(arguments) => arguments.as_object()?.clone(),
            },
        })
    }
}

/// The field `key` of a JSON answer `body`.
fn field(body: &str, key: &str) -> Option<Value> {
    let mut answer: Value = serde_json::from_str(body).ok()?;
    answer.get_mut(key).map(Value::take)
}

impl Remote {
    /// The doorbell's generation, which grows each time an action is queued.
    pub async fn doorbell(&self) -> Result<i64, RelayError> {
        let url = self.url("rosen/doorbell");
        let accepted = self.get(&url).await?;
        field(&accepted.body, "generation")
            .and_then(|generation| generation.as_i64())
            .ok_or_else(|| self.failure(&url, accepted.unreadable("unreadable doorbell")))
    }

    /// Queued actions after `after`, oldest first, each still as JSON so
    /// one Collapse sends malformed is refused alone.
    pub async fn actions(&self, after: i64) -> Result<Vec<Value>, RelayError> {
        let url = self.url(&format!("rosen/actions?after={after}"));
        let accepted = self.get(&url).await?;
        match field(&accepted.body, "actions") {
            Some(Value::Array(actions)) => Ok(actions),
            _ => Err(self.failure(&url, accepted.unreadable("unreadable action list"))),
        }
    }

    /// Posts an outcome. Collapse keeps the first result for an action and
    /// answers a repeat with what it holds, so posting again is safe.
    /// True once Collapse holds a result; false when it never will, as for
    /// an action it no longer has.
    pub async fn post_result(
        &self,
        id: i64,
        state: &str,
        result: &str,
    ) -> Result<bool, RelayError> {
        let url = self.url(&format!("rosen/actions/{id}/result"));
        let result: Value = serde_json::from_str(result).unwrap_or(Value::Null);
        let body = serde_json::json!({ "state": state, "result": result });
        let body = serde_json::to_vec(&body).unwrap_or_default();
        match send::post_json(&self.scratch, &url, &body, self.credential.as_ref()).await {
            Ok(_) => Ok(true),
            // Unknown, or never handed out (`not_sent`): no retry can land it.
            Err(SendFailure::Failed {
                status: Some(404 | 409),
                ..
            }) => Ok(false),
            Err(failure) => Err(self.failure(&url, failure)),
        }
    }

    async fn get(&self, url: &str) -> Result<Accepted, RelayError> {
        send::get_json(url, self.credential.as_ref())
            .await
            .map_err(|failure| self.failure(url, failure))
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.root.trim_end_matches('/'))
    }

    fn failure(&self, url: &str, failure: SendFailure) -> RelayError {
        match failure {
            SendFailure::Gone => RelayError::Gone {
                endpoint: self.root.clone(),
            },
            SendFailure::Failed { status, message } => {
                let message = match (&self.credential, status) {
                    (None, Some(401 | 403 | 404)) => format!(
                        "{message} (git's credential helper holds no token for {})",
                        self.root
                    ),
                    _ => message,
                };
                RelayError::Send {
                    endpoint: url.to_owned(),
                    status,
                    message,
                }
            }
        }
    }
}
