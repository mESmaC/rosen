//! Content addresses. Rosen stores objects in git format (D5), so an object's
//! ID is the SHA-1 of its git header and body, exactly as `git hash-object`
//! computes it. Matching git bit for bit is what lets import and the mirror
//! push (R1) move history between Rosen and git without rewriting it.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::fmt;
use std::str::FromStr;

/// The SHA-1 content address of a git-format object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct ObjectId([u8; 20]);

impl ObjectId {
    pub const LEN: usize = 20;
    pub const HEX_LEN: usize = 40;

    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        ObjectId(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Reads a 20-byte raw ID, as stored inside a tree entry.
    pub fn from_slice(raw: &[u8]) -> Result<Self> {
        let bytes: [u8; 20] = raw
            .try_into()
            .map_err(|_| Error::InvalidId(format!("raw id of {} bytes", raw.len())))?;
        Ok(ObjectId(bytes))
    }

    pub fn from_hex(hex: &str) -> Result<Self> {
        Ok(ObjectId(decode_hex::<20>(hex)?))
    }

    pub fn to_hex(&self) -> String {
        encode_hex(&self.0)
    }

    /// Hashes a body under the git header `<kind> <len>\0`.
    pub fn hash_object(kind: &str, body: &[u8]) -> Self {
        let mut hasher = Sha1::new();
        hasher.update(kind.as_bytes());
        hasher.update(b" ");
        hasher.update(body.len().to_string().as_bytes());
        hasher.update([0u8]);
        hasher.update(body);
        ObjectId(hasher.finalize().into())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.to_hex())
    }
}

impl FromStr for ObjectId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        ObjectId::from_hex(s)
    }
}

impl From<ObjectId> for String {
    fn from(id: ObjectId) -> String {
        id.to_hex()
    }
}

impl TryFrom<String> for ObjectId {
    type Error = Error;
    fn try_from(hex: String) -> Result<Self> {
        ObjectId::from_hex(&hex)
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    out
}

/// Decodes exactly `N` bytes of lowercase or uppercase hex.
fn decode_hex<const N: usize>(hex: &str) -> Result<[u8; N]> {
    let digits = hex.as_bytes();
    if digits.len() != N * 2 {
        return Err(Error::InvalidId(format!(
            "{hex:?} is not {} hex digits",
            N * 2
        )));
    }
    let mut out = [0u8; N];
    for (i, pair) in digits.as_chunks::<2>().0.iter().enumerate() {
        let high = hex_digit(pair[0]).ok_or_else(|| Error::InvalidId(hex.to_string()))?;
        let low = hex_digit(pair[1]).ok_or_else(|| Error::InvalidId(hex.to_string()))?;
        out[i] = (high << 4) | low;
    }
    Ok(out)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
