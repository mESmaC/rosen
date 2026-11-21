//! Which ledger entries the push sends, and in what shape (R13).
//!
//! Only the `channel_ledger` table is read, so claims, messages, presence
//! and events can never reach the endpoint. Velocity keeps versioned
//! records (checkouts, task results, delivery acknowledgements) inside Past
//! work, written as `velocity-airc/...` envelopes or, in older builds, as
//! bare JSON. They are not ledger entries anyone reads, and the endpoint
//! refuses a whole batch holding one, so they are skipped. A discovery
//! Velocity wrapped in its `velocity-airc/discovery/v<n>:` envelope is
//! unwrapped into its text and issue title.
//!
//! The limits match Collapse's intake (its SPEC C6): a batch it refuses
//! stores nothing, and the same batch would be refused on every retry, so
//! fields are clipped to fit rather than sent whole.

use std::collections::BTreeSet;

use rosen_protocol::channel::Ledger;
use rusqlite::{params, Connection, Row};

use super::cursor::{Cursor, Cursors, Extent};

/// The most entries one batch holds.
pub const MAX_ENTRIES: usize = 1000;
/// The largest request body, kept under the endpoint's 2 MiB bound.
pub const MAX_BODY: usize = 2_000_000;
const MAX_TEXT: usize = 256 * 1024;
const MAX_NAME: usize = 256;
const MAX_TITLE: usize = 4096;

/// What every Velocity versioned record starts with.
const VERSIONED_PREFIX: &str = "velocity-airc/";
const DISCOVERY_ENVELOPE: &str = "velocity-airc/discovery/v";
/// Fields that mark a bare JSON record from older Velocity builds.
const LEGACY_RECORDS: [&str; 3] = ["checkout", "task_result", "delivery_ack"];

/// One encoded ledger entry ready to include in a request batch.
#[derive(Debug, Clone)]
pub struct BatchEntry {
    pub ledger: Ledger,
    pub seq: u64,
    pub json: Vec<u8>,
}

/// One request's worth of entries, in sequence order.
#[derive(Debug, Default)]
pub struct Batch {
    pub entries: Vec<BatchEntry>,
    /// The rows it read and the entries it sent, per ledger.
    pub extent: Extent,
}

impl Batch {
    /// True when it read no row, so the ledgers hold nothing new.
    pub fn is_empty(&self) -> bool {
        self.extent == Extent::default()
    }
}

/// A stored ledger row with its author's machine.
struct StoredEntry {
    seq: u64,
    ledger: Ledger,
    nick: String,
    machine: Option<String>,
    text: String,
    velocity_issue: Option<String>,
    created_at: i64,
}

/// The request body that sends no entries, which reads what the endpoint
/// holds.
pub fn empty_body() -> Vec<u8> {
    br#"{"entries":[]}"#.to_vec()
}

/// Encodes `entries` as `{"entries":[...]}`.
pub fn encode(entries: &[BatchEntry]) -> Vec<u8> {
    let mut body = br#"{"entries":["#.to_vec();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            body.push(b',');
        }
        body.extend_from_slice(&entry.json);
    }
    body.extend_from_slice(b"]}");
    body
}

/// The scanned and delivered sequences covered by `entries`.
pub fn extent_of(entries: &[BatchEntry]) -> Extent {
    let mut extent = Extent::default();
    for entry in entries {
        let cursor = extent.get_mut(entry.ledger);
        cursor.scanned = cursor.scanned.max(entry.seq);
        cursor.delivered = cursor.delivered.max(entry.seq);
    }
    extent
}

