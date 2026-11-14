//! Rosen storage: the pluggable backend interface and the git-format
//! loose-object backend (SPEC.md D5, §3).
//!
//! The object types and their git encoding live in `rosen_core`; this crate
//! persists them. [`LooseObjectStore`] is the v1 backend: it writes the same
//! loose objects git does, so imported history keeps its IDs and landed
//! history pushes to a git mirror unchanged. Other backends, such as the later
//! Rosen-native remote store, implement [`ObjectStore`].

mod backend;
mod loose;

pub use backend::{MemoryObjectStore, ObjectStore, RawObject, StoreError};
pub use loose::{LooseObjectStore, PendingObject, ReadWithin};
