//! Project configuration (SPEC "Project configuration"): the `rosen.toml`
//! file at the repository root.
//!
//! The server reads it from the base, never from a change, so no change can
//! lower its own review tier, rename the check set its receipt must cover,
//! or point the mirror somewhere else before it has landed.

use crate::claim::PathPattern;
use crate::platform::Platform;
use crate::receipt::PRE_LAND_CHECK_SET;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectConfig {
    /// Explicit build graph; empty discovers Cargo packages from the base.
    pub build_units: std::collections::BTreeMap<String, BuildUnit>,
    /// Additional inputs shared by every build unit.
    pub workspace_inputs: Vec<PathPattern>,
    /// Files regenerated after a merge, never merged by hand (R5). They
    /// never count as an overlap (§2).
    pub generated: Vec<PathPattern>,
    /// Commands registered in `[generated.commands]` to regenerate generated
    /// files on the merged tree at land (R5, R8, #428). Every pattern here is
    /// also treated as a generated file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub generated_commands: BTreeMap<PathPattern, String>,
    /// Files where both sides' additions are kept (R5). They never count as
    /// an overlap (§2).
    pub registration_lists: Vec<PathPattern>,
    /// Paths whose changes take the sensitive review tier (R7).
    pub sensitive: Vec<PathPattern>,
    /// Untracked paths snapshots skip (R2), in `.gitignore` syntax relative
    /// to the repository root, such as `target/`. They rank below every
    /// `.gitignore` file, so a workspace's own `!` line can re-include one.
    pub ignore: Vec<String>,
    /// Whether an approval gates a land (R7, R8). Left out of the printed
    /// configuration when it is the default, so a `before-land` project's
    /// `config show` output is what it was before the setting existed.
    #[serde(skip_serializing_if = "ReviewMode::is_before_land")]
    pub review: ReviewMode,
    /// Receipt reuse on a moved base (R8); reach preserves the original rule.
    #[serde(skip_serializing_if = "RecheckMode::is_reach")]
    pub recheck: RecheckMode,
    pub checks: CheckSets,
    pub warm_pool: WarmPool,
    pub prepare: Prepare,
    pub mirror: Mirror,
    /// Maximum landed blob length in bytes, including intermediate stack
    /// layers. Defaults to GitHub's 100 MiB limit even without a mirror.
    #[serde(
        default = "default_max_file_size",
        skip_serializing_if = "is_default_max_file_size"
    )]
    pub max_file_size: u64,
    /// Where the server sends new Discoveries and Past work entries after
    /// each append (R13), such as Collapse's
    /// `https://api.collapse.sh/<owner>/<name>/ledgers`. `None` sends
    /// nothing. Like `mirror.remote`, it never holds a credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger_endpoint: Option<String>,
    /// The project's Collapse root, such as
    /// `https://api.collapse.sh/<owner>/<name>`. The server sends ledger
    /// entries to `<collapse>/ledgers` when `ledger_endpoint` is unset, and
    /// its live state (open changes, review, coverage and this
    /// configuration) to `<collapse>/rosen/state`. `None` sends no state.
    /// It never holds a credential either.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapse: Option<String>,
    /// Whether the state feed also sends channel lines: messages, claims
    /// and presence changes (R9, R13). Off by default, since channel lines
    /// hold agent messages and the owner decides whether they leave the
    /// machine. Ledger entries never go this way; the ledger push sends them.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub collapse_channel: bool,
    /// Named steps an external checker runs after a post-land full-test green
    /// checkpoint covers a range touching their paths (R12). Kept in source
    /// declaration order; omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deploy: Vec<DeployStep>,
}

pub const DEFAULT_MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

