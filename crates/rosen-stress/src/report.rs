//! One machine's measurements, written as JSON so `verify` can merge the
//! reports of every machine in the run, and the journal they are built
//! from, so a killed `agents` process resumes where it stopped (§5).

use crate::plan::Plan;
use anyhow::{Context, Result};
use rosen_client::{ChangeId, ObjectId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub plan: Option<Plan>,
    pub machine: String,
    /// The agent indexes this machine ran, `first..first + count`.
    pub first_agent: usize,
    pub agent_count: usize,
    /// The base tip when this machine's agents first started. `verify`
    /// walks the base back to the oldest of these.
    pub base_at_start: Option<ObjectId>,
    /// Time the agents ran, summed over every start of the process.
    pub elapsed_ms: u64,
    /// Times the `agents` process started again after being killed.
    pub restarts: u32,
    pub workspace_starts: Vec<StartSample>,
    pub lands: Vec<LandSample>,
    /// Every receipt the machine's checker submitted (R8 step 1).
    pub receipts: Vec<ReceiptSample>,
    pub changes: Vec<ChangeOutcome>,
    /// Two agents seen holding one claimed path at once.
    pub claim_violations: Vec<String>,
    /// Hot-path claims granted and refused, to show contention happened.
    pub hot_claims_granted: u64,
    pub hot_claims_refused: u64,
    /// Land refusals by reason code, including ones the agent recovered
    /// from by re-receipting.
    pub land_refusals: BTreeMap<String, u64>,
    /// Workspaces of closed changes the daemon had not deleted when the
    /// run ended (R4 deletes them automatically).
    pub workspaces_kept: Vec<String>,
    /// Requests that failed and were retried or abandoned.
    pub errors: Vec<String>,
}

/// Creating a change and its workspace (R3), as one agent waited for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSample {
    pub ms: f64,
    /// `warm_pool`, `block_clone`, `copy` or `mixed`.
    pub method: String,
}

/// One land request that moved the base, with the receipt it was given
/// just before for the exact head and base (R8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LandSample {
    pub change: ChangeId,
    pub ms: f64,
    /// Compare-and-swap attempts inside the server.
    pub attempts: u32,
    /// Land requests the agent repeated after a fresh receipt.
    pub rechecks: u32,
}

/// A receipt the run's checker submitted, so `verify` can match each
/// landed commit's receipt trailer to one the run really gave.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptSample {
    pub change: ChangeId,
    pub head: ObjectId,
    pub base: ObjectId,
}

/// What became of one change, and the lines that must be in the final
/// base if it landed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeOutcome {
    pub change: ChangeId,
    pub agent: String,
    /// The agent's index and the change's number, so a restarted agent
    /// knows which of its changes are finished.
    pub index: usize,
    pub n: usize,
    pub stacked_on: Option<ChangeId>,
    /// `(path, line)`: the file must hold this exact line.
    pub expected: Vec<(String, String)>,
    /// The change's local workspace on its machine.
    pub workspace: PathBuf,
    pub landed: Option<ObjectId>,
    pub failure: Option<String>,
}

/// A change an agent created, recorded before it edits or claims, so a
/// restarted agent carries the same change on instead of making another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Began {
    pub index: usize,
    pub n: usize,
    pub change: ChangeId,
    pub root: PathBuf,
    pub stacked_on: Option<ChangeId>,
    pub expected: Vec<(String, String)>,
}

/// One line of a machine's journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
    /// The `agents` process started, with the base tip it saw.
    Started { base: ObjectId },
    Began(Began),
    Start(StartSample),
    Land(LandSample),
    Receipt(ReceiptSample),
    Outcome(ChangeOutcome),
    Refusal { reason: String },
    HotClaim { granted: bool },
    Violation { line: String },
    Error { line: String },
    /// The process finished its agents after running this long.
    Ran { ms: u64 },
}

impl Report {
    /// Folds one journal record into the report.
    pub fn apply(&mut self, record: &Record) {
        match record {
            Record::Started { base } => {
                self.restarts += u32::from(self.base_at_start.is_some());
                self.base_at_start.get_or_insert(*base);
            }
            Record::Began(_) => {}
            Record::Start(sample) => self.workspace_starts.push(sample.clone()),
            Record::Land(sample) => self.lands.push(sample.clone()),
            Record::Receipt(sample) => self.receipts.push(sample.clone()),
            Record::Outcome(outcome) => self.changes.push(outcome.clone()),
            Record::Refusal { reason } => {
                *self.land_refusals.entry(reason.clone()).or_default() += 1;
            }
            Record::HotClaim { granted: true } => self.hot_claims_granted += 1,
            Record::HotClaim { granted: false } => self.hot_claims_refused += 1,
            Record::Violation { line } => self.claim_violations.push(line.clone()),
            Record::Error { line } => self.errors.push(line.clone()),
            Record::Ran { ms } => self.elapsed_ms += ms,
        }
    }
}

/// The append-only file a machine's records go to as they happen.
pub struct Journal {
    file: std::fs::File,
}

impl Journal {
    /// Opens `path`, creating it, and returns the records already in it.
    /// A process killed mid-write can leave a partial last line: it is
    /// skipped and closed with a newline, so the next record starts a line
    /// of its own.
    pub fn open(path: &Path) -> Result<(Self, Vec<Record>)> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err).with_context(|| path.display().to_string()),
        };
        let records = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| path.display().to_string())?;
        if !text.is_empty() && !text.ends_with('\n') {
            file.write_all(b"\n")?;
        }
        Ok((Self { file }, records))
    }

    /// Appends one record in a single write, so a kill never leaves half
    /// of it in front of the next.
    pub fn append(&mut self, record: &Record) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(record).expect("records serialize");
        line.push(b'\n');
        self.file.write_all(&line)
    }
}

/// The nearest-rank percentile, so a reported p95 is a real sample.
pub fn percentile(samples: &[f64], fraction: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}
