//! Operator-only mirror history repair (SPEC R1).
use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_core::ObjectId;
use rosen_protocol::{HistoryRewrite, MirrorAdoption, MirrorCatchUp, Request, Response};

impl Client {
    /// Adopt a Git mirror branch tip on the authoritative server's local endpoint.
    pub async fn mirror_adopt(&self, commit: ObjectId) -> Result<MirrorAdoption, Error> {
        match self.request(Request::MirrorAdopt { commit }).await? {
            Response::MirrorAdopted(answer) => Ok(answer),
            other => Err(unexpected(other)),
        }
    }

    /// Fast-forward the server's base to its Git mirror tip, on the
    /// server's local endpoint, when another server landed there.
    pub async fn mirror_catch_up(&self) -> Result<MirrorCatchUp, Error> {
        match self.request(Request::MirrorCatchUp).await? {
            Response::MirrorCaughtUp(answer) => Ok(answer),
            other => Err(unexpected(other)),
        }
    }

    /// Run against the authoritative server's local endpoint. The server
    /// pauses landing and pushes until the rewrite commits or is refused.
    pub async fn mirror_drop_paths(&self, paths: Vec<String>) -> Result<HistoryRewrite, Error> {
        match self.request(Request::MirrorDropPaths { paths }).await? {
            Response::MirrorRewritten(answer) => Ok(answer),
            other => Err(unexpected(other)),
        }
    }
}