/// Reads the rows after each ledger's cursor, oldest first, and packs the
/// ones to send into one batch. Rows it skips (or that were previously
/// refused) still count as read.
pub fn next_batch(
    conn: &Connection,
    cursors: &Cursors,
    refused: &BTreeSet<(&'static str, u64)>,
) -> rusqlite::Result<Batch> {
    let rows = read_after(conn, cursors)?;
    let mut extent = Extent::default();
    let mut entries = Vec::new();
    let mut body_len = empty_body().len();
    for row in rows {
        let (seq, ledger) = (row.seq, row.ledger);
        if !refused.contains(&(ledger.as_str(), seq)) {
            if let Some(entry) = wire_entry(row) {
                let json = entry.to_string().into_bytes();
                // A row that does not fit waits for the next batch, unread. The
                // first always fits: clipping keeps one entry far under the bound.
                let full = body_len + json.len() + 1 > MAX_BODY && !entries.is_empty();
                if entries.len() == MAX_ENTRIES || full {
                    break;
                }
                body_len += json.len() + 1;
                entries.push(BatchEntry { ledger, seq, json });
                extent.get_mut(ledger).delivered = seq;
            }
        }
        extent.get_mut(ledger).scanned = seq;
    }
    Ok(Batch { entries, extent })
}

fn read_after(conn: &Connection, cursors: &Cursors) -> rusqlite::Result<Vec<StoredEntry>> {
    let (discovery, past_work) = (cursors.discovery, cursors.past_work);
    let from = |cursor: Cursor| i64::try_from(cursor.scanned).unwrap_or(i64::MAX);
    // The first bound lets SQLite walk the primary key from the lower of
    // the two cursors instead of the whole table.
    let mut stmt = conn.prepare_cached(
        "SELECT l.seq, l.ledger, l.nick, m.machine, l.text, l.velocity_issue, l.created_at
         FROM channel_ledger l LEFT JOIN channel_members m ON m.member = l.member
         WHERE l.seq > min(?1, ?2)
           AND ((l.ledger = 'discovery' AND l.seq > ?1)
             OR (l.ledger = 'past_work' AND l.seq > ?2))
         ORDER BY l.seq LIMIT ?3",
    )?;
    let limit = i64::try_from(MAX_ENTRIES).unwrap_or(i64::MAX);
    let rows = stmt.query_map(params![from(discovery), from(past_work), limit], read_row)?;
    rows.collect()
}

fn read_row(row: &Row) -> rusqlite::Result<StoredEntry> {
    let ledger: String = row.get(1)?;
    let ledger = Ledger::parse(&ledger).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            format!("unknown ledger {ledger:?}").into(),
        )
    })?;
    Ok(StoredEntry {
        seq: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
        ledger,
        nick: row.get(2)?,
        machine: row.get(3)?,
        text: row.get(4)?,
        velocity_issue: row.get(5)?,
        created_at: row.get(6)?,
    })
}

/// The entry to send for a stored row, as the endpoint receives it, or
/// `None` when it is skipped. Every field is required except the issue
/// titles, and a discovery carries at most one of those; the endpoint
/// refuses any other field.
fn wire_entry(row: StoredEntry) -> Option<serde_json::Value> {
    let column_issue = row.velocity_issue.filter(|title| !title.trim().is_empty());
    let (text, rosen_issue, velocity_issue) = match row.ledger {
        Ledger::Discovery => discovery_parts(row.text, column_issue)?,
        Ledger::PastWork if is_legacy_record(&row.text) => return None,
        Ledger::PastWork => (row.text, None, None),
    };
    if row.seq == 0 || text.trim_start().starts_with(VERSIONED_PREFIX) {
        return None;
    }
    let mut entry = serde_json::json!({
        "ledger": row.ledger.as_str(),
        "sequence": row.seq,
        "nick": clip(row.nick, MAX_NAME),
        "machine": clip(row.machine.unwrap_or_default(), MAX_NAME),
        "time": rfc3339_utc(row.created_at),
        "text": clip(text, MAX_TEXT),
    });
    let titles = [
        ("rosen_issue", rosen_issue),
        ("velocity_issue", velocity_issue),
    ];
    for (key, title) in titles {
        if let Some(title) = title {
            entry[key] = clip(title, MAX_TITLE).into();
        }
    }
    Some(entry)
}

type DiscoveryParts = (String, Option<String>, Option<String>);

/// A discovery's text and issue title, unwrapping Velocity's envelope. An
/// envelope this build cannot read is skipped, never sent as it is.
fn discovery_parts(text: String, column_issue: Option<String>) -> Option<DiscoveryParts> {
    let trimmed = text.trim_start();
    let Some(versioned) = trimmed.strip_prefix(DISCOVERY_ENVELOPE) else {
        return Some((text, None, column_issue));
    };
    let (version, json) = versioned.split_once(':')?;
    if version.is_empty() || !version.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let fields: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json).ok()?;
    let title = |key: &str| {
        let value = fields.get(key)?.as_str()?;
        (!value.trim().is_empty()).then(|| value.to_owned())
    };
    let body = fields.get("text")?.as_str()?.to_owned();
    let rosen_issue = title("rosen_issue");
    // The endpoint takes one title per discovery; a Rosen issue is the one
    // the agent chose for this entry.
    let velocity_issue = match rosen_issue {
        Some(_) => None,
        None => title("velocity_issue").or(column_issue),
    };
    Some((body, rosen_issue, velocity_issue))
}

/// A bare JSON object an older Velocity stored as a Past work record.
fn is_legacy_record(text: &str) -> bool {
    let Ok(record) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(text)
    else {
        return false;
    };
    let kind = record.get("kind").and_then(serde_json::Value::as_str);
    LEGACY_RECORDS
        .iter()
        .any(|field| record.contains_key(*field) || kind == Some(*field))
}

/// At most `max` bytes of `text`, cut at a character boundary.
fn clip(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` from Unix milliseconds (the civil-from-days
/// algorithm), so the push needs no date library.
pub fn rfc3339_utc(millis: i64) -> String {
    let seconds = millis.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60,
        millis.rem_euclid(1000)
    )
}
