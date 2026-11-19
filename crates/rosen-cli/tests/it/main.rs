//! The crate's integration tests, built as one test binary.
//!
//! Cargo links each file under `tests/` into its own binary, and on Windows
//! each link pulls in the whole crate and its dependencies (#414). Add new
//! integration tests as modules here unless they need their own process.

mod json_output;
mod run_nested;
mod run_output;