fn default_max_file_size() -> u64 {
    DEFAULT_MAX_FILE_SIZE
}
fn is_default_max_file_size(size: &u64) -> bool {
    *size == DEFAULT_MAX_FILE_SIZE
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            build_units: Default::default(),
            workspace_inputs: Vec::new(),
            generated: Vec::new(),
            generated_commands: BTreeMap::new(),
            registration_lists: Vec::new(),
            sensitive: Vec::new(),
            ignore: Vec::new(),
            review: Default::default(),
            recheck: Default::default(),
            checks: Default::default(),
            warm_pool: Default::default(),
            prepare: Default::default(),
            mirror: Default::default(),
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            ledger_endpoint: None,
            collapse: None,
            collapse_channel: false,
            deploy: Vec::new(),
        }
    }
}

/// When a receipt for the current head counts on a moved base (R8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecheckMode {
    #[default]
    Reach,
    Merge,
}

impl RecheckMode {
    pub fn is_reach(&self) -> bool {
        *self == Self::Reach
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reach => "reach",
            Self::Merge => "merge",
        }
    }
}

/// When review happens relative to a land (R7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewMode {
    /// A land needs an approval for the change's current head.
    #[default]
    BeforeLand,
    /// A land needs neither approval nor receipt. Reviewers review landed changes and record
    /// it as their reviewed state; post-land checks (R12) are the safety net.
    AfterLand,
}

impl ReviewMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewMode::BeforeLand => "before-land",
            ReviewMode::AfterLand => "after-land",
        }
    }

    pub fn is_before_land(&self) -> bool {
        *self == ReviewMode::BeforeLand
    }

    /// Whether a land needs an approval for the change's current head.
    pub fn approval_gates_land(self) -> bool {
        self.is_before_land()
    }
}

impl fmt::Display for ReviewMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
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
    #[serde(default)]
    platforms: Vec<Platform>,
    #[serde(default)]
    platform_commands: BTreeMap<Platform, Vec<String>>,
}

