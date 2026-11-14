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
    /// Explicit build graph; empty discovers Cargo packages from the base.
    pub build_units: std::collections::BTreeMap<String, BuildUnit>,
    /// Additional inputs shared by every build unit.
    pub workspace_inputs: Vec<PathPattern>,
    /// Files regenerated after a merge, never merged by hand (R5). They
    /// never count as an overlap (§2).
    pub generated: Vec<PathPattern>,
    /// Files where both sides' additions are kept (R5). They never count as
    /// an overlap (§2).
    pub registration_lists: Vec<PathPattern>,
    /// Paths whose changes take the sensitive review tier (R7).
    pub sensitive: Vec<PathPattern>,
    /// Untracked paths snapshots skip (R2), in `.gitignore` syntax relative
    /// to the repository root, such as `target/`. They rank below every
    /// `.gitignore` file, so a workspace's own `!` line can re-include one.
    pub ignore: Vec<String>,
    pub checks: CheckSets,
    pub warm_pool: WarmPool,
    pub prepare: Prepare,
    pub mirror: Mirror,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuildUnit {
    pub paths: Vec<PathPattern>,
    pub dependencies: Vec<String>,
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

/// Steps each daemon runs in its ready workspaces (R3). Their results must
/// not depend on the workspace's path or agent, because a prepared
/// workspace is later taken by any agent at any destination.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Prepare {
    /// Shell commands, run in order from the workspace root.
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Mirror {
    /// The git remote the base is pushed to after each land (R1): an
    /// `https`, `ssh` or `git` URL with no credentials, or an scp-style ssh
    /// address. `None` keeps the remote `rosen import --mirror` stored.
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
        self.ignore
            .iter()
            .try_for_each(|line| single_line("ignore", line))?;
        let prepare = &self.prepare.commands;
        prepare
            .iter()
            .try_for_each(|line| single_line("prepare.commands", line))?;
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

/// An ignore line or a prepare command must be one non-empty line: a line
/// break would smuggle a second rule or command past `rosen config show`.
fn single_line(key: &str, line: &str) -> Result<(), ConfigError> {
    if line.trim().is_empty() || line.chars().any(char::is_control) {
        return Err(ConfigError(format!(
            "{key} entry {line:?} must be one non-empty line with no control characters"
        )));
    }
    Ok(())
}

/// URL schemes a configured mirror remote may use. Each one reaches another
/// machine through a transport built into git. Any other scheme runs a
/// `git-remote-<scheme>` helper from the server's PATH, and `file://` writes
/// into a repository on the server itself.
const MIRROR_SCHEMES: [&str; 3] = ["https", "ssh", "git"];

/// The server runs `git push` against this text after a land, and the text
/// comes from repository content that any approved change can edit. So only
/// a network remote is accepted: a URL in [`MIRROR_SCHEMES`] or an scp-style
/// `[user@]host:path` ssh address. A local path, a remote helper (`ext::`,
/// `fd::`, any `<helper>::`) and a URL carrying credentials are refused.
/// Only `rosen import --mirror`, run by the operator on the server, can name
/// a local path (issue #69).
fn mirror_remote(remote: &str) -> Result<(), ConfigError> {
    let refuse = |why: &str| {
        let shown = without_userinfo(remote);
        Err(ConfigError(format!("mirror.remote {shown:?} {why}")))
    };
    if remote.trim().is_empty() {
        return refuse("is empty");
    }
    if remote.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return refuse("contains a space or a control character");
    }
    let checked = match remote.split_once("://") {
        Some((scheme, rest)) => url_remote(scheme, rest),
        None => scp_remote(remote),
    };
    checked.or_else(|why| refuse(&why))
}

/// `<scheme>://[user@]host[:port]/path` with a scheme in [`MIRROR_SCHEMES`].
fn url_remote(scheme: &str, rest: &str) -> Result<(), String> {
    if !MIRROR_SCHEMES.contains(&scheme) {
        return Err(format!(
            "must use one of the schemes {MIRROR_SCHEMES:?}; only `rosen import --mirror` can name another"
        ));
    }
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if path.is_empty() {
        return Err("names no repository path".to_owned());
    }
    let (user, host_port) = match authority.rsplit_once('@') {
        Some((user, host_port)) => (Some(user), host_port),
        None => (None, authority),
    };
    if let Some(user) = user {
        // GitHub and GitLab take an access token as the https user name, so
        // any https user is treated as a credential, and a password always is.
        if scheme == "https" || user.contains(':') {
            return Err(
                "carries credentials; give them to git's credential helper on the server"
                    .to_owned(),
            );
        }
        user_name(user)?;
    }
    let (host, port) = split_port(host_port);
    if !port.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("has a port {port:?} that is not a number"));
    }
    host_name(host)
}

