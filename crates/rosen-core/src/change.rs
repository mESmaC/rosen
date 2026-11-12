//! Changes (SPEC.md §2, D2, D15, D18): one agent's unit of work. A change is
//! a record, not a ref. Its commits belong to it and are never rewritten; a
//! server rebase moves only its parent and its merged head tree.

use crate::hash::ObjectId;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The letters a change ID is written in, one per nibble. Using `k`–`z`
/// (as jj does) means a change ID can never be mistaken for a hex commit ID.
const CHANGE_ID_ALPHABET: &[u8; 16] = b"zyxwvutsrqponmlk";

/// A change's stable identity. It never changes across snapshots, rebases,
/// lands or reverts, so every other record can refer to it. The server draws
/// the 16 bytes from OS randomness; this crate only holds and renders them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct ChangeId([u8; 16]);

impl ChangeId {
    pub const TEXT_LEN: usize = 32;

    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        ChangeId(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    fn letter_value(letter: u8) -> Option<u8> {
        CHANGE_ID_ALPHABET
            .iter()
            .position(|&c| c == letter)
            .map(|nibble| nibble as u8)
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = String::with_capacity(Self::TEXT_LEN);
        for byte in self.0 {
            text.push(CHANGE_ID_ALPHABET[(byte >> 4) as usize] as char);
            text.push(CHANGE_ID_ALPHABET[(byte & 0xf) as usize] as char);
        }
        f.write_str(&text)
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangeId({self})")
    }
}

impl FromStr for ChangeId {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self> {
        let invalid = || Error::InvalidId(format!("{text:?} is not a change id"));
        let letters = text.as_bytes();
        if letters.len() != Self::TEXT_LEN {
            return Err(invalid());
        }
        let mut bytes = [0u8; 16];
        for (byte, pair) in bytes.iter_mut().zip(letters.as_chunks::<2>().0) {
            let high = Self::letter_value(pair[0]).ok_or_else(invalid)?;
            let low = Self::letter_value(pair[1]).ok_or_else(invalid)?;
            *byte = (high << 4) | low;
        }
        Ok(ChangeId(bytes))
    }
}

impl From<ChangeId> for String {
    fn from(id: ChangeId) -> String {
        id.to_string()
    }
}

impl TryFrom<String> for ChangeId {
    type Error = Error;
    fn try_from(text: String) -> Result<Self> {
        text.parse()
    }
}

/// An agent's (or human's) identity: a change's owner, a reviewer, a
/// receipt submitter. It appears in commit trailers, so it must fit on one
/// line and hold no whitespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct AgentId(String);

impl AgentId {
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::InvalidAgentId(name));
        }
        Ok(AgentId(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<AgentId> for String {
    fn from(id: AgentId) -> String {
        id.0
    }
}

impl TryFrom<String> for AgentId {
    type Error = Error;
    fn try_from(name: String) -> Result<Self> {
        AgentId::new(name)
    }
}

/// Refers to a stored conflict object (R5). The conflict's contents are
/// defined with the merge code; a change only needs to know which it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConflictId(pub u64);

/// What a change sits on: a base commit, or another unlanded change (D19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeParent {
    Base(ObjectId),
    Change(ChangeId),
}

/// The change lifecycle: `open` → `ready` → `landed`, or `abandoned`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeState {
    Open,
    /// The owner asked to land.
    Ready,
    Landed,
    Abandoned,
}

impl ChangeState {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeState::Open => "open",
            ChangeState::Ready => "ready",
            ChangeState::Landed => "landed",
            ChangeState::Abandoned => "abandoned",
        }
    }

    /// Landed and abandoned changes take no new snapshots.
    pub fn is_closed(self) -> bool {
        matches!(self, ChangeState::Landed | ChangeState::Abandoned)
    }

    /// The whole state machine. `landed → open` exists only for the
    /// post-land auto-revert (R12), which hands the culprit back to its owner.
    pub fn can_transition_to(self, next: ChangeState) -> bool {
        use ChangeState::*;
        matches!(
            (self, next),
            (Open, Ready)
                | (Ready, Open)
                | (Ready, Landed)
                | (Open | Ready, Abandoned)
                | (Landed, Open)
        )
    }
}

impl fmt::Display for ChangeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ChangeState {
    type Err = Error;
    fn from_str(name: &str) -> Result<Self> {
        match name {
            "open" => Ok(ChangeState::Open),
            "ready" => Ok(ChangeState::Ready),
            "landed" => Ok(ChangeState::Landed),
            "abandoned" => Ok(ChangeState::Abandoned),
            other => Err(Error::InvalidState(other.to_string())),
        }
    }
}

