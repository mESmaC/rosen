//! Post-land events and, when the project opts in, channel lines, read
//! straight from the R9 log for the sequence window a pass covers (#406).
//!
//! Each line goes once, keyed by its own sequence, with the time the log
//! stored it. Details are trimmed to what Collapse shows: a conflict names
//! its paths and base but no file content, a mirror report leaves out the
//! remote, and check output keeps only its last 64 KiB.

use std::path::Path;

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use super::unit::{Section, Unit};

/// The event kinds the feed sends as lines.
pub const SENT_KINDS: [&str; 8] = [
    "conflict",
    "built",
    "green",
    "reverted",
    "revert_refused",
    "post_land_failure",
    "bisect",
    "mirror",
];
/// The most bytes of check output a line carries, from its end.
const OUTPUT_TAIL: usize = 64 * 1024;
/// The channel line R13 writes for a ledger entry; the ledger push sends
/// those, so the feed never does.
const LEDGER_LINE: &str = "ledger";

/// Every sendable line with a sequence in `(after, through]`, oldest first.
/// Channel lines are included only when `channel` is set. Reads SQLite:
/// call it on the blocking pool.
pub fn read(
    metadata: &Path,
    after: u64,
    through: u64,
    channel: bool,
) -> rusqlite::Result<Vec<Unit>> {
    if through <= after {
        return Ok(Vec::new());
    }
    let conn = crate::metadata::open(metadata)?;
    let mut kinds: Vec<&str> = SENT_KINDS.to_vec();
    if channel {
        kinds.push("channel");
    }
    let rows = rows(&conn, after, through, &kinds)?;
    Ok(rows.into_iter().filter_map(line).collect())
}

/// One stored event row: sequence, kind, change, detail and time.
type Row = (u64, String, Option<String>, String, i64);

fn rows(conn: &Connection, after: u64, through: u64, kinds: &[&str]) -> rusqlite::Result<Vec<Row>> {
    // The kinds are this file's constants, never input, so they can be
    // spelled into the statement.
    let listed: Vec<String> = kinds.iter().map(|kind| format!("'{kind}'")).collect();
    let sql = format!(
        "SELECT seq, kind, change_id, detail, created_at FROM events
         WHERE seq > ?1 AND seq <= ?2 AND kind IN ({}) ORDER BY seq",
        listed.join(", ")
    );
    let mut statement = conn.prepare_cached(&sql)?;
    let from = i64::try_from(after).unwrap_or(i64::MAX);
    let to = i64::try_from(through).unwrap_or(i64::MAX);
    let rows = statement.query_map(params![from, to], |row| {
        Ok((
            u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    })?;
    rows.collect()
}

/// The unit for one row, or `None` for a ledger-entry channel line.
fn line((seq, kind, change, detail, time): Row) -> Option<Unit> {
    let detail: Value = serde_json::from_str(&detail).unwrap_or(Value::Null);
    if kind == "channel" {
        if detail.get("line").and_then(Value::as_str) == Some(LEDGER_LINE) {
            return None;
        }
        let item = json!({ "seq": seq, "detail": detail, "time": time });
        return Some(Unit::log(seq, Section::Channel, item));
    }
    let item = json!({
        "seq": seq,
        "kind": kind,
        "change": change,
        "detail": trimmed(&kind, detail),
        "time": time,
    });
    Some(Unit::log(seq, Section::Events, item))
}

/// The detail Collapse may hold for an event of `kind`.
fn trimmed(kind: &str, detail: Value) -> Value {
    let Value::Object(mut map) = detail else {
        return detail;
    };
    match kind {
        "conflict" => map.retain(|key, _| key == "paths" || key == "onto"),
        "mirror" => {
            map.remove("remote");
        }
        "post_land_failure" | "reverted" => cap_output(&mut map),
        _ => {}
    }
    Value::Object(map)
}

/// Keeps the last [`OUTPUT_TAIL`] bytes of `output`, where a failure's
/// cause usually is, and says whether anything was cut.
fn cap_output(map: &mut Map<String, Value>) {
    let Some(Value::String(output)) = map.get("output") else {
        map.insert("truncated".into(), Value::Bool(false));
        return;
    };
    let (kept, truncated) = tail(output, OUTPUT_TAIL);
    let kept = kept.to_owned();
    map.insert("output".into(), Value::String(kept));
    map.insert("truncated".into(), Value::Bool(truncated));
}

/// The longest suffix of `text` within `limit` bytes that starts on a
/// character boundary.
fn tail(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (&text[start..], true)
}