/// `[user@]host:path`, which git reads as ssh. Anything git would read as a
/// local path or a remote helper is refused instead.
fn scp_remote(remote: &str) -> Result<(), String> {
    let not_network = || {
        Err(format!(
            "is not a network remote: use a {MIRROR_SCHEMES:?} URL or `user@host:path`; only `rosen import --mirror` can name a local path"
        ))
    };
    // A bracketed IPv6 host (`git@[2001:db8::1]:repo`) has colons of its
    // own, so the separator is the one after `]`. `host_name` then checks
    // the brackets enclose only an address.
    let split = match remote.find("]:") {
        Some(end) => Some((&remote[..=end], &remote[end + 2..])),
        None => remote.split_once(':'),
    };
    let Some((authority, path)) = split else {
        return not_network();
    };
    // A separator before the colon makes git read a local path; `x::` is a
    // remote helper; a one-letter host is a Windows drive such as `C:`.
    if authority.contains(['/', '\\']) || path.is_empty() || path.starts_with(':') {
        return not_network();
    }
    let (user, host) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, authority),
    };
    if host.chars().count() < 2 {
        return not_network();
    }
    if let Some(user) = user {
        user_name(user)?;
    }
    host_name(host)
}

/// A user name git passes to ssh; one starting with `-` would be an option.
/// No `:` either: git reads `<scheme>::` at the start of a remote as a
/// helper before it looks for a user, as in `ext::x@[::1]:repo`.
fn user_name(user: &str) -> Result<(), String> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '~');
    if user.is_empty() || user.starts_with('-') || !user.chars().all(allowed) {
        return Err(format!("has an invalid user name {user:?}"));
    }
    Ok(())
}

/// A DNS name, an IPv4 address or a bracketed IPv6 address. A host starting
/// with `-` would reach ssh as an option such as `-oProxyCommand`.
fn host_name(host: &str) -> Result<(), String> {
    let bracketed = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .is_some_and(|inner| {
            !inner.is_empty()
                && inner
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        });
    let named = !host.is_empty()
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if bracketed || named {
        Ok(())
    } else {
        Err(format!("has an invalid host {host:?}"))
    }
}

/// Splits `host:port` at the port's colon, leaving a bracketed IPv6 host
/// whole. The port is empty when there is none.
fn split_port(host_port: &str) -> (&str, &str) {
    let after_brackets = host_port.rfind(']').map_or(0, |end| end + 1);
    match host_port[after_brackets..].rfind(':') {
        Some(colon) => {
            let colon = after_brackets + colon;
            (&host_port[..colon], &host_port[colon + 1..])
        }
        None => (host_port, ""),
    }
}

/// The remote with any URL user information blanked, so a refusal that
/// `rosen config show` prints never repeats a token.
fn without_userinfo(remote: &str) -> String {
    let Some((scheme, rest)) = remote.split_once("://") else {
        return remote.to_owned();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://***@{}", &rest[at + 1..]),
        None => remote.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Risk (SPEC R1, issue #69): `mirror.remote` is repository content that
    /// the server hands to `git push`. Accepting a local path or a helper
    /// would let any approved change push into, or run, things on the server.
    #[test]
    fn mirror_remote_accepts_only_network_remotes() {
        let parsed =
            |remote: &str| ProjectConfig::parse(&format!("[mirror]\nremote = {remote:?}\n"));
        for good in [
            "https://github.com/you/repo.git",
            "ssh://git@github.com:22/you/repo.git",
            "git://example.com/repo.git",
            "git@github.com:you/repo.git",
            "ssh://[::1]/repo.git",
            "git@[2001:db8::1]:team/repo.git",
        ] {
            assert!(parsed(good).is_ok(), "refused {good}");
        }
        for bad in [
            "/srv/git/other.git",
            r"C:\repos\other.git",
            "C:/repos/other.git",
            "../other.git",
            "file:///srv/git/other.git",
            "ext::sh -c calc",
            "fd::3",
            "custom::anything",
            "ext::calc@[::1]:repo.git",
            "http://example.com/repo.git",
            "https://ghp_token@github.com/you/repo.git",
            "ssh://user:secret@example.com/repo.git",
            "ssh://-oProxyCommand=calc/repo.git",
            "-oProxyCommand=calc:repo.git",
            "https://github.com",
        ] {
            assert!(parsed(bad).is_err(), "accepted {bad}");
        }
        let refusal = parsed("https://ghp_token@github.com/r.git").unwrap_err();
        assert!(!refusal.to_string().contains("ghp_token"), "{refusal}");
    }
}
