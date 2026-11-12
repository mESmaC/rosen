//! Line-level three-way merge (diff3).
//!
//! Base, ours and theirs are cut into chunks. A *stable* chunk is a base line
//! both sides kept. An *unstable* chunk is the stretch between two stable
//! chunks, taken from all three files. An unstable chunk merges cleanly when
//! one side left it as in the base (take the other side) or both sides made
//! the same edit; otherwise it is a conflict carrying all three texts. This is
//! the model proved in `proofs/ThreeWayMerge/Design.lean`.

use std::ops::Range;

use crate::diff::match_lines;
use crate::lines::{split_lines, LineInterner};

/// One region of a three-way merge result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hunk {
    /// Text that merged automatically.
    Clean(Vec<u8>),
    /// A region both sides changed differently.
    Conflict(ConflictRegion),
}

/// A region that did not merge automatically: what each of the three files
/// holds there, and which lines of each file it covers (0-based, half-open).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRegion {
    pub base: Vec<u8>,
    pub ours: Vec<u8>,
    pub theirs: Vec<u8>,
    pub base_lines: Range<usize>,
    pub ours_lines: Range<usize>,
    pub theirs_lines: Range<usize>,
}

/// The result of merging three versions of one text file, as an ordered list
/// of hunks. Adjacent clean hunks are joined, so clean and conflict hunks
/// alternate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextMerge {
    pub hunks: Vec<Hunk>,
}

/// Labels written after the conflict markers.
#[derive(Clone, Copy, Debug)]
pub struct MarkerLabels<'a> {
    pub ours: &'a str,
    pub base: &'a str,
    pub theirs: &'a str,
}

impl Default for MarkerLabels<'_> {
    fn default() -> Self {
        Self {
            ours: "ours",
            base: "base",
            theirs: "theirs",
        }
    }
}

impl TextMerge {
    pub fn is_clean(&self) -> bool {
        self.conflicts().next().is_none()
    }

    pub fn conflicts(&self) -> impl Iterator<Item = &ConflictRegion> {
        self.hunks.iter().filter_map(|hunk| match hunk {
            Hunk::Conflict(region) => Some(region),
            Hunk::Clean(_) => None,
        })
    }

    /// The merged text, or `None` if any region conflicts.
    pub fn clean_text(&self) -> Option<Vec<u8>> {
        self.is_clean()
            .then(|| self.render(MarkerLabels::default()))
    }

    /// The merged text with diff3-style conflict markers around each
    /// conflict, as a workspace shows it (SPEC R5).
    pub fn render(&self, labels: MarkerLabels) -> Vec<u8> {
        let mut out = Vec::new();
        for hunk in &self.hunks {
            match hunk {
                Hunk::Clean(text) => out.extend_from_slice(text),
                Hunk::Conflict(region) => write_conflict(&mut out, region, labels),
            }
        }
        out
    }
}

fn write_conflict(out: &mut Vec<u8>, region: &ConflictRegion, labels: MarkerLabels) {
    write_marker_section(out, &format!("<<<<<<< {}\n", labels.ours), &region.ours);
    write_marker_section(out, &format!("||||||| {}\n", labels.base), &region.base);
    write_marker_section(out, "=======\n", &region.theirs);
    out.extend_from_slice(format!(">>>>>>> {}\n", labels.theirs).as_bytes());
}

fn write_marker_section(out: &mut Vec<u8>, marker: &str, text: &[u8]) {
    out.extend_from_slice(marker.as_bytes());
    out.extend_from_slice(text);
    // Markers must start a line, so a side whose region ends the file
    // without a newline gets one here.
    if text.last().is_some_and(|&byte| byte != b'\n') {
        out.push(b'\n');
    }
}

/// One chunk of the diff3 partition, as line ranges into each file.
#[derive(Debug)]
enum Chunk {
    /// Base lines `base`, kept unchanged by both sides.
    Stable { base: Range<usize> },
    Unstable {
        base: Range<usize>,
        ours: Range<usize>,
        theirs: Range<usize>,
    },
}

