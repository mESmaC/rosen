//! Per-agent change objects (SPEC §2 "Change", R2, D18).
//!
//! A change is a row in the metadata store, not a ref. Its ID is minted once,
//! from OS randomness, when the change is created, and nothing ever rewrites
//! it: rebases move the parent and head, snapshots append commits, the state
//! moves along `open → ready → landed | abandoned`, and the ID stays put. The
//! rules themselves live in `rosen_core::Change`; this module persists them.

mod error;
mod registry;
mod schema;
mod service;

pub use error::ChangeError;
pub use registry::{
    abandon, create, get, list, rebase, rebase_checked, record_snapshot, record_snapshot_merged,
    resolve, update, NewParent,
};
pub use schema::migrate;
pub use service::{BaseSource, BaseTip, ChangeService, NoBase, METADATA_DB};