impl WrittenCheckSet {
    fn named_or(self, default: &str) -> CheckSet {
        CheckSet {
            name: self.name.unwrap_or_else(|| default.to_owned()),
            commands: self.commands,
            platforms: self.platforms,
            platform_commands: self.platform_commands,
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
    /// Required platforms (`["windows", "macos", "linux"]`). Empty means any platform (R12).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<Platform>,
    /// Additional commands appended when running on matching platforms (R12).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub platform_commands: BTreeMap<Platform, Vec<String>>,
}

impl CheckSet {
    fn named(name: &str) -> Self {
        CheckSet {
            name: name.to_owned(),
            commands: Vec::new(),
            platforms: Vec::new(),
            platform_commands: BTreeMap::new(),
        }
    }

    /// Commands a checker on `platform` runs: the shared `commands` followed
    /// by any `platform_commands` entries matching `platform`.
    pub fn commands_for(&self, platform: Option<&Platform>) -> Vec<String> {
        let mut all = self.commands.clone();
        if let Some(platform) = platform {
            for (target, extra) in &self.platform_commands {
                if platform.matches(target) {
                    all.extend(extra.iter().cloned());
                }
            }
        }
        all
    }

    /// All configured platform entries in `self.platforms` and
    /// `self.platform_commands` that `checker` satisfies, ordered with required
    /// entries before advisory ones and architecture-specific matches before
    /// OS-only ones within each tier (R12, #459).
    pub fn matching_targets(&self, checker: &Platform) -> Vec<&Platform> {
        let mut matches = Vec::new();
        Self::push_matches(&mut matches, self.platforms.iter(), checker);
        Self::push_matches(&mut matches, self.advisory_platforms(), checker);
        matches
    }

    /// The highest-priority configured platform entry in `self.platforms` or
    /// `self.platform_commands` that `checker` satisfies.
    pub fn matching_target(&self, checker: &Platform) -> Option<&Platform> {
        self.matching_targets(checker).into_iter().next()
    }

    /// Whether `target` is a required platform that gates green for this check set.
    pub fn is_required_platform(&self, target: &Platform) -> bool {
        self.platforms.contains(target)
    }

    /// Whether `target` is either a required platform or an advisory
    /// `platform_commands` target for this check set (R12, #457).
    pub fn accepts_platform(&self, target: &Platform) -> bool {
        self.platforms.contains(target)
            || self.advisory_platforms().any(|advisory| advisory == target)
    }

    /// Advisory targets declared in `platform_commands` that are not already
    /// covered by a required entry in `platforms` and do not gate green.
    pub fn advisory_platforms(&self) -> impl Iterator<Item = &Platform> {
        self.platform_commands
            .keys()
            .filter(|target| !self.platforms.iter().any(|req| target.matches(req)))
    }

    fn push_matches<'a>(
        out: &mut Vec<&'a Platform>,
        targets: impl Iterator<Item = &'a Platform>,
        checker: &Platform,
    ) {
        let mut generic = Vec::new();
        for target in targets {
            if !checker.matches(target) || out.contains(&target) {
                continue;
            }
            if target.arch().is_some() {
                out.push(target);
            } else if !generic.contains(&target) {
                generic.push(target);
            }
        }
        out.extend(generic);
    }

    fn validate(&self, key: &str) -> Result<(), ConfigError> {
        check_set_name(key, &self.name)?;
        let mut seen = std::collections::BTreeSet::new();
        for platform in &self.platforms {
            if !seen.insert(platform) {
                return Err(ConfigError(format!(
                    "{key}.platforms lists {platform:?} more than once"
                )));
            }
        }
        for (platform, commands) in &self.platform_commands {
            let subkey = format!("{key}.platform_commands.{platform}");
            commands
                .iter()
                .try_for_each(|line| single_line(&subkey, line))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WarmPool {
    /// Ready local workspaces kept at the workspace starting commit (R3). Zero
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

/// One deploy step declared under `[deploy.<name>]` in `rosen.toml` (R12).
///
/// Steps run in declaration order, one at a time, after a post-land full-test
/// green checkpoint covers a landed range touching `paths` (or any green range
/// when `paths` is empty). `secrets` carries names only, never values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployStep {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub paths: Vec<PathPattern>,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
}

impl DeployStep {
    /// Whether this step is due for a green landed range that touched `paths`.
    /// An empty `paths` list is not limited to a subset and matches every range.
    pub fn matches_any(&self, paths: &[String]) -> bool {
        self.paths.is_empty()
            || paths
                .iter()
                .any(|path| self.paths.iter().any(|pattern| pattern.matches(path)))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WrittenDeployStep {
    command: String,
    #[serde(default)]
    paths: Vec<PathPattern>,
    #[serde(default)]
    secrets: Vec<String>,
    #[serde(default)]
    platforms: Vec<String>,
}

/// `[deploy]` as written in `rosen.toml`: named step tables, kept in their
/// source declaration order using each key's TOML byte span.
#[derive(Default)]
struct WrittenDeploy(Vec<DeployStep>);

impl<'de> Deserialize<'de> for WrittenDeploy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DeployVisitor;

        impl<'de> Visitor<'de> for DeployVisitor {
            type Value = WrittenDeploy;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a [deploy] table of named steps")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries: Vec<(usize, DeployStep)> = Vec::new();
                while let Some((key, step)) =
                    map.next_entry::<toml::Spanned<String>, WrittenDeployStep>()?
                {
                    let start = key.span().start;
                    entries.push((
                        start,
                        DeployStep {
                            name: key.into_inner(),
                            command: step.command,
                            paths: step.paths,
                            secrets: step.secrets,
                            platforms: step.platforms,
                        },
                    ));
                }
                entries.sort_by_key(|(start, _)| *start);
                Ok(WrittenDeploy(
                    entries.into_iter().map(|(_, step)| step).collect(),
                ))
            }
        }

        deserializer.deserialize_map(DeployVisitor)
    }
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

/// `generated` as written in `rosen.toml`: either a list of path globs
/// (`generated = [...]`) or a table (`[generated] paths = [...]` and/or
/// `[generated.commands] "path" = "command"`).
#[derive(Default)]
struct WrittenGenerated {
    paths: Vec<PathPattern>,
    commands: BTreeMap<PathPattern, String>,
}

impl<'de> Deserialize<'de> for WrittenGenerated {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct GeneratedVisitor;

        impl<'de> Visitor<'de> for GeneratedVisitor {
            type Value = WrittenGenerated;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list of path patterns or a [generated] table")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut paths = Vec::new();
                while let Some(pattern) = seq.next_element()? {
                    paths.push(pattern);
                }
                Ok(WrittenGenerated {
                    paths,
                    commands: BTreeMap::new(),
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut paths = None;
                let mut commands = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "paths" => {
                            if paths.is_some() {
                                return Err(de::Error::duplicate_field("paths"));
                            }
                            paths = Some(map.next_value()?);
                        }
                        "commands" => {
                            if commands.is_some() {
                                return Err(de::Error::duplicate_field("commands"));
                            }
                            commands = Some(map.next_value()?);
                        }
                        other => {
                            return Err(de::Error::unknown_field(other, &["paths", "commands"]))
                        }
                    }
                }
                Ok(WrittenGenerated {
                    paths: paths.unwrap_or_default(),
                    commands: commands.unwrap_or_default(),
                })
            }
        }

        deserializer.deserialize_any(GeneratedVisitor)
    }
}

/// `rosen.toml` as written on disk, accepting both `generated = [...]` and
/// `[generated.commands]` while refusing unknown keys.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct WrittenProjectConfig {
    build_units: BTreeMap<String, BuildUnit>,
    workspace_inputs: Vec<PathPattern>,
    generated: WrittenGenerated,
    registration_lists: Vec<PathPattern>,
    sensitive: Vec<PathPattern>,
    ignore: Vec<String>,
    review: ReviewMode,
    recheck: RecheckMode,
    checks: CheckSets,
    warm_pool: WarmPool,
    prepare: Prepare,
    mirror: Mirror,
    #[serde(default = "default_max_file_size")]
    max_file_size: u64,
    ledger_endpoint: Option<String>,
    collapse: Option<String>,
    collapse_channel: bool,
    deploy: WrittenDeploy,
}

impl Default for WrittenProjectConfig {
    fn default() -> Self {
        let base = ProjectConfig::default();
        Self {
            build_units: base.build_units,
            workspace_inputs: base.workspace_inputs,
            generated: WrittenGenerated::default(),
            registration_lists: base.registration_lists,
            sensitive: base.sensitive,
            ignore: base.ignore,
            review: base.review,
            recheck: base.recheck,
            checks: base.checks,
            warm_pool: base.warm_pool,
            prepare: base.prepare,
            mirror: base.mirror,
            max_file_size: base.max_file_size,
            ledger_endpoint: base.ledger_endpoint,
            collapse: base.collapse,
            collapse_channel: base.collapse_channel,
            deploy: WrittenDeploy::default(),
        }
    }
}

impl From<WrittenProjectConfig> for ProjectConfig {
    fn from(written: WrittenProjectConfig) -> Self {
        let mut generated = written.generated.paths;
        for pattern in written.generated.commands.keys() {
            if !generated.contains(pattern) {
                generated.push(pattern.clone());
            }
        }
        ProjectConfig {
            build_units: written.build_units,
            workspace_inputs: written.workspace_inputs,
            generated,
            generated_commands: written.generated.commands,
            registration_lists: written.registration_lists,
            sensitive: written.sensitive,
            ignore: written.ignore,
            review: written.review,
            recheck: written.recheck,
            checks: written.checks,
            warm_pool: written.warm_pool,
            prepare: written.prepare,
            mirror: written.mirror,
            max_file_size: written.max_file_size,
            ledger_endpoint: written.ledger_endpoint,
            collapse: written.collapse,
            collapse_channel: written.collapse_channel,
            deploy: written.deploy.0,
        }
    }
}

impl ProjectConfig {
    /// Parses and validates a configuration file's text. Unknown keys are
    /// refused, so a misspelt `sensitive` cannot silently lower every tier.
    pub fn parse(text: &str) -> Result<ProjectConfig, ConfigError> {
        let written: WrittenProjectConfig = toml::from_str(text).map_err(|error| {
            let message = error.message().trim();
            match error.span() {
                Some(span) => {
                    let line = text[..span.start].matches('\n').count() + 1;
                    ConfigError(format!("line {line}: {message}"))
                }
                None => ConfigError(message.to_owned()),
            }
        })?;
        let config = ProjectConfig::from(written);
        config.validate()?;
        Ok(config)
    }

