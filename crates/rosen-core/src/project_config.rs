//! Project configuration (SPEC "Project configuration"): the `rosen.toml`
//! file at the repository root.
//!
//! The server reads it from the base, never from a change, so no change can
//! lower its own review tier, rename the check set its receipt must cover,
//! or point the mirror somewhere else before it has landed.

use crate::claim::PathPattern;
use crate::receipt::PRE_LAND_CHECK_SET;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// Where the configuration lives in every commit's tree.
pub const CONFIG_PATH: &str = "rosen.toml";

/// The post-land check set's name when the configuration names none (R12).
pub const POST_LAND_CHECK_SET: &str = "post-land";

/// Ready workspaces each daemon keeps when the configuration sets none (R3).
pub const DEFAULT_WARM_POOL_SIZE: u32 = 4;

/// The largest warm pool a configuration may ask for: the v1 agent target
/// many times over. A bound keeps one typo from filling every machine's disk.
pub const MAX_WARM_POOL_SIZE: u32 = 1000;

/// One project's configuration. Every field is optional in the file; an
/// absent file means [`ProjectConfig::default`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectConfig {
    /// Files regenerated after a merge, never merged by hand (R5). They
    /// never count as an overlap (§2).
    pub generated: Vec<PathPattern>,
    /// Files where both sides' additions are kept (R5). They never count as
    /// an overlap (§2).
    pub registration_lists: Vec<PathPattern>,
    /// Paths whose changes take the sensitive review tier (R7).
    pub sensitive: Vec<PathPattern>,
    pub checks: CheckSets,
    pub warm_pool: WarmPool,
    pub mirror: Mirror,
}

/// The check sets receipts and post-land runs are bound to (D20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckSets {
    /// Build and lint on the exact merge result, before a land (R8).
    #[serde(deserialize_with = "pre_land_set")]
    pub pre_land: CheckSet,
    /// The full tests, on landed commits in batches (R12).
    #[serde(deserialize_with = "post_land_set")]
    pub post_land: CheckSet,
}

/// A check set as written, where `name` may be left out.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WrittenCheckSet {
    name: Option<String>,
    #[serde(default)]
    commands: Vec<String>,
}

impl WrittenCheckSet {
    fn named_or(self, default: &str) -> CheckSet {
        CheckSet {
            name: self.name.unwrap_or_else(|| default.to_owned()),
            commands: self.commands,
        }
    }
}

fn pre_land_set<'de, D: Deserializer<'de>>(input: D) -> Result<CheckSet, D::Error> {
    WrittenCheckSet::deserialize(input).map(|set| set.named_or(PRE_LAND_CHECK_SET))
}

fn post_land_set<'de, D: Deserializer<'de>>(input: D) -> Result<CheckSet, D::Error> {
    WrittenCheckSet::deserialize(input).map(|set| set.named_or(POST_LAND_CHECK_SET))
}

impl Default for CheckSets {
    fn default() -> Self {
        CheckSets {
            pre_land: CheckSet::named(PRE_LAND_CHECK_SET),
            post_land: CheckSet::named(POST_LAND_CHECK_SET),
        }
    }
}

/// A named list of commands an external checker runs (D9). Rosen runs none
/// of them; it only names them, so every checker runs the same ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSet {
    /// The name receipts carry.
    pub name: String,
    /// Shell commands, run in order from the repository root.
    #[serde(default)]
    pub commands: Vec<String>,
}

