//! The agent identity a session acts as (SPEC.md R14 "Agents").
//!
//! Velocity names each agent when it starts the agent's session and passes
//! the nick in [`AGENT_ENV`]. A person at a terminal falls back to their OS
//! user name, so the CLI works without Velocity.

use crate::Error;
use rosen_core::AgentId;

/// The environment variable Velocity sets to the agent's nick.
pub const AGENT_ENV: &str = "ROSEN_AGENT";

/// Where an identity is looked for, in order: Velocity's nick, then the OS
/// user name on Windows and on Unix.
const IDENTITY_VARS: [&str; 3] = [AGENT_ENV, "USERNAME", "USER"];

/// The agent named by the environment, if any variable names one.
pub fn agent_from_env() -> Option<String> {
    IDENTITY_VARS
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

/// `explicit` when given, else the environment's agent. Fails when neither
/// names one, or the name is not a valid agent ID.
pub fn resolve_agent(explicit: Option<&str>) -> Result<AgentId, Error> {
    let name = explicit
        .map(str::to_owned)
        .or_else(agent_from_env)
        .ok_or_else(|| Error::InvalidArgument(format!("no agent: name one or set {AGENT_ENV}")))?;
    parse_agent(&name)
}

/// Validates an agent name as the server will.
pub fn parse_agent(name: &str) -> Result<AgentId, Error> {
    AgentId::new(name).map_err(|err| Error::InvalidArgument(err.to_string()))
}