    /// A generated file: regenerated after a merge, never merged (R5).
    pub fn is_generated(&self, path: &str) -> bool {
        matches_any(&self.generated, path)
            || self.generated_commands.keys().any(|p| p.matches(path))
    }

    /// The command registered in `[generated.commands]` to regenerate `path`,
    /// if any (R5, R8, #428).
    pub fn generated_command(&self, path: &str) -> Option<&str> {
        self.generated_commands
            .iter()
            .find(|(pattern, _)| pattern.matches(path))
            .map(|(_, command)| command.as_str())
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

    /// The patterns whose paths never count as an overlap (§2), for
    /// comparing two globs with [`PathPattern::overlaps_outside`].
    pub fn uncounted_patterns(&self) -> Vec<PathPattern> {
        let mut patterns = self.generated.clone();
        for pattern in self.generated_commands.keys() {
            if !patterns.contains(pattern) {
                patterns.push(pattern.clone());
            }
        }
        patterns.extend(self.registration_lists.iter().cloned());
        patterns
    }

    /// Whether a change to `path` puts its change in the sensitive tier (R7).
    pub fn is_sensitive(&self, path: &str) -> bool {
        matches_any(&self.sensitive, path)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let checks = &self.checks;
        checks.pre_land.validate("checks.pre_land")?;
        checks.post_land.validate("checks.post_land")?;
        // A shared name would let a post-land run's receipt pass as a
        // pre-land one.
        if checks.pre_land.name == checks.post_land.name {
            return Err(ConfigError(format!(
                "checks.pre_land and checks.post_land are both named {:?}",
                checks.pre_land.name
            )));
        }
        self.validate_generated()?;
        self.validate_deploy()?;
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
        if let Some(remote) = &self.mirror.remote {
            mirror_remote(remote)?;
        }
        if let Some(url) = &self.ledger_endpoint {
            endpoint_url("ledger_endpoint", url)?;
        }
        match &self.collapse {
            Some(url) => endpoint_url("collapse", url),
            None => Ok(()),
        }
    }

    fn validate_generated(&self) -> Result<(), ConfigError> {
        let command_patterns: Vec<&PathPattern> = self.generated_commands.keys().collect();
        for (index, pattern) in command_patterns.iter().enumerate() {
            let command = &self.generated_commands[*pattern];
            single_line("generated.commands", command)?;
            // Two overlapping command patterns would make the command for a
            // matching path ambiguous.
            if let Some(other) = command_patterns[index + 1..]
                .iter()
                .find(|other| pattern.overlaps(other))
            {
                return Err(ConfigError(format!(
                    "generated.commands {pattern:?} and {other:?} match the same paths"
                )));
            }
        }
        // Each path must merge one way: regenerated, or with both sides'
        // additions kept, never both.
        for generated in self.generated.iter().chain(self.generated_commands.keys()) {
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
        Ok(())
    }

    fn validate_deploy(&self) -> Result<(), ConfigError> {
        let mut seen = std::collections::BTreeSet::new();
        for step in &self.deploy {
            deploy_step_name(&step.name)?;
            if !seen.insert(step.name.as_str()) {
                return Err(ConfigError(format!(
                    "deploy step {:?} is defined more than once",
                    step.name
                )));
            }
            single_line(&format!("deploy.{}.command", step.name), &step.command)?;
            for secret in &step.secrets {
                secret_name(&step.name, secret)?;
            }
            for platform in &step.platforms {
                platform_name(&step.name, platform)?;
            }
        }
        Ok(())
    }

    /// Where the ledger push sends entries (R13): `ledger_endpoint`, or
    /// else the `ledgers` endpoint under `collapse`.
    pub fn ledger_push_url(&self) -> Option<String> {
        self.ledger_endpoint
            .clone()
            .or_else(|| self.collapse_url("ledgers"))
    }

    /// Where the state feed sends live state (R9): `rosen/state` under
    /// `collapse`, or `None` when the project names no Collapse root.
    pub fn state_feed_url(&self) -> Option<String> {
        self.collapse_url("rosen/state")
    }

    /// `<collapse>/<path>`, when the project names a Collapse root. The
    /// action relay (R14) reads its doorbell and actions under it.
    pub fn collapse_url(&self, path: &str) -> Option<String> {
        let root = self.collapse.as_deref()?;
        Some(format!("{}/{path}", root.trim_end_matches('/')))
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

const MAX_SECRET_NAME_LEN: usize = 128;
const MAX_PLATFORM_LEN: usize = 64;

fn deploy_step_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ConfigError(format!(
            "deploy step name {name:?} must be non-empty with no spaces or control characters"
        )));
    }
    Ok(())
}

/// Secret entries name secrets only, never values, so a committed `rosen.toml`
/// cannot carry credentials and error messages never echo a pasted value.
fn secret_name(step: &str, entry: &str) -> Result<(), ConfigError> {
    if let Some((prefix, _)) = entry.split_once('=') {
        return Err(ConfigError(format!(
            "deploy.{step}.secrets entry {prefix:?}=*** must be a secret name, never a value"
        )));
    }
    let valid_start = |c: char| c.is_ascii_alphabetic() || c == '_';
    let valid_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-');
    if entry.is_empty()
        || entry.len() > MAX_SECRET_NAME_LEN
        || !entry.chars().next().is_some_and(valid_start)
        || !entry.chars().all(valid_char)
    {
        return Err(ConfigError(format!(
            "deploy.{step}.secrets must list secret names only (letters, digits, '_' or '-'), never values"
        )));
    }
    Ok(())
}

fn platform_name(step: &str, platform: &str) -> Result<(), ConfigError> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/');
    if platform.is_empty()
        || platform.len() > MAX_PLATFORM_LEN
        || platform.starts_with('-')
        || !platform.chars().all(allowed)
    {
        return Err(ConfigError(format!(
            "deploy.{step}.platforms entry {platform:?} must be a non-empty platform name with no spaces or control characters"
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

/// The server POSTs ledger entries or live state to this URL with the push
/// token from its credential helper, and the text is repository content any approved change
/// can edit. So it must be `https`, where the token is never sent in the
/// clear, and carry no credentials of its own. Plain `http` is accepted only
/// for a loopback host, which never leaves the server machine, so a local
/// endpoint can be tried out.
fn endpoint_url(key: &str, url: &str) -> Result<(), ConfigError> {
    let refuse = |why: &str| {
        let shown = without_userinfo(url);
        Err(ConfigError(format!("{key} {shown:?} {why}")))
    };
    // The URL reaches curl inside a quoted config line; these characters
    // could end that quoting, and none belongs in a URL.
    let plain = |c: char| c.is_ascii_graphic() && c != '"' && c != '\\';
    if url.is_empty() || !url.chars().all(plain) {
        return refuse("must be a URL with no spaces, quotes, backslashes or control characters");
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return refuse("must be an https URL");
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.contains('@') {
        return refuse("carries credentials; give them to git's credential helper on the server");
    }
    if path.is_empty() {
        return refuse("names no path");
    }
    let (host, port) = split_port(authority);
    if !port.chars().all(|c| c.is_ascii_digit()) {
        return refuse(&format!("has a port {port:?} that is not a number"));
    }
    if let Err(why) = host_name(host) {
        return refuse(&why);
    }
    match scheme {
        "https" => Ok(()),
        "http" if matches!(host, "localhost" | "127.0.0.1" | "[::1]") => Ok(()),
        "http" => refuse("must use https; plain http is only for a loopback host"),
        _ => refuse("must be an https URL"),
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

    /// Risk (issue #395): the server sends the push token to this URL. One
    /// carrying a token, or plain http to another machine, would publish it.
    #[test]
    fn ledger_endpoint_accepts_only_https_without_credentials() {
        let parsed = |url: &str| ProjectConfig::parse(&format!("ledger_endpoint = {url:?}\n"));
        for good in [
            "https://api.collapse.sh/mESmaC/rosen/ledgers",
            "http://127.0.0.1:8080/o/r/ledgers",
        ] {
            assert!(parsed(good).is_ok(), "refused {good}");
        }
        for bad in [
            "https://tok@api.collapse.sh/o/r/ledgers",
            "http://api.collapse.sh/o/r/ledgers",
            "file:///srv/ledgers",
            "https://api.collapse.sh",
            "https://api.collapse.sh/a\"b",
        ] {
            assert!(parsed(bad).is_err(), "accepted {bad}");
        }
        let refusal = parsed("https://tok@api.collapse.sh/o/r/ledgers").unwrap_err();
        assert!(!refusal.to_string().contains("tok@"), "{refusal}");
        let collapse = ProjectConfig::parse(
            "collapse = \"https://tok@api.collapse.sh/o/r\"
",
        );
        assert!(collapse.is_err(), "accepted a collapse root with a token");
    }

    /// Risk (#428): `[generated.commands]` registers shell commands the server
    /// runs at land; overlapping patterns, multiline commands or registration-list
    /// collisions would make command selection ambiguous or bypass config review.
    #[test]
    fn generated_commands_parse_and_validate() {
        let array = ProjectConfig::parse("generated = [\"proofs/lake-manifest.json\"]\n").unwrap();
        assert!(array.is_generated("proofs/lake-manifest.json"));
        assert_eq!(array.generated_command("proofs/lake-manifest.json"), None);

        let table = ProjectConfig::parse(
            "[generated]\npaths = [\"proofs/lake-manifest.json\"]\n\
             [generated.commands]\n\
             \"Cargo.lock\" = \"cargo metadata --format-version 1\"\n",
        )
        .unwrap();
        assert!(table.is_generated("proofs/lake-manifest.json"));
        assert!(table.is_generated("Cargo.lock"));
        assert!(!table.counts_for_overlap("Cargo.lock"));
        assert_eq!(
            table.generated_command("Cargo.lock"),
            Some("cargo metadata --format-version 1")
        );
        assert_eq!(table.generated_command("proofs/lake-manifest.json"), None);

        for bad in [
            "[generated.commands]\n\"Cargo.lock\" = \"\"\n",
            "[generated.commands]\n\"Cargo.lock\" = \"a\\nb\"\n",
            "[generated.commands]\n\"*.lock\" = \"a\"\n\"Cargo.lock\" = \"b\"\n",
            "registration_lists = [\"Cargo.lock\"]\n[generated.commands]\n\"Cargo.lock\" = \"a\"\n",
            "[generated]\nunknown = []\n",
        ] {
            assert!(ProjectConfig::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    /// Risk (#440): per-platform check sets govern which checkers run which
    /// commands and when the base goes green; duplicate/unknown platforms or
    /// empty-field serialization drift would break matching or golden schemas.
    #[test]
    fn check_set_platforms_and_platform_commands_validate() {
        let config = ProjectConfig::parse(
            "[checks.post_land]\n\
             platforms = [\"windows\", \"macos\", \"linux\"]\n\
             commands = [\"cargo test --workspace\"]\n\
             [checks.post_land.platform_commands]\n\
             macos = [\"cd proofs && lake build --wfail\"]\n",
        )
        .unwrap();
        let post = &config.checks.post_land;
        assert_eq!(post.platforms.len(), 3);
        let mac_arm = Platform::parse("macos-arm64").unwrap();
        let win_x64 = Platform::parse("windows-x86_64").unwrap();
        assert_eq!(post.matching_target(&mac_arm).unwrap().as_str(), "macos");
        assert_eq!(
            post.commands_for(Some(&mac_arm)),
            vec![
                "cargo test --workspace".to_owned(),
                "cd proofs && lake build --wfail".to_owned(),
            ]
        );
        assert_eq!(
            post.commands_for(Some(&win_x64)),
            vec!["cargo test --workspace".to_owned()]
        );

        let default_toml = toml::to_string(&ProjectConfig::default().checks.post_land).unwrap();
        assert!(!default_toml.contains("platforms"));
        assert!(!default_toml.contains("platform_commands"));

        for bad in [
            "[checks.post_land]\nplatforms = [\"windows\", \"windows\"]\n",
            "[checks.post_land]\nplatforms = [\"solaris\"]\n",
            "[checks.post_land.platform_commands]\nmacos = [\"\"]\n",
        ] {
            assert!(ProjectConfig::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    /// Risk (R12, #441): `[deploy]` steps run in declaration order with
    /// secret names only; misordered steps, multiline commands, or embedded
    /// secret values would run steps out of order or commit credentials.
    #[test]
    fn deploy_steps_preserve_order_and_validate() {
        let cfg = ProjectConfig::parse(
            "[deploy.zeta]\n\
             command = \"deploy-zeta\"\n\
             paths = [\"server/\"]\n\
             secrets = [\"DEPLOY_TOKEN\", \"fly-key\"]\n\
             platforms = [\"linux\", \"linux/amd64\"]\n\
             [deploy.alpha]\n\
             command = \"deploy-alpha\"\n",
        )
        .unwrap();
        let names: Vec<&str> = cfg.deploy.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["zeta", "alpha"]);
        assert_eq!(
            cfg.deploy[0].paths,
            vec![PathPattern::new("server/**").unwrap()]
        );
        assert!(cfg.deploy[0].matches_any(&["server/src/main.rs".into()]));
        assert!(!cfg.deploy[0].matches_any(&["docs/readme.md".into()]));
        assert!(cfg.deploy[1].matches_any(&["docs/readme.md".into()]));

        for bad in [
            "[deploy.server]\ncommand = \"\"\n",
            "[deploy.server]\ncommand = \"a\\nb\"\n",
            "[deploy.server]\ncommand = \"ok\"\npaths = [\"../escape\"]\n",
            "[deploy.server]\ncommand = \"ok\"\nsecrets = [\"TOKEN=supersecret\"]\n",
            "[deploy.server]\ncommand = \"ok\"\nsecrets = [\"Bearer secret\"]\n",
            "[deploy.server]\ncommand = \"ok\"\n[deploy.server.secrets]\nTOKEN = \"val\"\n",
            "[deploy.server]\ncommand = \"ok\"\nplatforms = [\"-flag\"]\n",
            "[deploy.server]\ncommand = \"ok\"\nplatforms = [\"linux macos\"]\n",
            "[deploy.server]\ncommand = \"ok\"\nextra = 1\n",
        ] {
            assert!(ProjectConfig::parse(bad).is_err(), "accepted {bad:?}");
        }
        let err = ProjectConfig::parse(
            "[deploy.server]\ncommand = \"ok\"\nsecrets = [\"TOKEN=supersecret\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(!err.contains("supersecret"), "{err}");
    }
}
