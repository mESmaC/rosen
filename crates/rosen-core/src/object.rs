//! Git-format objects: blobs, trees and commits (D5).
//!
//! Text fields are kept as raw bytes, not `String`. Git does not require
//! UTF-8 in file names, author names or messages, and an imported commit must
//! re-encode to the same bytes or its ID (and every descendant's) would change.

use crate::hash::ObjectId;
use crate::{Error, Result};
use std::borrow::Cow;
use std::collections::HashSet;

/// The three object kinds Rosen stores. Git tags are not used: the base is
/// the only ref (D18), and release refs point at commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
}

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Blob => "blob",
            ObjectKind::Tree => "tree",
            ObjectKind::Commit => "commit",
        }
    }

    pub fn parse(name: &[u8]) -> Result<Self> {
        match name {
            b"blob" => Ok(ObjectKind::Blob),
            b"tree" => Ok(ObjectKind::Tree),
            b"commit" => Ok(ObjectKind::Commit),
            other => Err(malformed(format!(
                "unsupported object kind {:?}",
                String::from_utf8_lossy(other)
            ))),
        }
    }
}

/// A file's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub data: Vec<u8>,
}

/// The modes git allows in a tree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileMode {
    Regular,
    Executable,
    Symlink,
    Directory,
    /// A gitlink. Rosen keeps them so imported trees hash the same.
    Submodule,
    /// A mode old git versions wrote, such as `100664`. Kept verbatim so
    /// imported history keeps its IDs; Rosen never writes one itself.
    Legacy(u32),
}

impl FileMode {
    /// The octal form git writes inside tree objects (no leading zero).
    pub fn as_octal(self) -> Cow<'static, str> {
        Cow::Borrowed(match self {
            FileMode::Regular => "100644",
            FileMode::Executable => "100755",
            FileMode::Symlink => "120000",
            FileMode::Directory => "40000",
            FileMode::Submodule => "160000",
            FileMode::Legacy(mode) => return Cow::Owned(format!("{mode:o}")),
        })
    }

    pub fn parse(octal: &[u8]) -> Result<Self> {
        match octal {
            b"100644" => Ok(FileMode::Regular),
            b"100755" => Ok(FileMode::Executable),
            b"120000" => Ok(FileMode::Symlink),
            b"40000" => Ok(FileMode::Directory),
            b"160000" => Ok(FileMode::Submodule),
            other => Self::parse_legacy(other).ok_or_else(|| {
                Error::InvalidTreeEntry(format!("mode {:?}", String::from_utf8_lossy(other)))
            }),
        }
    }

    /// Accepts any octal mode without a leading zero; a zero-padded mode
    /// could not be written back byte for byte.
    fn parse_legacy(octal: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(octal).ok()?;
        if text.starts_with('0') || text.len() > 7 {
            return None;
        }
        u32::from_str_radix(text, 8).ok().map(FileMode::Legacy)
    }

    pub fn is_tree(self) -> bool {
        match self {
            FileMode::Directory => true,
            FileMode::Legacy(mode) => mode & 0o170000 == 0o040000,
            _ => false,
        }
    }
}

/// One named entry in a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: FileMode,
    pub name: Vec<u8>,
    pub id: ObjectId,
}

impl TreeEntry {
    pub fn new(mode: FileMode, name: impl Into<Vec<u8>>, id: ObjectId) -> Result<Self> {
        let name = name.into();
        validate_entry_name(&name)?;
        Ok(TreeEntry { mode, name, id })
    }

    /// Git sorts tree entries as if directory names ended in `/`; any other
    /// order produces a different tree ID for the same contents.
    fn sort_key(&self) -> impl Iterator<Item = u8> + '_ {
        let suffix = if self.mode.is_tree() {
            Some(b'/')
        } else {
            None
        };
        self.name.iter().copied().chain(suffix)
    }
}

fn validate_entry_name(name: &[u8]) -> Result<()> {
    let bad = name.is_empty()
        || name == b"."
        || name == b".."
        || name.eq_ignore_ascii_case(b".git")
        || name.iter().any(|&b| b == b'/' || b == 0);
    if bad {
        return Err(Error::InvalidTreeEntry(format!(
            "name {:?}",
            String::from_utf8_lossy(name)
        )));
    }
    Ok(())
}

/// A directory listing. Entries are always held in git order with unique
/// names, so encoding a `Tree` always yields its canonical ID.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tree {
    entries: Vec<TreeEntry>,
}

