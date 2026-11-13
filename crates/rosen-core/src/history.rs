//! Landed history (D15, R11). Each land writes one commit per change: the
//! change's final tree on top of the base, with trailers naming the change,
//! its receipt and its approvers. History queries walk the commit graph
//! through [`CommitGraph`], so the server can back it with its store or a
//! cached ancestry index.

use crate::change::{AgentId, ChangeId};
use crate::hash::ObjectId;
use crate::object::{Commit, Signature};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::str::FromStr;

pub const CHANGE_TRAILER: &str = "Rosen-Change";
pub const RECEIPT_TRAILER: &str = "Rosen-Receipt";
pub const APPROVAL_TRAILER: &str = "Rosen-Approved-By";

/// The trailers a landed commit carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandedTrailers {
    pub change: ChangeId,
    /// Absent for prose-only changes and server reverts, which land without
    /// a receipt (R8 step 1, R12).
    pub receipt: Option<ReceiptTrailer>,
    pub approvers: Vec<AgentId>,
}

impl LandedTrailers {
    fn render(&self) -> String {
        let mut out = format!(
            "{CHANGE_TRAILER}: {}
",
            self.change
        );
        if let Some(receipt) = &self.receipt {
            out.push_str(&format!(
                "{RECEIPT_TRAILER}: {receipt}
"
            ));
        }
        for approver in &self.approvers {
            out.push_str(&format!(
                "{APPROVAL_TRAILER}: {approver}
"
            ));
        }
        out
    }

    /// Reads Rosen's trailers from the last paragraph of a commit message.
    /// Returns `None` for commits that did not come from a land, such as
    /// imported history. A receipt trailer that does not parse reads as no
    /// receipt rather than hiding the change the commit landed.
    pub fn parse(message: &[u8]) -> Option<Self> {
        let text = String::from_utf8_lossy(message);
        let last_paragraph = text
            .trim_end()
            .rsplit(
                "

",
            )
            .next()?;
        let mut change = None;
        let mut receipt = None;
        let mut approvers = Vec::new();
        for line in last_paragraph.lines() {
            let Some((key, value)) = line.split_once(": ") else {
                continue;
            };
            match key {
                CHANGE_TRAILER => change = value.trim().parse::<ChangeId>().ok(),
                RECEIPT_TRAILER => receipt = value.parse::<ReceiptTrailer>().ok(),
                APPROVAL_TRAILER => approvers.extend(AgentId::new(value.trim()).ok()),
                _ => {}
            }
        }
        Some(LandedTrailers {
            change: change?,
            receipt,
            approvers,
        })
    }
}

/// The receipt a land counted (R8 step 1), as its `Rosen-Receipt` trailer:
/// `<check set> <head> <base> <submitter>`. It names the whole binding
/// `(head, base, check set)` so the landed commit records exactly what was
/// checked; when the base moved and the receipt was reused, `base` is older
/// than the commit's parent.
///
/// Every field is a single token, so the value can never break onto a new
/// line and forge another trailer (issue #18): the check set is validated
/// here, and the other fields are typed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ReceiptTrailerFields")]
pub struct ReceiptTrailer {
    check_set: String,
    pub head: ObjectId,
    pub base: ObjectId,
    pub submitter: AgentId,
}

/// The unvalidated wire form, so a deserialized trailer is checked like a
/// constructed one.
#[derive(Deserialize)]
struct ReceiptTrailerFields {
    check_set: String,
    head: ObjectId,
    base: ObjectId,
    submitter: AgentId,
}

impl TryFrom<ReceiptTrailerFields> for ReceiptTrailer {
    type Error = Error;
    fn try_from(fields: ReceiptTrailerFields) -> Result<Self> {
        ReceiptTrailer::new(fields.check_set, fields.head, fields.base, fields.submitter)
    }
}

impl ReceiptTrailer {
    pub fn new(
        check_set: impl Into<String>,
        head: ObjectId,
        base: ObjectId,
        submitter: AgentId,
    ) -> Result<Self> {
        let check_set = check_set.into();
        if check_set.is_empty()
            || check_set
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(Error::InvalidCheckSet(check_set));
        }
        Ok(ReceiptTrailer {
            check_set,
            head,
            base,
            submitter,
        })
    }

    pub fn check_set(&self) -> &str {
        &self.check_set
    }
}

impl fmt::Display for ReceiptTrailer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {}",
            self.check_set, self.head, self.base, self.submitter
        )
    }
}

impl FromStr for ReceiptTrailer {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        let malformed = || Error::InvalidCheckSet(value.to_owned());
        let fields: Vec<&str> = value.split_whitespace().collect();
        let [check_set, head, base, submitter] = fields[..] else {
            return Err(malformed());
        };
        ReceiptTrailer::new(
            check_set,
            ObjectId::from_hex(head)?,
            ObjectId::from_hex(base)?,
            AgentId::new(submitter)?,
        )
    }
}

