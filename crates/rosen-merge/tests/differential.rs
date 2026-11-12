//! Differential test: `merge_text` against `git merge-file --diff3` on a
//! generated corpus (SPEC.md §5 "Merge is also checked against
//! `git merge-file` on a differential corpus").
//!
//! Risk (SPEC R5): the Lean proof covers the merge model and the fixture test
//! covers single-letter files. Neither checks real line handling (missing
//! final newlines, CRLF, empty files) or marker rendering against an
//! independent, widely used merge. A divergence here is a line silently
//! dropped, duplicated or picked where git would keep it or conflict.
//!
//! Two diffs of the same pair of files can differ and both be minimal when
//! lines repeat, and Rosen's Myers diff breaks those ties differently from
//! git's xdiff. Such merges legitimately differ, so a case is compared only
//! when both sides' alignment with the base is forced: exactly one maximal
//! matching of lines exists. Then both tools see the same two diffs and must
//! produce the same bytes and the same number of conflicts.

use std::path::Path;
use std::process::Command;

use rosen_merge::{merge_text, MarkerLabels};

/// Compared cases per corpus. Each spawns `git` once, so this bounds the run
/// time.
const COMPARED_PER_CORPUS: usize = 500;
/// Draws per corpus before giving up: most repeated-line cases have tied
/// alignments and are skipped.
const MAX_DRAWS_PER_CORPUS: usize = 50_000;
const WORKERS: usize = 8;

/// xorshift64: the corpus must be the same on every run and machine, so a
/// failure reproduces from its case number alone.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Corpus {
    /// Every line is distinct, so every alignment is forced.
    DistinctLines,
    /// Lines come from a tiny alphabet, so they repeat and alignments tie.
    RepeatedLines,
}

/// Makes the lines a side writes. Distinct-line files number their lines, so
/// two sides share a new line only when they made the same edit.
struct LineSource {
    corpus: Corpus,
    alphabet: usize,
    next_fresh: usize,
}

impl LineSource {
    fn base_line(&mut self, rng: &mut Rng, index: usize) -> String {
        match self.corpus {
            Corpus::DistinctLines => format!("base {index}"),
            Corpus::RepeatedLines => self.fresh_line(rng),
        }
    }

    fn fresh_line(&mut self, rng: &mut Rng) -> String {
        match self.corpus {
            Corpus::DistinctLines => {
                self.next_fresh += 1;
                format!("new {}", self.next_fresh)
            }
            Corpus::RepeatedLines => ((b'a' + rng.below(self.alphabet) as u8) as char).to_string(),
        }
    }

    fn fresh_lines(&mut self, rng: &mut Rng) -> Vec<String> {
        (0..1 + rng.below(2))
            .map(|_| self.fresh_line(rng))
            .collect()
    }
}

/// What one side does to one base line, or to the end of the file.
#[derive(Clone)]
enum Edit {
    Keep,
    Delete,
    Replace(Vec<String>),
    InsertBefore(Vec<String>),
}

fn random_edit(rng: &mut Rng, lines: &mut LineSource) -> Edit {
    match rng.below(10) {
        0 => Edit::Delete,
        1 | 2 => Edit::Replace(lines.fresh_lines(rng)),
        3 => Edit::InsertBefore(lines.fresh_lines(rng)),
        _ => Edit::Keep,
    }
}

/// Edits for both sides at one position. Theirs sometimes copies ours, so
/// identical edits on both sides (which merge cleanly) are common.
fn edit_pair(rng: &mut Rng, lines: &mut LineSource) -> (Edit, Edit) {
    let ours = random_edit(rng, lines);
    let theirs = if rng.chance(4) {
        ours.clone()
    } else {
        random_edit(rng, lines)
    };
    (ours, theirs)
}

fn apply(edit: Edit, base_line: Option<&String>, out: &mut Vec<String>) {
    match edit {
        Edit::Keep => out.extend(base_line.cloned()),
        Edit::Delete => {}
        Edit::Replace(new) => out.extend(new),
        Edit::InsertBefore(new) => {
            out.extend(new);
            out.extend(base_line.cloned());
        }
    }
}

/// One merge input: base, ours and theirs.
struct Case {
    files: [Vec<u8>; 3],
}

fn random_case(rng: &mut Rng, corpus: Corpus) -> Case {
    let mut lines = LineSource {
        corpus,
        alphabet: 3 + rng.below(3),
        next_fresh: 0,
    };
    let base: Vec<String> = (0..rng.below(12))
        .map(|i| lines.base_line(rng, i))
        .collect();
    let (mut ours, mut theirs) = (Vec::new(), Vec::new());
    // One position past the end, so both sides can append to the file.
    for position in 0..=base.len() {
        let (our_edit, their_edit) = edit_pair(rng, &mut lines);
        apply(our_edit, base.get(position), &mut ours);
        apply(their_edit, base.get(position), &mut theirs);
    }
    let newline = if rng.chance(6) { "\r\n" } else { "\n" };
    let files = [base, ours, theirs].map(|file| render_file(rng, &file, newline));
    Case { files }
}

/// Joins lines, sometimes leaving the last one without its newline.
fn render_file(rng: &mut Rng, lines: &[String], newline: &str) -> Vec<u8> {
    let mut text: String = lines
        .iter()
        .map(|line| format!("{line}{newline}"))
        .collect();
    if rng.chance(6) {
        text.truncate(text.len() - text.len().min(newline.len()));
    }
    text.into_bytes()
}