impl Tree {
    /// Sorts entries into git order and rejects duplicate names.
    pub fn new(mut entries: Vec<TreeEntry>) -> Result<Self> {
        entries.sort_by(|a, b| a.sort_key().cmp(b.sort_key()));
        // Checked by name alone, not across adjacent git-ordered entries: git
        // order sorts directory `a` as `a/`, so another entry such as `a.b`
        // can fall between file `a` and directory `a`. git fsck rejects that
        // tree as duplicateEntries, which would fail a mirror push (R1).
        let mut seen = HashSet::with_capacity(entries.len());
        if let Some(entry) = entries.iter().find(|entry| !seen.insert(&entry.name[..])) {
            return Err(Error::InvalidTreeEntry(format!(
                "duplicate name {:?}",
                String::from_utf8_lossy(&entry.name)
            )));
        }
        Ok(Tree { entries })
    }

    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    pub fn get(&self, name: &[u8]) -> Option<&TreeEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in &self.entries {
            out.extend_from_slice(entry.mode.as_octal().as_bytes());
            out.push(b' ');
            out.extend_from_slice(&entry.name);
            out.push(0);
            out.extend_from_slice(entry.id.as_bytes());
        }
        out
    }

    fn decode(mut body: &[u8]) -> Result<Self> {
        let mut entries = Vec::new();
        while !body.is_empty() {
            let space = position(body, b' ', "tree entry mode")?;
            let mode = FileMode::parse(&body[..space])?;
            body = &body[space + 1..];
            let nul = position(body, 0, "tree entry name")?;
            let name = body[..nul].to_vec();
            body = &body[nul + 1..];
            if body.len() < ObjectId::LEN {
                return Err(malformed("truncated tree entry id"));
            }
            let id = ObjectId::from_slice(&body[..ObjectId::LEN])?;
            body = &body[ObjectId::LEN..];
            entries.push(TreeEntry::new(mode, name, id)?);
        }
        let tree = Tree::new(entries.clone())?;
        // A tree out of git order would re-encode to a different ID, which
        // would silently change history on import.
        if tree.entries != entries {
            return Err(malformed("tree entries are not in git order"));
        }
        Ok(tree)
    }
}

/// An author or committer line: `Name <email> <seconds> <+hhmm>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: Vec<u8>,
    pub email: Vec<u8>,
    /// Seconds since the Unix epoch.
    pub seconds: i64,
    /// The timezone exactly as written, for example `+0100`. Kept as text
    /// because some real repositories hold offsets git would not write today.
    pub timezone: Vec<u8>,
}

impl Signature {
    pub fn new(name: impl Into<Vec<u8>>, email: impl Into<Vec<u8>>, seconds: i64) -> Self {
        Signature {
            name: name.into(),
            email: email.into(),
            seconds,
            timezone: b"+0000".to_vec(),
        }
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.name);
        out.extend_from_slice(b" <");
        out.extend_from_slice(&self.email);
        out.extend_from_slice(b"> ");
        out.extend_from_slice(self.seconds.to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(&self.timezone);
    }

    fn decode(line: &[u8]) -> Result<Self> {
        let open = position(line, b'<', "signature email")?;
        let close = open + position(&line[open..], b'>', "signature email end")?;
        let name = line[..open]
            .strip_suffix(b" ")
            .unwrap_or(&line[..open])
            .to_vec();
        let email = line[open + 1..close].to_vec();
        let rest = std::str::from_utf8(&line[close + 1..])
            .map_err(|_| malformed("signature time is not text"))?;
        let mut parts = rest.split_whitespace();
        let seconds = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| malformed("signature time"))?;
        let timezone = parts.next().unwrap_or("+0000").as_bytes().to_vec();
        Ok(Signature {
            name,
            email,
            seconds,
            timezone,
        })
    }
}

/// A commit. `extra_headers` holds headers Rosen does not interpret (such as
/// `gpgsig`, `mergetag` or `encoding`) so imported commits keep their IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Signature,
    pub committer: Signature,
    pub extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub message: Vec<u8>,
}

impl Commit {
    pub fn message_text(&self) -> String {
        String::from_utf8_lossy(&self.message).into_owned()
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        push_header(&mut out, b"tree", self.tree.to_hex().as_bytes());
        for parent in &self.parents {
            push_header(&mut out, b"parent", parent.to_hex().as_bytes());
        }
        let mut signature = Vec::new();
        self.author.encode_into(&mut signature);
        push_header(&mut out, b"author", &signature);
        signature.clear();
        self.committer.encode_into(&mut signature);
        push_header(&mut out, b"committer", &signature);
        for (key, value) in &self.extra_headers {
            push_header(&mut out, key, value);
        }
        out.push(b'\n');
        out.extend_from_slice(&self.message);
        out
    }

    fn decode(body: &[u8]) -> Result<Self> {
        let (headers, message) = split_commit_headers(body)?;
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        let mut extra_headers = Vec::new();
        for (key, value) in headers {
            match key.as_slice() {
                b"tree" if tree.is_none() => tree = Some(hex_id(&value)?),
                b"parent" => parents.push(hex_id(&value)?),
                b"author" if author.is_none() => author = Some(Signature::decode(&value)?),
                b"committer" if committer.is_none() => committer = Some(Signature::decode(&value)?),
                _ => extra_headers.push((key, value)),
            }
        }
        Ok(Commit {
            tree: tree.ok_or_else(|| malformed("commit has no tree"))?,
            parents,
            author: author.ok_or_else(|| malformed("commit has no author"))?,
            committer: committer.ok_or_else(|| malformed("commit has no committer"))?,
            extra_headers,
            message: message.to_vec(),
        })
    }
}

