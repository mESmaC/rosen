//! Conflicts as first-class objects stored on a change (SPEC §2 "Conflict", R5, R8).
//!
//! A merge region that does not resolve automatically is never an error and never a
//! silent pick: it becomes a [`Conflict`] recorded in the change's [`ConflictSet`].
//! While any conflict is open the change cannot land.

use std::fmt;
use std::str::FromStr;

/// Stable identity of a conflict, derived from its content so the same unmerged
/// region always gets the same ID, across re-merges and server restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConflictId(u64);

impl ConflictId {
    /// Hashes the conflict's location and all three sides.
    ///
    /// FNV-1a keeps `rosen-core` free of dependencies. Each field is length-prefixed
    /// so that moving bytes between adjacent fields always changes the ID.
    fn of(path: &str, start_line: u32, base: &[u8], ours: &[u8], theirs: &[u8]) -> Self {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = OFFSET;
        let mut feed = |bytes: &[u8]| {
            for &byte in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(PRIME);
            }
        };
        feed(path.as_bytes());
        feed(&start_line.to_le_bytes());
        feed(base);
        feed(ours);
        feed(theirs);
        ConflictId(hash)
    }
}

impl fmt::Display for ConflictId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl FromStr for ConflictId {
    type Err = ConflictError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.len() != 16 {
            return Err(ConflictError::BadId(text.to_owned()));
        }
        u64::from_str_radix(text, 16)
            .map(ConflictId)
            .map_err(|_| ConflictError::BadId(text.to_owned()))
    }
}

/// Whether a conflict still blocks its change from landing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictState {
    Open,
    Resolved,
}

impl ConflictState {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictState::Open => "open",
            ConflictState::Resolved => "resolved",
        }
    }
}

impl FromStr for ConflictState {
    type Err = ConflictError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "open" => Ok(ConflictState::Open),
            "resolved" => Ok(ConflictState::Resolved),
            other => Err(ConflictError::BadState(other.to_owned())),
        }
    }
}

/// One region of one file that did not merge automatically.
///
/// The three sides are kept as raw bytes, exactly as they appear in each version,
/// so a conflict can be shown, re-rendered or resolved without re-running the merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub id: ConflictId,
    /// Repository-relative path, `/`-separated.
    pub path: String,
    /// 1-based line in the merged result where the conflicted region begins.
    pub start_line: u32,
    pub base: Vec<u8>,
    /// The change's own side.
    pub ours: Vec<u8>,
    /// The side being merged in (the new base, or the parent change when stacked).
    pub theirs: Vec<u8>,
    pub state: ConflictState,
}

impl Conflict {
    /// Creates an open conflict. Its ID is derived from everything else.
    pub fn new(
        path: impl Into<String>,
        start_line: u32,
        base: impl Into<Vec<u8>>,
        ours: impl Into<Vec<u8>>,
        theirs: impl Into<Vec<u8>>,
    ) -> Self {
        let (path, base, ours, theirs) = (path.into(), base.into(), ours.into(), theirs.into());
        Conflict {
            id: ConflictId::of(&path, start_line, &base, &ours, &theirs),
            path,
            start_line,
            base,
            ours,
            theirs,
            state: ConflictState::Open,
        }
    }

    pub fn is_open(&self) -> bool {
        self.state == ConflictState::Open
    }

    /// Renders the region as diff3-style conflict markers, which is what the
    /// workspace shows in place of the region (R5).
    pub fn render_markers(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(self.base.len() + self.ours.len() + self.theirs.len() + 64);
        let sections: [(&str, &[u8]); 3] = [
            ("<<<<<<< ours", &self.ours),
            ("||||||| base", &self.base),
            ("=======", &self.theirs),
        ];
        for (marker, body) in sections {
            out.extend_from_slice(marker.as_bytes());
            out.push(b'\n');
            push_terminated(&mut out, body);
        }
        out.extend_from_slice(b">>>>>>> theirs\n");
        out
    }
}

/// Appends `body`, adding a newline if it lacks one so the next marker starts on
/// its own line.
fn push_terminated(out: &mut Vec<u8>, body: &[u8]) {
    out.extend_from_slice(body);
    if !body.is_empty() && !body.ends_with(b"\n") {
        out.push(b'\n');
    }
}

