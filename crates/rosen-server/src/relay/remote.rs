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

/// Collapse limits the whole serialized result POST body to this many bytes.
pub const MAX_RESULT: usize = 16 << 10;

/// Serializes the JSON body sent to `POST /rosen/actions/<id>/result`.
pub fn result_body(state: &str, result: &Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "state": state, "result": result })).unwrap_or_default()
}

/// Minimal `rosen.error/v1` object posted when an outcome exceeds Collapse's
/// body limit or is rejected with HTTP 400 or 413 (#443).
pub fn result_too_large(command: &str) -> Value {
    serde_json::json!({
        "schema": "rosen.error/v1",
        "command": command,
        "code": "result_too_large",
        "error": "action outcome exceeded Collapse's result size limit",
    })
}

fn command_of(result: &Value) -> &str {
    result
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| command.len() <= 64)
        .unwrap_or("")
}

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
    /// `change`, `head`, `commit` and `state`, or for `config_change`
    /// `base`, `expected_blob`, `text` and `message`, or for
    /// `machine_revoke` `machine`, as the kind needs.
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
        let body = result_body(state, &result);
        match send::post_json(&self.scratch, &url, &body, self.credential.as_ref()).await {
            Ok(_) => Ok(true),
            // Unknown, or never handed out (`not_sent`): no retry can land it.
            Err(SendFailure::Failed {
                status: Some(404 | 409),
                ..
            }) => Ok(false),
            // Oversized or permanently rejected body: post a minimal error so
            // one stuck outcome never blocks later actions (#443).
            Err(SendFailure::Failed {
                status: Some(400 | 413),
                ..
            }) => self.post_too_large(&url, command_of(&result)).await,
            Err(failure) => Err(self.failure(&url, failure)),
        }
    }

    async fn post_too_large(&self, url: &str, command: &str) -> Result<bool, RelayError> {
        let body = result_body("refused", &result_too_large(command));
        match send::post_json(&self.scratch, url, &body, self.credential.as_ref()).await {
            Ok(_) => Ok(true),
            Err(SendFailure::Failed {
                status: Some(400 | 404 | 409 | 413),
                ..
            }) => Ok(false),
            Err(failure) => Err(self.failure(url, failure)),
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
            SendFailure::SourceChanged { message } => RelayError::Send {
                endpoint: url.to_owned(),
                status: Some(409),
                message,
                no_credential: false,
            },
            SendFailure::Failed { status, message } => {
                let no_credential = self.root.starts_with("https://")
                    && self.credential.is_none()
                    && matches!(status, Some(401 | 403 | 404));
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
                    no_credential,
                }
            }
        }
    }
}