impl CheckSet {
    fn named(name: &str) -> Self {
        CheckSet {
            name: name.to_owned(),
            commands: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WarmPool {
    /// Ready local workspaces each daemon keeps at the base tip (R3). Zero
    /// turns the pool off.
    pub size: u32,
}

impl Default for WarmPool {
    fn default() -> Self {
        WarmPool {
            size: DEFAULT_WARM_POOL_SIZE,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Mirror {
    /// The git remote the base is pushed to after each land (R1): a URL or
    /// a path on the server. `None` keeps the remote `rosen import --mirror`
    /// stored.
    pub remote: Option<String>,
}

/// Why a configuration file was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{CONFIG_PATH}: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

impl ProjectConfig {
    /// Parses and validates a configuration file's text. Unknown keys are
    /// refused, so a misspelt `sensitive` cannot silently lower every tier.
    pub fn parse(text: &str) -> Result<ProjectConfig, ConfigError> {
        let config: ProjectConfig = toml::from_str(text).map_err(|error| {
            let message = error.message().trim();
            match error.span() {
                Some(span) => {
                    let line = text[..span.start].matches('\n').count() + 1;
                    ConfigError(format!("line {line}: {message}"))
                }
                None => ConfigError(message.to_owned()),
            }
        })?;
        config.validate()?;
        Ok(config)
    }

    /// A generated file: regenerated after a merge, never merged (R5).
    pub fn is_generated(&self, path: &str) -> bool {
        matches_any(&self.generated, path)
    }

    /// A registration list: both sides' additions are kept (R5).
    pub fn is_registration_list(&self, path: &str) -> bool {
        matches_any(&self.registration_lists, path)
    }

    /// Whether an edit to `path` can count as an overlap (§2). Generated
    /// files and registration lists never do.
    pub fn counts_for_overlap(&self, path: &str) -> bool {
        !self.is_generated(path) && !self.is_registration_list(path)
    }

    /// Whether a change to `path` puts its change in the sensitive tier (R7).
    pub fn is_sensitive(&self, path: &str) -> bool {
        matches_any(&self.sensitive, path)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let checks = &self.checks;
        check_set_name("checks.pre_land", &checks.pre_land.name)?;
        check_set_name("checks.post_land", &checks.post_land.name)?;
        // A shared name would let a post-land run's receipt pass as a
        // pre-land one.
        if checks.pre_land.name == checks.post_land.name {
            return Err(ConfigError(format!(
                "checks.pre_land and checks.post_land are both named {:?}",
                checks.pre_land.name
            )));
        }
        // Each path must merge one way: regenerated, or with both sides'
        // additions kept, never both.
        for generated in &self.generated {
            if let Some(list) = self
                .registration_lists
                .iter()
                .find(|list| list.overlaps(generated))
            {
                return Err(ConfigError(format!(
                    "generated {generated:?} and registration_lists {list:?} match the same paths"
                )));
            }
        }
        if self.warm_pool.size > MAX_WARM_POOL_SIZE {
            return Err(ConfigError(format!(
                "warm_pool.size {} is over the limit of {MAX_WARM_POOL_SIZE}",
                self.warm_pool.size
            )));
        }
        match &self.mirror.remote {
            Some(remote) => mirror_remote(remote),
            None => Ok(()),
        }
    }
}

fn matches_any(patterns: &[PathPattern], path: &str) -> bool {
    patterns.iter().any(|pattern| pattern.matches(path))
}

/// The same rule receipts apply, so every configured name can be submitted.
fn check_set_name(key: &str, name: &str) -> Result<(), ConfigError> {
    if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ConfigError(format!(
            "{key}.name {name:?} must be non-empty with no spaces or control characters"
        )));
    }
    Ok(())
}

/// The server runs `git push` against this text, and the text comes from
/// repository content, so transports that run a command are refused.
fn mirror_remote(remote: &str) -> Result<(), ConfigError> {
    let refuse = |why: &str| Err(ConfigError(format!("mirror.remote {remote:?} {why}")));
    if remote.trim().is_empty() {
        return refuse("is empty");
    }
    if remote.chars().any(char::is_control) {
        return refuse("contains a control character");
    }
    let lower = remote.to_ascii_lowercase();
    if lower.starts_with("ext::") || lower.starts_with("fd::") {
        return refuse("uses a transport that runs a command");
    }
    Ok(())
}
