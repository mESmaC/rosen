//! Per-agent change objects (SPEC §2 "Change", R2, D18).
//!
//! A change is a row in the metadata store, not a ref. Its ID is minted once,
//! from OS randomness, when the change is created, and nothing ever rewrites
//! it: rebases move the parent and head tree, snapshots append commits, state
//! moves along `open → ready → landed | abandoned`, and the ID stays put.

mod api;
mod error;
mod registry;
mod schema;

pub use api::{handle, BaseSource, BaseTip, ChangeRequest, ChangeResponse};
pub use error::ChangeError;
pub use registry::{
    abandon, append_commit, create, get, list, rebase, resolve, transition, ChangeRecord,
    ListFilter, NewParent,
};
pub use schema::migrate;