/// Every conflict stored on one change, open and resolved, ordered by path then line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConflictSet {
    conflicts: Vec<Conflict>,
}

impl ConflictSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds a set from stored conflicts, restoring the canonical order.
    pub fn from_stored(conflicts: impl IntoIterator<Item = Conflict>) -> Self {
        let mut set = ConflictSet {
            conflicts: conflicts.into_iter().collect(),
        };
        set.sort();
        set
    }

    /// Records the outcome of merging one file: the latest merge is the truth for
    /// that path.
    ///
    /// Open conflicts on `path` that the merge no longer produces are dropped, since
    /// the region now merges. A resolved conflict that the merge produces again is
    /// reopened: resolving it did not change the inputs, and picking a side silently
    /// would break R5. A clean merge passes an empty `conflicts`.
    pub fn record_merge(&mut self, path: &str, conflicts: impl IntoIterator<Item = Conflict>) {
        self.conflicts.retain(|c| c.path != path || !c.is_open());
        for mut conflict in conflicts {
            debug_assert_eq!(conflict.path, path, "merge result for one path only");
            conflict.state = ConflictState::Open;
            match self.conflicts.iter_mut().find(|c| c.id == conflict.id) {
                Some(existing) => existing.state = ConflictState::Open,
                None => self.conflicts.push(conflict),
            }
        }
        self.sort();
    }

    /// Marks one conflict resolved (`rosen resolve <id>`).
    pub fn resolve(&mut self, id: ConflictId) -> Result<&Conflict, ConflictError> {
        let conflict = self
            .conflicts
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or(ConflictError::Unknown(id))?;
        conflict.state = ConflictState::Resolved;
        Ok(conflict)
    }

    /// Marks every open conflict in `path` resolved (`rosen resolve <path>`) and
    /// returns how many were open.
    pub fn resolve_path(&mut self, path: &str) -> usize {
        let mut resolved = 0;
        for conflict in self
            .conflicts
            .iter_mut()
            .filter(|c| c.path == path && c.is_open())
        {
            conflict.state = ConflictState::Resolved;
            resolved += 1;
        }
        resolved
    }

    pub fn get(&self, id: ConflictId) -> Option<&Conflict> {
        self.conflicts.iter().find(|c| c.id == id)
    }

    pub fn all(&self) -> &[Conflict] {
        &self.conflicts
    }

    pub fn open(&self) -> impl Iterator<Item = &Conflict> {
        self.conflicts.iter().filter(|c| c.is_open())
    }

    /// Files with at least one open conflict, sorted and deduplicated. These are
    /// the files a `conflict` event names (R8).
    pub fn open_paths(&self) -> Vec<&str> {
        let mut paths: Vec<&str> = self.open().map(|c| c.path.as_str()).collect();
        paths.dedup();
        paths
    }

    /// The landing gate: a change that has open conflicts cannot land (SPEC §2, R8).
    pub fn check_landable(&self) -> Result<(), ConflictError> {
        let paths = self.open_paths();
        if paths.is_empty() {
            return Ok(());
        }
        Err(ConflictError::BlocksLanding {
            open: self.open().count(),
            paths: paths.into_iter().map(str::to_owned).collect(),
        })
    }

    fn sort(&mut self) {
        self.conflicts
            .sort_by(|a, b| (&a.path, a.start_line, a.id).cmp(&(&b.path, b.start_line, b.id)));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictError {
    /// No conflict with this ID is stored on the change.
    Unknown(ConflictId),
    /// A conflict ID that is not 16 hex digits.
    BadId(String),
    /// A stored state that is neither `open` nor `resolved`.
    BadState(String),
    /// The change still has open conflicts in these files.
    BlocksLanding { open: usize, paths: Vec<String> },
}

impl fmt::Display for ConflictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConflictError::Unknown(id) => write!(f, "no conflict {id} on this change"),
            ConflictError::BadId(text) => {
                write!(f, "invalid conflict id {text:?}: expected 16 hex digits")
            }
            ConflictError::BadState(text) => write!(f, "invalid conflict state {text:?}"),
            ConflictError::BlocksLanding { open, paths } => write!(
                f,
                "{open} open conflict(s) block landing; resolve them in: {}",
                paths.join(", ")
            ),
        }
    }
}

impl std::error::Error for ConflictError {}
