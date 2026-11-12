//! Rosen data model: changes, stacks, conflicts, receipts, reviews, path
//! claims and edit regions (SPEC.md §2, §3).
//!
//! This crate is pure logic and performs no I/O, so the server, the client
//! and the proofs' reference models can all share it.

// TEMPORARY stand-ins for task:2 (define-the-core); replaced on rebase.
pub mod change;
pub mod object;
