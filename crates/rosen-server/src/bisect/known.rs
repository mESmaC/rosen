//! Search past recorded build, lint and test culprits without calling a broken prefix green.
use rosen_core::ObjectId;
use rosen_protocol::{CheckKind, ErrorBody};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// One earlier confirmed culprit whose failure may still appear on commits
/// before its fix or revert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct KnownEntry {
    pub output: String,
    pub tests: BTreeSet<String>,
    /// Commit index in the session's `commits` where this culprit's revert landed;
    /// `None` means no recorded revert commit bounds it (e.g. fix-forward test culprit
    /// or pending/refused build revert).
    pub until: Option<usize>,
}

/// Known earlier culprits captured when a bisection session opens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct KnownFailures {
    entries: Vec<KnownEntry>,
}

/// Unbound known culprits queried from history before `commits` is materialized.
pub(super) struct KnownContext {
    raw: Vec<(String, Option<ObjectId>)>,
}

impl KnownContext {
    /// Binds each culprit's optional revert commit to its index in `commits`.
    pub fn bind(self, commits: &[(ObjectId, Option<String>)]) -> KnownFailures {
        let mut entries = Vec::new();
        for (output, reverted_by) in self.raw {
            let until = match reverted_by {
                None => None,
                Some(revert_commit) => {
                    match commits.iter().position(|(id, _)| *id == revert_commit) {
                        // Active only strictly before its revert commit.
                        Some(idx) if idx > 0 => Some(idx),
                        // Already reverted at or before the lower bound.
                        _ => continue,
                    }
                }
            };
            let tests = parse_failing_tests(&output);
            entries.push(KnownEntry {
                output,
                tests,
                until,
            });
        }
        KnownFailures { entries }
    }
}

impl KnownFailures {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn from_stored(raw: Option<String>) -> Self {
        let Some(raw) = raw else {
            return Self::default();
        };
        if let Some(entries) = parse_envelope(&raw) {
            return Self { entries };
        }
        if raw.trim().is_empty() {
            return Self::default();
        }
        let tests = parse_failing_tests(&raw);
        Self {
            entries: vec![KnownEntry {
                output: raw,
                tests,
                until: None,
            }],
        }
    }

    pub fn to_stored(&self) -> Result<Option<String>, ErrorBody> {
        if self.is_empty() {
            return Ok(None);
        }
        let items: Vec<Value> = self
            .entries
            .iter()
            .map(|entry| {
                let mut obj = json!({
                    "output": entry.output,
                    "tests": entry.tests,
                });
                if let Some(until) = entry.until {
                    obj["until"] = json!(until);
                }
                obj
            })
            .collect();
        let envelope = json!({ "rosen_known": 1, "entries": items });
        serde_json::to_string(&envelope)
            .map(Some)
            .map_err(super::internal)
    }

    /// True when `output` at commit index `index` is fully explained by earlier
    /// known culprits active at `index`.
    pub fn matches_at(&self, index: usize, output: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        let failing = parse_failing_tests(output);
        if !failing.is_empty() {
            let known_tests = self.active_tests(index);
            return !known_tests.is_empty() && failing.is_subset(&known_tests);
        }
        let trimmed = output.trim();
        !trimmed.is_empty()
            && self
                .active(index)
                .any(|entry| entry.output.trim() == trimmed)
    }

    /// Failing test names parsed from `output` that differ from every active
    /// known culprit's failing tests at `index`.
    pub fn unfamiliar_tests(&self, index: usize, output: &str) -> Vec<String> {
        let known_tests = self.active_tests(index);
        parse_failing_tests(output)
            .difference(&known_tests)
            .cloned()
            .collect()
    }

    fn active(&self, index: usize) -> impl Iterator<Item = &KnownEntry> {
        self.entries
            .iter()
            .filter(move |entry| entry.until.is_none_or(|until| index < until))
    }

    fn active_tests(&self, index: usize) -> BTreeSet<String> {
        self.active(index)
            .flat_map(|entry| entry.tests.iter().cloned())
            .collect()
    }
}

fn parse_envelope(raw: &str) -> Option<Vec<KnownEntry>> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let obj = value.as_object()?;
    if obj.get("rosen_known")?.as_u64()? != 1 {
        return None;
    }
    let items = obj.get("entries")?.as_array()?;
    let mut entries = Vec::with_capacity(items.len());
    for item in items {
        let map = item.as_object()?;
        let output = map.get("output")?.as_str()?.to_owned();
        let tests = map
            .get("tests")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_else(|| parse_failing_tests(&output));
        let until = map
            .get("until")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok());
        entries.push(KnownEntry {
            output,
            tests,
            until,
        });
    }
    Some(entries)
}