/// Writes one header line. Git continues a multi-line value (such as a
/// signature) by starting each following line with a space.
fn push_header(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    out.extend_from_slice(key);
    out.push(b' ');
    for &byte in value {
        out.push(byte);
        if byte == b'\n' {
            out.push(b' ');
        }
    }
    out.push(b'\n');
}

type Header = (Vec<u8>, Vec<u8>);

/// Splits a commit body into its headers and the message after the blank line.
fn split_commit_headers(body: &[u8]) -> Result<(Vec<Header>, &[u8])> {
    let mut headers: Vec<Header> = Vec::new();
    let mut rest = body;
    loop {
        let newline = position(rest, b'\n', "commit header line")?;
        let line = &rest[..newline];
        rest = &rest[newline + 1..];
        if line.is_empty() {
            return Ok((headers, rest));
        }
        if let Some(continuation) = line.strip_prefix(b" ") {
            let (_, value) = headers
                .last_mut()
                .ok_or_else(|| malformed("continuation before any header"))?;
            value.push(b'\n');
            value.extend_from_slice(continuation);
            continue;
        }
        let space = position(line, b' ', "commit header key")?;
        headers.push((line[..space].to_vec(), line[space + 1..].to_vec()));
    }
}

/// Any object Rosen stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Object {
    Blob(Blob),
    Tree(Tree),
    /// Boxed because a commit is far larger than a blob or tree handle.
    Commit(Box<Commit>),
}

impl Object {
    pub fn kind(&self) -> ObjectKind {
        match self {
            Object::Blob(_) => ObjectKind::Blob,
            Object::Tree(_) => ObjectKind::Tree,
            Object::Commit(_) => ObjectKind::Commit,
        }
    }

    /// The object body in git format, without the `<kind> <len>\0` header.
    pub fn encode_body(&self) -> Vec<u8> {
        match self {
            Object::Blob(blob) => blob.data.clone(),
            Object::Tree(tree) => tree.encode(),
            Object::Commit(commit) => commit.encode(),
        }
    }

    pub fn id(&self) -> ObjectId {
        ObjectId::hash_object(self.kind().as_str(), &self.encode_body())
    }

    /// The bytes git stores in a loose object before zlib compression:
    /// header and body. The store crate compresses and writes these.
    pub fn encode_loose(&self) -> Vec<u8> {
        let body = self.encode_body();
        let mut out = format!("{} {}\0", self.kind().as_str(), body.len()).into_bytes();
        out.extend_from_slice(&body);
        out
    }

    /// Parses an object body. A tree or commit that would not re-encode to
    /// the same bytes (odd spacing, reordered headers) is refused: accepting
    /// it would silently give it, and every descendant, a new ID.
    pub fn decode_body(kind: ObjectKind, body: &[u8]) -> Result<Self> {
        let object = match kind {
            ObjectKind::Blob => {
                return Ok(Object::Blob(Blob {
                    data: body.to_vec(),
                }))
            }
            ObjectKind::Tree => Object::Tree(Tree::decode(body)?),
            ObjectKind::Commit => Object::Commit(Box::new(Commit::decode(body)?)),
        };
        if object.encode_body() != body {
            return Err(malformed(format!(
                "{} does not re-encode to the same bytes",
                kind.as_str()
            )));
        }
        Ok(object)
    }

    /// Parses decompressed loose-object bytes and checks the declared length.
    pub fn decode_loose(raw: &[u8]) -> Result<Self> {
        let nul = position(raw, 0, "loose object header")?;
        let header = &raw[..nul];
        let space = position(header, b' ', "loose object kind")?;
        let kind = ObjectKind::parse(&header[..space])?;
        let declared: usize = std::str::from_utf8(&header[space + 1..])
            .ok()
            .and_then(|len| len.parse().ok())
            .ok_or_else(|| malformed("loose object length"))?;
        let body = &raw[nul + 1..];
        if body.len() != declared {
            return Err(malformed(format!(
                "loose object declares {declared} bytes but holds {}",
                body.len()
            )));
        }
        Object::decode_body(kind, body)
    }
}

impl From<Blob> for Object {
    fn from(blob: Blob) -> Self {
        Object::Blob(blob)
    }
}

impl From<Tree> for Object {
    fn from(tree: Tree) -> Self {
        Object::Tree(tree)
    }
}

impl From<Commit> for Object {
    fn from(commit: Commit) -> Self {
        Object::Commit(Box::new(commit))
    }
}

fn hex_id(value: &[u8]) -> Result<ObjectId> {
    let text = std::str::from_utf8(value).map_err(|_| malformed("object id is not text"))?;
    ObjectId::from_hex(text)
}

fn position(haystack: &[u8], needle: u8, what: &str) -> Result<usize> {
    haystack
        .iter()
        .position(|&b| b == needle)
        .ok_or_else(|| malformed(format!("missing {what}")))
}

fn malformed(reason: impl Into<String>) -> Error {
    Error::MalformedObject(reason.into())
}
