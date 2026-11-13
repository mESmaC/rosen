//! Typed calls for history queries (SPEC R11). The server answers every one
//! from its object store, so none needs a workspace.
//!
//! A revision is `base` (or `None`), a 40-hex commit or tree ID, or a
//! change ID or unique prefix of one.

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{
    Blame, BlameRequest, Diff, DiffRequest, Log, LogRequest, MergeBaseAnswer, MergeResultAnswer,
    ReadAnswer, ReadRequest, Request, Response,
};

impl Client {
    /// The log at a revision, newest first, following first parents. With
    /// paths, only commits that changed one of them (path history).
    pub async fn log(&self, request: LogRequest) -> Result<Log, Error> {
        match self.request(Request::Log(request)).await? {
            Response::Log(log) => Ok(log),
            other => Err(unexpected(other)),
        }
    }

    /// Which commit, or which unlanded change, last changed each line.
    pub async fn blame(&self, request: BlameRequest) -> Result<Blame, Error> {
        match self.request(Request::Blame(request)).await? {
            Response::Blame(blame) => Ok(blame),
            other => Err(unexpected(other)),
        }
    }

    /// A line diff; with no `from`, against the new side's parent.
    pub async fn diff(&self, request: DiffRequest) -> Result<Diff, Error> {
        match self.request(Request::Diff(request)).await? {
            Response::Diff(diff) => Ok(diff),
            other => Err(unexpected(other)),
        }
    }

    pub async fn merge_base(&self, a: &str, b: &str) -> Result<MergeBaseAnswer, Error> {
        let request = Request::MergeBase {
            a: a.to_owned(),
            b: b.to_owned(),
        };
        match self.request(request).await? {
            Response::MergeBase(answer) => Ok(answer),
            other => Err(unexpected(other)),
        }
    }

    /// A file's bytes or a directory's listing at a revision.
    pub async fn read_file(&self, request: ReadRequest) -> Result<ReadAnswer, Error> {
        match self.request(Request::ReadFile(request)).await? {
            Response::File(answer) => Ok(answer),
            other => Err(unexpected(other)),
        }
    }

    /// Merges a change onto the current base on the server and stores the
    /// result: the tree a checker builds for a receipt.
    pub async fn merge_result(&self, change: &str) -> Result<MergeResultAnswer, Error> {
        let request = Request::MergeResult {
            change: change.to_owned(),
        };
        match self.request(request).await? {
            Response::MergeResult(answer) => Ok(answer),
            other => Err(unexpected(other)),
        }
    }
}
