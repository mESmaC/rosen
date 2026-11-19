//! Request bodies for the state endpoint, each under its 2 MiB bound.
//!
//! A body is `{"format":1,"through":N,"changes":[...],"conflicts":[...],
//! "threads":[...],"coverage":[...],"config":[...]}`. Every item carries
//! `seq`, the event sequence it reflects, so the endpoint keeps the newer of
//! two copies and a repeated batch changes nothing. `through` is the
//! sequence the endpoint is complete through once it stores the batch: the
//! pass's sequence on its last batch, and what was already delivered on the
//! others, so a pass cut short never claims more than arrived.

use serde_json::{json, Map, Value};

use super::store::Sent;
use super::unit::{Section, Unit};
use crate::ledger_push::MAX_BODY;

/// The format Collapse reads; it refuses one it does not know.
const FORMAT: u64 = 1;
/// Room left for the envelope around the items.
const ENVELOPE: usize = 256;
const SECTIONS: [Section; 5] = [
    Section::Changes,
    Section::Conflicts,
    Section::Threads,
    Section::Coverage,
    Section::Config,
];

/// One request and the documents it completes.
pub struct Batch {
    pub body: Vec<u8>,
    pub documents: Vec<(String, Sent)>,
}

/// The body that sends nothing, asking only what the endpoint holds.
pub fn probe() -> Vec<u8> {
    body(0, &[])
}

/// Splits `units` into batches. A unit's document is recorded with the
/// batch holding its last item, so it only counts as delivered once all
/// of it has been.
pub fn split(units: Vec<Unit>, seq: u64, delivered: u64) -> Vec<Batch> {
    let mut batches = Vec::new();
    let mut items: Vec<(Section, Value)> = Vec::new();
    let mut documents = Vec::new();
    let mut size = ENVELOPE;
    for unit in units {
        for (section, item) in unit.items {
            let item = sized(stamped(item, seq));
            let length = serde_json::to_vec(&item).map_or(0, |bytes| bytes.len()) + 1;
            if size + length > MAX_BODY && !items.is_empty() {
                batches.push(Batch {
                    body: body(delivered, &items),
                    documents: std::mem::take(&mut documents),
                });
                items.clear();
                size = ENVELOPE;
            }
            size += length;
            items.push((section, item));
        }
        let sent = Sent {
            fingerprint: unit.fingerprint,
            related: unit.related,
        };
        documents.push((unit.document, sent));
    }
    // Every unit holds an item, so `items` is empty only when `units` was.
    if !items.is_empty() {
        batches.push(Batch {
            body: body(seq, &items),
            documents,
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
    for key in ["id", "change", "seq"] {
        if let Some(value) = item.get(key) {
            marker.insert(key.into(), value.clone());
        }
    }
    marker.insert("too_large".into(), Value::Bool(true));
    Value::Object(marker)
}

fn body(through: u64, items: &[(Section, Value)]) -> Vec<u8> {
    let mut envelope = Map::new();
    envelope.insert("format".into(), json!(FORMAT));
    envelope.insert("through".into(), json!(through));
    for section in SECTIONS {
        let listed: Vec<&Value> = items
            .iter()
            .filter(|(kind, _)| *kind == section)
            .map(|(_, item)| item)
            .collect();
        envelope.insert(section.key().into(), json!(listed));
    }
    serde_json::to_vec(&Value::Object(envelope)).unwrap_or_default()
}