/// How a snapshot came to be (D17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotKind {
    /// Taken automatically from workspace writes.
    Automatic,
    /// Taken by `rosen commit`, carrying the user's message.
    Named(String),
}

/// One commit in a change's own history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub commit: ObjectId,
    pub tree: ObjectId,
    pub kind: SnapshotKind,
}

/// One agent's unit of work (SPEC.md §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub id: ChangeId,
    pub owner: AgentId,
    pub parent: ChangeParent,
    /// The change's own commits, oldest first. Append-only (R2).
    pub snapshots: Vec<Snapshot>,
    /// The current head tree: the latest snapshot, or after a server rebase
    /// the merge of that snapshot onto the new parent.
    pub head: ObjectId,
    pub state: ChangeState,
    pub conflicts: Vec<ConflictId>,
}

impl Change {
    /// A new open change whose head starts at its parent's tree.
    pub fn new(id: ChangeId, owner: AgentId, parent: ChangeParent, parent_tree: ObjectId) -> Self {
        Change {
            id,
            owner,
            parent,
            snapshots: Vec::new(),
            head: parent_tree,
            state: ChangeState::Open,
            conflicts: Vec::new(),
        }
    }

    /// Appends a snapshot and moves the head to its tree.
    pub fn record_snapshot(&mut self, snapshot: Snapshot) -> Result<()> {
        self.ensure_not_closed()?;
        self.head = snapshot.tree;
        self.snapshots.push(snapshot);
        Ok(())
    }

    /// A server rebase (R8, D19): only the parent and merged head move. The
    /// change's snapshots are untouched, so approvals survive (R7).
    pub fn rebase(&mut self, new_parent: ChangeParent, merged_head: ObjectId) -> Result<()> {
        self.ensure_not_closed()?;
        self.parent = new_parent;
        self.head = merged_head;
        Ok(())
    }

    pub fn mark_ready(&mut self) -> Result<()> {
        self.transition(ChangeState::Ready, "mark ready")
    }

    /// The owner withdraws a land request.
    pub fn mark_open(&mut self) -> Result<()> {
        if self.state != ChangeState::Ready {
            return Err(self.refuse("withdraw a land request for"));
        }
        self.transition(ChangeState::Open, "reopen")
    }

    /// Only a ready change without conflicts can land. Approval and receipt
    /// checks belong to the landing transaction (R8), not to the change.
    pub fn mark_landed(&mut self) -> Result<()> {
        if !self.conflicts.is_empty() {
            return Err(self.refuse("land (it has conflicts)"));
        }
        self.transition(ChangeState::Landed, "land")
    }

    pub fn abandon(&mut self) -> Result<()> {
        self.transition(ChangeState::Abandoned, "abandon")
    }

    /// Post-land auto-revert (R12) returns the culprit to its owner with its
    /// snapshots intact, now stacked on the base that holds the revert.
    pub fn reopen_after_revert(&mut self, new_base: ObjectId, head: ObjectId) -> Result<()> {
        if self.state != ChangeState::Landed {
            return Err(self.refuse("revert"));
        }
        self.transition(ChangeState::Open, "reopen after revert")?;
        self.parent = ChangeParent::Base(new_base);
        self.head = head;
        Ok(())
    }

    pub fn add_conflict(&mut self, conflict: ConflictId) {
        if !self.conflicts.contains(&conflict) {
            self.conflicts.push(conflict);
        }
    }

    /// Returns whether the conflict was held.
    pub fn resolve_conflict(&mut self, conflict: ConflictId) -> bool {
        let before = self.conflicts.len();
        self.conflicts.retain(|held| *held != conflict);
        self.conflicts.len() != before
    }

    pub fn can_land(&self) -> bool {
        self.state == ChangeState::Ready && self.conflicts.is_empty()
    }

    fn transition(&mut self, to: ChangeState, action: &'static str) -> Result<()> {
        if !self.state.can_transition_to(to) {
            return Err(self.refuse(action));
        }
        self.state = to;
        Ok(())
    }

    fn refuse(&self, action: &'static str) -> Error {
        Error::InvalidTransition {
            from: self.state,
            action,
        }
    }

    fn ensure_not_closed(&self) -> Result<()> {
        if self.state.is_closed() {
            return Err(Error::ChangeClosed(self.id));
        }
        Ok(())
    }
}
