//! Typed calls for history queries (SPEC R11). The server answers every one
//! from its object store, so none needs a workspace.
//!
//! A revision is `base` (or `None`), a 40-hex commit or tree ID, or a
//! change ID or unique prefix of one.

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_core::ObjectId;
use rosen_protocol::{
    Blame, BlameRequest, Diff, DiffRequest, LargeFile, Log, LogRequest, MergeBaseAnswer,
    MergeResultAnswer, ObjectChunkQuery, ReadAnswer, ReadRequest, Request, Response, WireKind,
    OBJECT_CHUNK_BYTES,
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

    /// A file's bytes or a directory's listing at a revision. A file too
    /// large for one frame is read in slices.
    pub async fn read_file(&self, request: ReadRequest) -> Result<ReadAnswer, Error> {
        match self.request(Request::ReadFile(request)).await? {
            Response::File(answer) => Ok(answer),
            Response::LargeFile(large) => self.read_large_file(large).await,
            other => Err(unexpected(other)),
        }
    }

    /// Reads a file too large for one frame slice by slice (#243), and
    /// checks the bytes against the file's ID before answering with them.
    async fn read_large_file(&self, large: LargeFile) -> Result<ReadAnswer, Error> {
        let bad = |why: &str| Error::Unexpected(format!("blob {}: {why}", large.id));
        let total = usize::try_from(large.len).map_err(|_| bad("too large to hold"))?;
        let mut data = Vec::with_capacity(total);
        while data.len() < total {
            let query = ObjectChunkQuery {
                id: large.id,
                offset: data.len() as u64,
                len: OBJECT_CHUNK_BYTES as u64,
            };
            let chunk = match self.request(Request::GetObjectChunk(query)).await? {
                Response::ObjectChunk(chunk) => chunk,
                other => return Err(unexpected(other)),
            };
            if chunk.kind != WireKind::Blob || chunk.total != large.len || chunk.data.is_empty() {
                return Err(bad("the server sent a slice of another object"));
            }
            data.extend_from_slice(&chunk.data);
        }
        if data.len() != total || ObjectId::hash_object("blob", &data) != large.id {
            return Err(bad("the slices the server sent do not hash to it"));
        }
        Ok(ReadAnswer::File {
            mode: large.mode,
            id: large.id,
            data,
        })
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
