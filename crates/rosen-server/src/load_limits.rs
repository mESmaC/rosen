//! Shared admission for server workspace I/O and protocol buffers (SPEC R3).
use rosen_protocol::frame::{read_frame_len, read_frame_payload};
use rosen_protocol::Request;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const DEFAULT_WORKSPACE_IO: usize = 8;

pub fn workspace_io(cap: std::num::NonZeroUsize) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(cap.get()))
}

const MIB: u32 = 1024 * 1024;
const INCOMING_MIB: usize = 128;
const LARGE_REQUESTS: usize = 3;
pub(crate) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) struct TransferLimits {
    incoming: Arc<Semaphore>,
    pub(crate) large_requests: Arc<Semaphore>,
}

impl Default for TransferLimits {
    fn default() -> Self {
        Self {
            incoming: Arc::new(Semaphore::new(INCOMING_MIB)),
            large_requests: Arc::new(Semaphore::new(LARGE_REQUESTS)),
        }
    }
}

impl TransferLimits {
    pub(crate) async fn read_body(
        &self,
        reader: &mut (impl AsyncRead + Unpin),
    ) -> io::Result<Option<Charged<Vec<u8>>>> {
        let Some(len) = read_frame_len(reader).await? else {
            return Ok(None);
        };
        let weight = len.div_ceil(MIB).max(1);
        let permit = self
            .incoming
            .clone()
            .acquire_many_owned(weight)
            .await
            .expect("transfer limits stay open");
        // Wait for a length without a reservation. Once bytes are reserved,
        // a stalled peer has a deadline to deliver them and free the buffer.
        let value = tokio::time::timeout(TRANSFER_TIMEOUT, read_frame_payload(reader, len))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "incomplete frame"))??;
        Ok(Some(Charged {
            value,
            permit: Some(permit),
        }))
    }
}

/// The reservation travels with a frame through ownership transfers, until
/// the frame has been consumed or the connection has dropped it.
pub(crate) struct Charged<T> {
    pub(crate) value: T,
    pub(crate) permit: Option<OwnedSemaphorePermit>,
}

impl<T> From<T> for Charged<T> {
    fn from(value: T) -> Self {
        Self {
            value,
            permit: None,
        }
    }
}

pub(crate) fn large_request(request: &Request) -> bool {
    matches!(
        request,
        Request::PutObjects(_)
            | Request::PutBlobChunk(_)
            | Request::Snapshot(_)
            | Request::GetObjects(_)
            | Request::GetObjectChunk(_)
            | Request::EnsureObjects(_)
            | Request::WorkspaceWrite(_)
            | Request::WorkspaceWriteBlob(_)
            | Request::ReadFile(_)
            | Request::Diff(_)
            | Request::Blame(_)
            | Request::Log(_)
            | Request::MergeBase { .. }
            | Request::MergeResult { .. }
            | Request::Overlaps(_)
            | Request::EditRegions { .. }
            | Request::ChangeRebase { .. }
            | Request::Land { .. }
            | Request::Import { .. }
            | Request::MirrorAdopt { .. }
            | Request::MirrorCatchUp
            | Request::MirrorDropPaths { .. }
    )
}