/// Lines with their terminators, as both Rosen and git's xdiff compare them.
fn lines(text: &[u8]) -> Vec<&[u8]> {
    text.split_inclusive(|&byte| byte == b'\n').collect()
}

/// Whether exactly one maximal matching of equal lines exists between `old`
/// and `new`. Then every minimal diff, Myers or xdiff, keeps the same lines.
fn alignment_is_forced(old: &[u8], new: &[u8]) -> bool {
    let (old, new) = (lines(old), lines(new));
    let width = new.len() + 1;
    // For prefixes old[..i] and new[..j]: the longest common subsequence
    // length, and how many maximal matchings reach it.
    let mut length = vec![0usize; (old.len() + 1) * width];
    let mut count = vec![1u128; (old.len() + 1) * width];
    for i in 1..=old.len() {
        for j in 1..=new.len() {
            let [here, up, left, diagonal] = [
                i * width + j,
                (i - 1) * width + j,
                i * width + j - 1,
                (i - 1) * width + j - 1,
            ];
            let equal = old[i - 1] == new[j - 1];
            let paired = if equal { length[diagonal] + 1 } else { 0 };
            length[here] = paired.max(length[up]).max(length[left]);
            let reaching = |at: usize| {
                if length[at] == length[here] {
                    count[at]
                } else {
                    0
                }
            };
            // A maximal matching pairs the two last lines, or leaves the last
            // old line unpaired, or the last new line; it cannot pair each
            // with an earlier line, as those pairs would cross. The last two
            // sets overlap in the matchings that leave both unpaired.
            let pairs_last = if equal && paired == length[here] {
                count[diagonal]
            } else {
                0
            };
            count[here] = pairs_last + reaching(up) + reaching(left) - reaching(diagonal);
        }
    }
    count[old.len() * width + new.len()] == 1
}

/// Runs `git merge-file --diff3` with Rosen's default marker labels. Returns
/// the merged text and the number of conflicts (git's exit code).
fn git_merge_file(dir: &Path, case: &Case) -> (Vec<u8>, usize) {
    for (name, text) in ["base", "ours", "theirs"].iter().zip(&case.files) {
        std::fs::write(dir.join(name), text).expect("write merge input");
    }
    let labels = ["-L", "ours", "-L", "base", "-L", "theirs"];
    let output = Command::new("git")
        .args(["merge-file", "--stdout", "--diff3"])
        .args(labels)
        .args(["ours", "base", "theirs"])
        .current_dir(dir)
        .output()
        .expect("git must be on PATH to run the differential merge test");
    let conflicts = output
        .status
        .code()
        .and_then(|code| usize::try_from(code).ok())
        .filter(|&code| code < 128)
        .unwrap_or_else(|| {
            panic!(
                "git merge-file failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    (output.stdout, conflicts)
}

/// Compares one case. Returns a reproducible description on disagreement.
fn compare(dir: &Path, id: &str, case: &Case) -> Option<String> {
    let [base, ours, theirs] = &case.files;
    let merge = merge_text(base, ours, theirs);
    let rosen = (
        merge.render(MarkerLabels::default()),
        merge.conflicts().count(),
    );
    let git = git_merge_file(dir, case);
    (rosen != git).then(|| {
        let show = |bytes: &[u8]| format!("{:?}", String::from_utf8_lossy(bytes));
        format!(
            "{id}\n  base   {}\n  ours   {}\n  theirs {}\n  git    {} ({} conflicts)\n  rosen  {} ({} conflicts)",
            show(base), show(ours), show(theirs), show(&git.0), git.1, show(&rosen.0), rosen.1
        )
    })
}

/// Runs every case through both merges on parallel workers, each with its own
/// directory for git's input files. Returns the disagreements.
fn compare_all(cases: &[(String, Case)]) -> Vec<String> {
    let chunk = cases.len().div_ceil(WORKERS).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = cases
            .chunks(chunk)
            .map(|batch| {
                scope.spawn(move || {
                    let dir = tempfile::tempdir().expect("temp dir");
                    batch
                        .iter()
                        .filter_map(|(id, case)| compare(dir.path(), id, case))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("worker"))
            .collect()
    })
}

#[test]
fn merge_text_matches_git_merge_file() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut cases = Vec::new();
    for (corpus, name) in [
        (Corpus::DistinctLines, "distinct"),
        (Corpus::RepeatedLines, "repeated"),
    ] {
        let mut compared = 0;
        for number in 0..MAX_DRAWS_PER_CORPUS {
            if compared == COMPARED_PER_CORPUS {
                break;
            }
            let case = random_case(&mut rng, corpus);
            let [base, ours, theirs] = &case.files;
            if alignment_is_forced(base, ours) && alignment_is_forced(base, theirs) {
                cases.push((format!("{name} case {number}"), case));
                compared += 1;
            }
        }
        assert!(
            compared == COMPARED_PER_CORPUS,
            "{name} corpus: only {compared} cases have forced alignments"
        );
    }
    let failures = compare_all(&cases);
    assert!(
        failures.is_empty(),
        "{} of {} cases differ from git merge-file:\n{}",
        failures.len(),
        cases.len(),
        failures
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
