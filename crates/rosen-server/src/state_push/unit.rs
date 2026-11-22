//! A document the feed sends as one: its items, the fingerprint that tells
//! whether the endpoint already holds it, and the changes it names. Event
//! and channel lines from the log travel as units too, unfingerprinted.

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
    Events,
    Machines,
    Presence,
    Channel,
    /// The action kinds the relay runs (R14): one object, not a list.
    Relay,
    /// Per-platform post-land checkpoints and live checker coverage (R12, #440): one object.
    Platforms,
    /// Hourly fleet timing metrics (Collapse C12 wire key `"metrics"`, R9, #449, #465).
    Stats,
}

impl Section {
    pub fn key(self) -> &'static str {
        match self {
            Section::Changes => "changes",
            Section::Conflicts => "conflicts",
            Section::Threads => "threads",
            Section::Coverage => "coverage",
            Section::Config => "config",
            Section::Events => "events",
            Section::Machines => "machines",
            Section::Presence => "presence",
            Section::Channel => "channel",
            Section::Relay => "relay",
            Section::Platforms => "platforms",
            Section::Stats => "metrics",
        }
    }
}

/// How a unit decides whether it goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Delivery {
    /// Sent when its fingerprint differs from the one delivered.
    #[default]
    Document,
    /// Sent even when unchanged, so the endpoint sees how current it is,
    /// but only alongside something else: never in a request of its own.
    Refresh,
    /// A line from the event log: sent once, in sequence order, with its
    /// own `seq`, and never recorded, since it never changes.
    Log,
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
    pub delivery: Delivery,
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
            delivery: Delivery::Document,
        }
    }

    /// One line of the event log, keyed by its sequence.
    pub fn log(seq: u64, section: Section, item: Value) -> Self {
        Self {
            document: format!("log:{seq}"),
            fingerprint: String::new(),
            related: BTreeSet::new(),
            items: vec![(section, item)],
            delivery: Delivery::Log,
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
