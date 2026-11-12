//! TEMPORARY stand-in for task:2 (define-the-core). Mirrors the agreed
//! `rosen_core::change` API so the change registry builds; dropped in favour
//! of the real module when that PR merges.

use crate::object::ObjectId;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Nibble → letter. Letters k–z never overlap hex digits, so a change ID can
/// never be confused with a commit ID.
const ALPHABET: &[u8; 16] = b"zyxwvutsrqponmlk";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChangeId([u8; 16]);

impl ChangeId {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        ChangeId(bytes)
    }
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{}", ALPHABET[(byte >> 4) as usize] as char)?;
            write!(f, "{}", ALPHABET[(byte & 0xf) as usize] as char)?;
        }
        Ok(())
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangeId({self})")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseChangeIdError;

impl fmt::Display for ParseChangeIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a change ID is 32 letters in k-z")
    }
}

impl FromStr for ChangeId {
    type Err = ParseChangeIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.as_bytes();
        if s.len() != 32 {
            return Err(ParseChangeIdError);
        }
        let nibble = |c: u8| {
            ALPHABET
                .iter()
                .position(|&a| a == c)
                .ok_or(ParseChangeIdError)
        };
        let mut out = [0u8; 16];
        for (i, pair) in s.chunks(2).enumerate() {
            out[i] = ((nibble(pair[0])? as u8) << 4) | nibble(pair[1])? as u8;
        }
        Ok(ChangeId(out))
    }
}

impl Serialize for ChangeId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ChangeId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(String);

impl AgentId {
    pub fn new(id: impl Into<String>) -> Self {
        AgentId(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeState {
    Open,
    Ready,
    Landed,
    Abandoned,
}

impl ChangeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeState::Open => "open",
            ChangeState::Ready => "ready",
            ChangeState::Landed => "landed",
            ChangeState::Abandoned => "abandoned",
        }
    }
    pub fn can_transition_to(self, next: ChangeState) -> bool {
        use ChangeState::*;
        matches!(
            (self, next),
            (Open, Ready)
                | (Ready, Open)
                | (Ready, Landed)
                | (Open, Abandoned)
                | (Ready, Abandoned)
        )
    }
}

impl FromStr for ChangeState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(ChangeState::Open),
            "ready" => Ok(ChangeState::Ready),
            "landed" => Ok(ChangeState::Landed),
            "abandoned" => Ok(ChangeState::Abandoned),
            other => Err(format!("unknown change state {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeParent {
    Base(ObjectId),
    Change(ChangeId),
}