/// Everything the server needs to write a change's landed commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandedCommit {
    /// The base commit the land compare-and-swaps from.
    pub base: ObjectId,
    /// The change's final merged tree.
    pub tree: ObjectId,
    pub author: Signature,
    pub committer: Signature,
    /// The change's description, which becomes the message body.
    pub description: String,
    pub trailers: LandedTrailers,
}

impl LandedCommit {
    /// Builds the commit object. Its only parent is the base, so the base
    /// stays a single line of one commit per change.
    pub fn to_commit(&self) -> Commit {
        let description = self.description.trim_end();
        let mut message = String::with_capacity(description.len() + 128);
        if !description.is_empty() {
            message.push_str(description);
            message.push_str("\n\n");
        }
        message.push_str(&self.trailers.render());
        Commit {
            tree: self.tree,
            parents: vec![self.base],
            author: self.author.clone(),
            committer: self.committer.clone(),
            extra_headers: Vec::new(),
            message: message.into_bytes(),
        }
    }
}

/// Read access to commit ancestry.
pub trait CommitGraph {
    /// The parents of a commit, first parent first.
    fn parents(&self, commit: &ObjectId) -> Result<Vec<ObjectId>>;
}

impl CommitGraph for HashMap<ObjectId, Commit> {
    fn parents(&self, commit: &ObjectId) -> Result<Vec<ObjectId>> {
        self.get(commit)
            .map(|found| found.parents.clone())
            .ok_or(Error::MissingCommit(*commit))
    }
}

/// The base's log: follows first parents from `start`, newest first. Landed
/// history is linear (D15); for imported merges the first parent is the
/// branch that was merged into.
pub fn first_parent_log(
    graph: &impl CommitGraph,
    start: ObjectId,
    limit: usize,
) -> Result<Vec<ObjectId>> {
    let mut log = Vec::new();
    let mut next = Some(start);
    while let Some(commit) = next.filter(|_| log.len() < limit) {
        log.push(commit);
        next = graph.parents(&commit)?.first().copied();
    }
    Ok(log)
}

/// Every commit reachable from `start`, including itself.
pub fn ancestors(graph: &impl CommitGraph, start: ObjectId) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::from([start]);
    let mut queue = VecDeque::from([start]);
    while let Some(commit) = queue.pop_front() {
        for parent in graph.parents(&commit)? {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }
    Ok(seen)
}

/// Whether `ancestor` is reachable from `descendant`. The mirror push uses
/// this to stay a fast-forward (R1).
pub fn is_ancestor(
    graph: &impl CommitGraph,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool> {
    let mut seen = HashSet::from([descendant]);
    let mut queue = VecDeque::from([descendant]);
    while let Some(commit) = queue.pop_front() {
        if commit == ancestor {
            return Ok(true);
        }
        for parent in graph.parents(&commit)? {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }
    Ok(false)
}

/// The best common ancestors of `a` and `b`: common ancestors that are not
/// ancestors of another common ancestor. Criss-cross histories can have
/// more than one; the result is sorted so callers pick deterministically.
pub fn merge_bases(graph: &impl CommitGraph, a: ObjectId, b: ObjectId) -> Result<Vec<ObjectId>> {
    let reachable_from_a = ancestors(graph, a)?;
    let candidates = nearest_common(graph, &reachable_from_a, b)?;
    let mut dominated = HashSet::new();
    for candidate in &candidates {
        for parent in graph.parents(candidate)? {
            for older in ancestors(graph, parent)? {
                if candidates.contains(&older) {
                    dominated.insert(older);
                }
            }
        }
    }
    let mut best: Vec<ObjectId> = candidates.difference(&dominated).copied().collect();
    best.sort();
    Ok(best)
}

/// The single merge base three-way merge uses (R5), or `None` when the
/// histories are unrelated.
pub fn merge_base(graph: &impl CommitGraph, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
    Ok(merge_bases(graph, a, b)?.into_iter().next())
}

/// Walks back from `start` and stops at the first commits that are in
/// `targets`, without walking past them.
fn nearest_common(
    graph: &impl CommitGraph,
    targets: &HashSet<ObjectId>,
    start: ObjectId,
) -> Result<HashSet<ObjectId>> {
    let mut found = HashSet::new();
    let mut seen = HashSet::from([start]);
    let mut queue = VecDeque::from([start]);
    while let Some(commit) = queue.pop_front() {
        if targets.contains(&commit) {
            found.insert(commit);
            continue;
        }
        for parent in graph.parents(&commit)? {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }
    Ok(found)
}
