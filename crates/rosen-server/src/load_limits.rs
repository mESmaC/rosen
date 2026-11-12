//! Shared admission for server workspace I/O (SPEC R3).
use std::sync::Arc;
use tokio::sync::Semaphore;

pub const DEFAULT_WORKSPACE_IO: usize = 8;

pub fn workspace_io(cap: std::num::NonZeroUsize) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(cap.get()))
}