/// Merges `ours` and `theirs`, both derived from `base`, line by line.
pub fn merge_text(base: &[u8], ours: &[u8], theirs: &[u8]) -> TextMerge {
    let files = [split_lines(base), split_lines(ours), split_lines(theirs)];
    let mut interner = LineInterner::default();
    let ids = files.each_ref().map(|lines| interner.intern_all(lines));
    let ours_matches = match_lines(&ids[0], &ids[1]);
    let theirs_matches = match_lines(&ids[0], &ids[2]);
    let chunks = partition(&ours_matches, &theirs_matches, ids[1].len(), ids[2].len());
    let mut merge = TextMerge::default();
    for chunk in chunks {
        let hunk = resolve_chunk(&chunk, &files, &ids);
        push_hunk(&mut merge.hunks, hunk);
    }
    merge
}

/// Cuts the three files into stable and unstable chunks from the two match
/// maps. Every line of every file lands in exactly one chunk, in order, so
/// no line can be dropped or duplicated by the partition itself.
fn partition(
    ours_of: &[Option<usize>],
    theirs_of: &[Option<usize>],
    ours_len: usize,
    theirs_len: usize,
) -> Vec<Chunk> {
    let base_len = ours_of.len();
    let (mut base, mut ours, mut theirs) = (0, 0, 0);
    let mut chunks = Vec::new();
    loop {
        let stable_start = base;
        while base < base_len && ours_of[base] == Some(ours) && theirs_of[base] == Some(theirs) {
            (base, ours, theirs) = (base + 1, ours + 1, theirs + 1);
        }
        if base > stable_start {
            chunks.push(Chunk::Stable {
                base: stable_start..base,
            });
        }
        if base == base_len && ours == ours_len && theirs == theirs_len {
            return chunks;
        }
        // The chunk ends at the next base line both sides kept; matches are
        // increasing, so that line sits at or after the current position in
        // each side.
        let sync = (base..base_len).find_map(|line| Some((line, ours_of[line]?, theirs_of[line]?)));
        let (next_base, next_ours, next_theirs) = sync.unwrap_or((base_len, ours_len, theirs_len));
        chunks.push(Chunk::Unstable {
            base: base..next_base,
            ours: ours..next_ours,
            theirs: theirs..next_theirs,
        });
        (base, ours, theirs) = (next_base, next_ours, next_theirs);
    }
}

fn resolve_chunk(chunk: &Chunk, files: &[Vec<&[u8]>; 3], ids: &[Vec<u32>; 3]) -> Hunk {
    let (base, ours, theirs) = match chunk {
        Chunk::Stable { base } => return Hunk::Clean(files[0][base.clone()].concat()),
        Chunk::Unstable { base, ours, theirs } => (base, ours, theirs),
    };
    let [base_ids, ours_ids, theirs_ids] = [
        &ids[0][base.clone()],
        &ids[1][ours.clone()],
        &ids[2][theirs.clone()],
    ];
    if ours_ids == base_ids || ours_ids == theirs_ids {
        // Ours unchanged, or both sides made the same edit: theirs is right.
        Hunk::Clean(files[2][theirs.clone()].concat())
    } else if theirs_ids == base_ids {
        Hunk::Clean(files[1][ours.clone()].concat())
    } else {
        Hunk::Conflict(ConflictRegion {
            base: files[0][base.clone()].concat(),
            ours: files[1][ours.clone()].concat(),
            theirs: files[2][theirs.clone()].concat(),
            base_lines: base.clone(),
            ours_lines: ours.clone(),
            theirs_lines: theirs.clone(),
        })
    }
}

fn push_hunk(hunks: &mut Vec<Hunk>, hunk: Hunk) {
    match (hunks.last_mut(), hunk) {
        (_, Hunk::Clean(text)) if text.is_empty() => {}
        (Some(Hunk::Clean(previous)), Hunk::Clean(text)) => previous.extend_from_slice(&text),
        (_, hunk) => hunks.push(hunk),
    }
}
