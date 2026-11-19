//! A document the feed sends as one: its items, the fingerprint that tells
//! whether the endpoint already holds it, and the changes it names.

use std::collections::BTreeSet;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Which list of a batch an item goes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Changes,
    Conflicts,
    Threads,
    Coverage,
    Config,
}

impl Section {
    pub fn key(self) -> &'static str {
        match self {
            Section::Changes => "changes",
            Section::Conflicts => "conflicts",
            Section::Threads => "threads",
            Section::Coverage => "coverage",
            Section::Config => "config",
        }
    }
}

/// One document and the items it sends. A change sends its conflicts and
/// threads as items of their own, so a large change splits across batches.
#[derive(Clone, Debug)]
pub struct Unit {
    /// `change:<id>`, `coverage:<id>` or `config`.
    pub document: String,
    pub fingerprint: String,
    /// The changes a change document names as overlapping it.
    pub related: BTreeSet<String>,
    pub items: Vec<(Section, Value)>,
}

impl Unit {
    /// A unit fingerprinted over its items.
    pub fn new(document: String, items: Vec<(Section, Value)>) -> Self {
        let mut hasher = Sha256::new();
        for (section, item) in &items {
            hasher.update(section.key().as_bytes());
            // serde_json's maps are sorted, so equal items hash equally.
            hasher.update(serde_json::to_vec(item).unwrap_or_default());
        }
        Self {
            document,
            fingerprint: hex(&hasher.finalize()),
            related: BTreeSet::new(),
            items,
        }
    }

    /// A unit fingerprinted over `basis` instead of its items, for a
    /// document carrying a field that changes without its meaning changing.
    pub fn with_basis(document: String, items: Vec<(Section, Value)>, basis: &Value) -> Self {
        let mut unit = Self::new(document, items);
        let digest = Sha256::digest(serde_json::to_vec(basis).unwrap_or_default());
        unit.fingerprint = hex(&digest);
        unit
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
