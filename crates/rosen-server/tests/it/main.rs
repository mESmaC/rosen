//! The crate's integration tests, built as one test binary.
//!
//! Cargo links each file under `tests/` into its own binary, and on Windows
//! each link pulls in the whole crate and its dependencies (#414). Add new
//! integration tests as modules here unless they need their own process.

mod after_land;
mod atomic_transactions;
mod auto_revert;
mod bisection;
mod change_ids;
mod channel_events;
mod channel_quarantine;
mod claim_release;
mod collapse_check;
mod crash_recovery;
mod enrollment;
mod event_resume;
mod import_git;
mod land_check;
mod landing_fixtures;
mod landing_race;
mod ledger_push;
mod machine_management;
mod metadata;
mod mirror_push;
mod network;
mod network_file_access;
mod object_repair;
mod overlaps;
mod path_claims;
mod piece_claim_fixtures;
mod post_land;
mod project_config;
mod rebase;
mod receipt_signatures;
mod recheck;
mod relay_config;
mod review_approvals;
mod review_tiers;
mod server_workspace;
mod stack_landing;
mod stacked_changes;
mod state_push;
