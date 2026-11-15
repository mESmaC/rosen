//! Build-unit reach and independence (SPEC §2, R8). Unknown inputs reach all
//! units, including a sentinel when the project has no discovered units.
use rosen_core::claim::PathPattern;
use rosen_core::project_config::BuildUnit;
use std::collections::{BTreeMap, BTreeSet};

pub type Reach = BTreeSet<String>;

#[derive(PartialEq, Eq)]
pub struct BuildGraph {
    pub units: BTreeMap<String, BuildUnit>,
    pub workspace_inputs: Vec<PathPattern>,
}

impl BuildGraph {
    pub fn reach(&self, paths: &[String], prose: bool) -> Reach {
        if prose {
            return Reach::new();
        }
        let mut reached = Reach::new();
        for path in paths {
            let holders: Reach = self
                .units
                .iter()
                .filter(|(_, unit)| unit.paths.iter().any(|p| p.matches(path)))
                .map(|(name, _)| name.clone())
                .collect();
            if self.wide(path) || !self.valid() {
                return self.all();
            }
            // Mirror workflows affect builds only when a project names them
            // as inputs or includes them in a build unit (#367).
            if holders.is_empty() && !path.starts_with(".github/workflows/") {
                return self.all();
            }
            reached.extend(holders);
        }
        self.expand(reached)
    }

    pub(super) fn expand(&self, mut reached: Reach) -> Reach {
        if !self.valid() {
            return self.all();
        }
        loop {
            let before = reached.len();
            for (name, unit) in &self.units {
                if unit.dependencies.iter().any(|d| reached.contains(d)) {
                    reached.insert(name.clone());
                }
            }
            if reached.len() == before {
                return reached;
            }
        }
    }

    fn valid(&self) -> bool {
        self.units
            .values()
            .all(|u| u.dependencies.iter().all(|d| self.units.contains_key(d)))
    }

    pub(super) fn all(&self) -> Reach {
        self.units.keys().cloned().chain([String::new()]).collect()
    }

    pub(super) fn wide(&self, path: &str) -> bool {
        matches!(path, "rust-toolchain" | "rust-toolchain.toml")
            || path.starts_with(".cargo/")
            || self.workspace_inputs.iter().any(|p| p.matches(path))
    }
}

/// One membership probe per unit in the smaller reach, stopping at the first
/// shared unit. The Lean model proves this is the maximal safe reuse rule.
pub fn independent(left: &Reach, right: &Reach) -> bool {
    left.is_disjoint(right)
}
