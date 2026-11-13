//! Fault injection for the Rosen stress run (SPEC.md §5).
//!
//! §5 requires the stress run to keep its guarantees with the server,
//! machine daemons and clients killed at random points, with the server
//! unreachable from some machines for a period, and with an injected failing
//! commit bisected and reverted. This crate supplies those faults:
//!
//! - [`plan`]: a seeded, replayable schedule of kills and partitions.
//! - [`execute`]: plays a plan against any harness through [`execute::Faults`].
//! - [`proxy`]: cuts one machine off from the server without a firewall.
//! - [`process`]: a process that can be killed abruptly and restarted.
//! - [`failing`]: the injected failure and the post-land check that sees it.
//! - [`checker`]: the post-land checker loop that gets it bisected and reverted.
//! - [`drill`]: a complete faulted run on one computer, with its checks.

pub mod checker;
pub mod drill;
pub mod execute;
pub mod failing;
pub mod plan;
pub mod process;
pub mod proxy;
pub mod rng;
