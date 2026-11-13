//! The run plan every machine shares, and the deterministic layout of the
//! edits it makes.
//!
//! Every machine runs with the same plan and its own slice of agent
//! indexes, so the seed repository, each agent's paths and every expected
//! line can be computed anywhere, including by `verify` afterwards.

use clap::Args;
use serde::{Deserialize, Serialize};

/// What one stress run does, identical on every machine.
#[derive(Debug, Clone, Args, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plan {
    /// Names this run. Agent nicks and edit contents carry it, so runs on
    /// one project never mistake each other's work.
    #[arg(long, default_value = "stress")]
    pub run: String,
    /// Agents across every machine.
    #[arg(long, default_value_t = 100)]
    pub agents: usize,
    /// Lands each agent makes. 100 agents × 10 lands meets §5's 1000.
    #[arg(long, default_value_t = 10)]
    pub lands_per_agent: usize,
    /// Build units in the seed repository. Agents spread across them, so
    /// changes in different units are independent and reuse receipts.
    #[arg(long, default_value_t = 20)]
    pub units: usize,
    /// Every Nth change of an agent is stacked on its previous one (0: none).
    #[arg(long, default_value_t = 4)]
    pub stack_every: usize,
    /// Every Nth change also tries to claim its unit's contended path
    /// (0: never), so first-claim-wins is exercised under load.
    #[arg(long, default_value_t = 3)]
    pub hot_every: usize,
    /// One change of the last agent also adds a file that fails the
    /// post-land check, so the run sees it bisected and reverted (R12).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub inject_failure: bool,
}

/// Lines of padding between slots in a unit's shared file. More than the
/// 3-line overlap distance (§2), so edits to different slots never overlap
/// and always merge cleanly.
const SLOT_STRIDE: usize = 5;

impl Plan {
    /// The flags that give a child `rosen-stress` process this plan.
    pub fn to_args(&self) -> Vec<String> {
        let flags = [
            ("--run", self.run.clone()),
            ("--agents", self.agents.to_string()),
            ("--lands-per-agent", self.lands_per_agent.to_string()),
            ("--units", self.units.to_string()),
            ("--stack-every", self.stack_every.to_string()),
            ("--hot-every", self.hot_every.to_string()),
            ("--inject-failure", self.inject_failure.to_string()),
        ];
        flags
            .into_iter()
            .flat_map(|(flag, value)| [flag.to_owned(), value])
            .collect()
    }

    pub fn unit_of(&self, agent: usize) -> usize {
        agent % self.units
    }

    pub fn unit_dir(unit: usize) -> String {
        format!("u{unit:03}")
    }

    /// The file every change in a unit edits one line of: the shared
    /// file whose merges §5's "no edit is lost" is mostly about.
    pub fn slots_path(unit: usize) -> String {
        format!("{}/slots.rs", Self::unit_dir(unit))
    }

    /// The path several agents in a unit race to claim.
    pub fn hot_path(unit: usize) -> String {
        format!("{}/hot.rs", Self::unit_dir(unit))
    }

    /// The new file change `n` of `agent` adds; it claims it too.
    pub fn own_path(&self, agent: usize, n: usize) -> String {
        let unit = Self::unit_dir(self.unit_of(agent));
        format!("{unit}/agents/{}/{n:04}.rs", self.agent_name(agent))
    }

    pub fn agents_per_unit(&self) -> usize {
        self.agents.div_ceil(self.units)
    }

    pub fn slots_per_unit(&self) -> usize {
        self.agents_per_unit() * self.lands_per_agent
    }

    /// The slot change `n` of `agent` rewrites, unique within its unit.
    pub fn slot_of(&self, agent: usize, n: usize) -> usize {
        agent / self.units + self.agents_per_unit() * n
    }

    pub fn slot_line_index(slot: usize) -> usize {
        slot * SLOT_STRIDE
    }

    /// The seed's text for slot `slot`, before any change rewrites it.
    pub fn open_slot_line(slot: usize) -> String {
        format!("slot {slot:06}: open")
    }

    /// The seed's text for one unit's shared file.
    pub fn seed_slots_file(&self) -> String {
        let mut text = String::new();
        for slot in 0..self.slots_per_unit() {
            text.push_str(&Self::open_slot_line(slot));
            text.push('\n');
            for pad in 1..SLOT_STRIDE {
                text.push_str(&format!("  pad {slot:06}.{pad}\n"));
            }
        }
        text
    }

    /// The text an agent writes into its slot and its own file. It names
    /// the run, agent and change number, so `verify` can find it again.
    pub fn edit_line(&self, agent: usize, n: usize) -> String {
        format!(
            "slot {:06}: {} change {n}",
            self.slot_of(agent, n),
            self.agent_name(agent)
        )
    }

    pub fn agent_name(&self, agent: usize) -> String {
        format!("{}-a{agent:03}", self.run)
    }

    pub fn reviewer_name(&self, machine: &str) -> String {
        format!("{}-{machine}-reviewer", self.run)
    }

    pub fn checker_name(&self, machine: &str) -> String {
        format!("{}-{machine}-checker", self.run)
    }

    /// Whether change `n` sits on the agent's previous change instead of
    /// the base.
    pub fn is_stacked(&self, n: usize) -> bool {
        self.stack_every > 0 && n % self.stack_every == self.stack_every - 1
    }

    /// The agent and change number that carry the injected failure. It is
    /// a change halfway through, so landed history surrounds it, and one
    /// with no change stacked on it, so the revert reopens only it.
    pub fn culprit(&self) -> Option<(usize, usize)> {
        if !self.inject_failure || self.agents == 0 || self.lands_per_agent == 0 {
            return None;
        }
        let mut n = self.lands_per_agent / 2;
        while n + 1 < self.lands_per_agent && self.is_stacked(n + 1) {
            n += 1;
        }
        Some((self.agents - 1, n))
    }

    /// Where the culprit puts the failing file: in its own unit, so its
    /// change stays independent of the other units' changes.
    pub fn failure_path(&self) -> Option<String> {
        let (agent, _) = self.culprit()?;
        let unit = Self::unit_dir(self.unit_of(agent));
        Some(format!("{unit}/{}", rosen_faults::failing::FILE_NAME))
    }

    pub fn claims_hot(&self, n: usize) -> bool {
        self.hot_every > 0 && n.is_multiple_of(self.hot_every)
    }

    /// `rosen.toml` for the seed: one build unit per unit directory, so
    /// changes in different units are independent (§2, R8 step 2).
    pub fn seed_config(&self) -> String {
        let mut text = String::new();
        for unit in 0..self.units {
            let dir = Self::unit_dir(unit);
            text.push_str(&format!("[build_units.{dir}]\npaths = ['{dir}/**']\n\n"));
        }
        text
    }
}
