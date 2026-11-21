//! Request bodies for the state endpoint, each under its 2 MiB bound.
//!
//! A body is `{"format":1,"through":N,"changes":[...],"conflicts":[...],
//! "threads":[...],"coverage":[...],"config":[...],"events":[...],
//! "machines":[...],"presence":[...],"channel":[...]}`. Every item carries
//! `seq`, the event sequence it reflects, so the endpoint keeps the newer of
//! two copies and a repeated batch changes nothing. An event or channel
//! line keeps its own sequence, which is its key. `through` is the
//! sequence the endpoint is complete through once it stores the batch: the
//! pass's sequence on its last batch, and what was already delivered on the
//! others, so a pass cut short never claims more than arrived.
//!
//! A body holding the relay document also carries `"relay":{"allow":[...]}`,
//! the action kinds the server runs (R14). Collapse keeps the newest it
//! was sent, so a body without it changes nothing.

use serde_json::{json, Map, Value};

use super::unit::{Delivery, Section, Unit};
use crate::ledger_push::MAX_BODY;

/// The format Collapse reads; it refuses one it does not know.
const FORMAT: u64 = 1;
/// Room left for the envelope around the items.
const ENVELOPE: usize = 256;
const SECTIONS: [Section; 9] = [
    Section::Changes,
    Section::Conflicts,
    Section::Threads,
    Section::Coverage,
    Section::Config,
    Section::Events,
    Section::Machines,
    Section::Presence,
    Section::Channel,
];

/// One stamped, sized item inside a batch, tied back to its source unit so
/// halving on HTTP 400/413/422 can name and skip the refused document (#444).
#[derive(Clone, Debug)]
pub struct BatchItem {
    pub section: Section,
    pub value: Value,
    pub unit: usize,
}

/// One request and the items it carries.
pub struct Batch {
    pub body: Vec<u8>,
    pub items: Vec<BatchItem>,
}

/// The body that sends nothing, asking only what the endpoint holds.
pub fn probe() -> Vec<u8> {
    encode(0, &[])
}

/// Splits `units` into size-bounded batches.
#[cfg(test)]
pub fn split(units: Vec<Unit>, seq: u64, delivered: u64) -> Vec<Batch> {
    split_slice(&units, seq, delivered)
}

/// Splits `units` into size-bounded batches, recording each item's index in
/// `units` so the sender can track unit completion across split sub-batches.
pub fn split_slice(units: &[Unit], seq: u64, delivered: u64) -> Vec<Batch> {
    let mut batches = Vec::new();
    let mut items: Vec<BatchItem> = Vec::new();
    let mut size = ENVELOPE;
    for (unit_index, unit) in units.iter().enumerate() {
        let logged = unit.delivery == Delivery::Log;
        for (section, item) in &unit.items {
            let item = if logged || *section == Section::Relay {
                item.clone()
            } else {
                stamped(item.clone(), seq)
            };
            let item = sized(item);
            let length = serde_json::to_vec(&item).map_or(0, |bytes| bytes.len()) + 1;
            if size + length > MAX_BODY && !items.is_empty() {
                batches.push(Batch {
                    body: encode(delivered, &items),
                    items: std::mem::take(&mut items),
                });
                size = ENVELOPE;
            }
            size += length;
            items.push(BatchItem {
                section: *section,
                value: item,
                unit: unit_index,
            });
        }
    }
    // Every unit holds an item, so `items` is empty only when `units` was.
    if !items.is_empty() {
        batches.push(Batch {
            body: encode(seq, &items),
            items,
        });
    }
    batches
}

fn stamped(mut item: Value, seq: u64) -> Value {
    if let Value::Object(map) = &mut item {
        map.insert("seq".into(), json!(seq));
    }
    item
}

/// An item too large for any batch is replaced by a marker naming it, so
/// the rest of the state still arrives.
fn sized(item: Value) -> Value {
    let length = serde_json::to_vec(&item).map_or(0, |bytes| bytes.len());
    if length + ENVELOPE <= MAX_BODY {
        return item;
    }
    let mut marker = Map::new();
    for key in ["id", "change", "kind", "seq"] {
        if let Some(value) = item.get(key) {
            marker.insert(key.into(), value.clone());
        }
    }
    marker.insert("too_large".into(), Value::Bool(true));
    Value::Object(marker)
}

/// Encodes `items` as a v1 state-feed JSON batch claiming completeness
/// through `through`.
pub fn encode(through: u64, items: &[BatchItem]) -> Vec<u8> {
    let mut envelope = Map::new();
    envelope.insert("format".into(), json!(FORMAT));
    envelope.insert("through".into(), json!(through));
    for section in SECTIONS {
        let listed: Vec<&Value> = items
            .iter()
            .filter(|item| item.section == section)
            .map(|item| &item.value)
            .collect();
        envelope.insert(section.key().into(), json!(listed));
    }
    if let Some(relay) = items.iter().find(|item| item.section == Section::Relay) {
        envelope.insert(Section::Relay.key().into(), relay.value.clone());
    }
    if let Some(platforms) = items.iter().find(|item| item.section == Section::Platforms) {
        envelope.insert(Section::Platforms.key().into(), platforms.value.clone());
    }
    serde_json::to_vec(&Value::Object(envelope)).unwrap_or_default()
}