/// Extracts failing test names from Cargo / Rust `libtest` output:
/// `---- <name> stdout ----` headers and `failures:` summary blocks.
pub(crate) fn parse_failing_tests(output: &str) -> BTreeSet<String> {
    let mut tests = BTreeSet::new();
    let mut in_failures = false;
    for raw in output.lines() {
        let line = raw.trim_end();
        if let Some(name) = parse_stdout_header(line.trim()) {
            in_failures = false;
            tests.insert(name.to_owned());
            continue;
        }
        if line == "failures:" {
            in_failures = true;
            continue;
        }
        if in_failures {
            if line.trim().is_empty() {
                in_failures = false;
            } else if line.starts_with(' ') || line.starts_with('\t') {
                let name = line.trim();
                if !name.is_empty()
                    && !name.starts_with("---- ")
                    && !name.starts_with("test result:")
                {
                    tests.insert(name.to_owned());
                }
            } else {
                in_failures = false;
            }
        }
    }
    tests
}

fn parse_stdout_header(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("---- ")?;
    let name = rest.strip_suffix(" stdout ----")?.trim();
    (!name.is_empty()).then_some(name)
}

/// Computes the lower boundary and earlier known culprits in `[green, failed)`
/// for both build/lint and test checks.
pub(super) fn context(
    conn: &Connection,
    name: &str,
    green: ObjectId,
    failed: ObjectId,
    kind: CheckKind,
    platform: Option<&str>,
) -> Result<(ObjectId, KnownContext), ErrorBody> {
    let platform = platform.unwrap_or("");
    crate::post_land::failures::ensure_schema(conn).map_err(super::internal)?;
    let bound = boundary(conn, name, green, failed, kind, platform)?;
    let raw = culprits(conn, name, green, failed, kind, platform)?;
    Ok((bound, KnownContext { raw }))
}

fn boundary(
    conn: &Connection,
    name: &str,
    green: ObjectId,
    failed: ObjectId,
    kind: CheckKind,
    platform: &str,
) -> Result<ObjectId, ErrorBody> {
    let found: Option<String> = conn
        .query_row(
            "SELECT h.commit_id FROM base_history h WHERE h.ref_name=?1 AND
            (h.commit_id=?2 OR (h.seq < (SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?3)
            AND (EXISTS(SELECT 1 FROM post_land_failures f WHERE f.commit_id=h.commit_id AND f.kind=?4 AND f.platform=?5)
                OR EXISTS(SELECT 1 FROM bisections b WHERE b.ref_name=?1 AND b.failed=h.commit_id
                    AND b.kind=?4 AND b.platform=?5 AND b.phase='known'))))
            ORDER BY h.seq DESC LIMIT 1",
            params![name, green.to_hex(), failed.to_hex(), kind.as_str(), platform],
            |r| r.get(0),
        )
        .optional()
        .map_err(super::internal)?;
    match found {
        Some(hex) => hex.parse().map_err(super::internal),
        None => Ok(green),
    }
}

fn culprits(
    conn: &Connection,
    name: &str,
    green: ObjectId,
    failed: ObjectId,
    kind: CheckKind,
    platform: &str,
) -> Result<Vec<(String, Option<ObjectId>)>, ErrorBody> {
    let has_auto_reverts: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='auto_reverts')",
            [],
            |r| r.get(0),
        )
        .map_err(super::internal)?;
    let revert_expr = if has_auto_reverts {
        "COALESCE(f.revert, (SELECT a.commit_id FROM auto_reverts a WHERE a.culprit=f.commit_id LIMIT 1))"
    } else {
        "f.revert"
    };
    let sql = format!(
        "SELECT f.output, {revert_expr}
         FROM post_land_failures f
         JOIN base_history h ON h.ref_name=?1 AND h.commit_id=f.commit_id
         WHERE f.kind=?4 AND f.platform=?5
         AND length(trim(f.output)) > 0
         AND h.seq >= (SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?2)
         AND h.seq < (SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?3)
         ORDER BY h.seq DESC"
    );
    let mut stmt = conn.prepare(&sql).map_err(super::internal)?;
    let rows = stmt
        .query_map(
            params![
                name,
                green.to_hex(),
                failed.to_hex(),
                kind.as_str(),
                platform
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .map_err(super::internal)?;
    let mut entries = Vec::new();
    for row in rows {
        let (output, revert) = row.map_err(super::internal)?;
        let revert = revert
            .map(|hex| hex.parse::<ObjectId>().map_err(super::internal))
            .transpose()?;
        entries.push((output, revert));
    }
    Ok(entries)
}
