//! Operator-only mirror history repair (SPEC R1).
use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{HistoryRewrite, Request, Response};

impl Client {
    /// Run against the authoritative server's local endpoint. The server
    /// pauses landing and pushes until the rewrite commits or is refused.
    pub async fn mirror_drop_paths(&self, paths: Vec<String>) -> Result<HistoryRewrite, Error> {
        match self.request(Request::MirrorDropPaths { paths }).await? {
            Response::MirrorRewritten(answer) => Ok(answer),
            other => Err(unexpected(other)),
        }
    }
}
