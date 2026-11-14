//! Checkout-free workspaces (SPEC R3). Reads always follow the current head,
//! including server rebases. Writes return only after their snapshot is stored.
use crate::{connection::unexpected, Client, Error};
use rosen_protocol::{
    Diff, DiffRequest, ReadAnswer, ReadRequest, Request, Response, SnapshotOutcome,
    WorkspaceBlobWrite, WorkspaceBytes, WorkspaceWrite, OBJECT_CHUNK_BYTES,
};

impl Client {
    /// Reads a file, or lists a directory (empty path lists the root).
    pub async fn workspace_read(&self, change: &str, path: &str) -> Result<ReadAnswer, Error> {
        self.read_file(ReadRequest {
            at: Some(change.to_owned()),
            path: path.to_owned(),
        })
        .await
    }

    /// Diffs the current workspace against its parent without a checkout.
    pub async fn workspace_diff(&self, change: &str) -> Result<Diff, Error> {
        self.diff(DiffRequest {
            to: Some(change.to_owned()),
            ..Default::default()
        })
        .await
    }

    /// Writes bytes or deletes a file with None, preserving the existing mode.
    /// New files are regular files. Missing parents are created automatically.
    /// Bytes too large for one frame are uploaded in slices first (#243).
    pub async fn workspace_write(
        &self,
        change: &str,
        path: &str,
        data: Option<Vec<u8>>,
    ) -> Result<SnapshotOutcome, Error> {
        let (change, path) = (change.to_owned(), path.to_owned());
        let request = match data {
            Some(data) if data.len() > OBJECT_CHUNK_BYTES => {
                let blob = crate::snapshot::put_held(self, &data).await?;
                Request::WorkspaceWriteBlob(WorkspaceBlobWrite { change, path, blob })
            }
            data => Request::WorkspaceWrite(WorkspaceWrite {
                change,
                path,
                data: data.map(WorkspaceBytes),
            }),
        };
        match self.request(request).await? {
            Response::Snapshot(answer) => Ok(answer),
            other => Err(unexpected(other)),
        }
    }
}
