//! Checks `merge_text` against the fixture table exported from the Lean model
//! (`proofs/ThreeWayMerge/Fixtures.lean`).
//!
//! Risk (SPEC R5, §7): the proof covers the model, not the Rust code. If the
//! two cut or resolve chunks differently, a merge can silently drop or
//! duplicate lines while the proof still checks.

use rosen_merge::{Hunk, merge_text};

/// One fixture character per line, as the model writes them.
fn to_lines(column: &str) -> Vec<u8> {
    column
        .chars()
        .flat_map(|line| [line as u8, b'\n'])
        .collect()
}

fn from_lines(text: &[u8]) -> String {
    text.iter()
        .filter(|&&byte| byte != b'\n')
        .map(|&byte| byte as char)
        .collect()
}

#[test]
fn merge_matches_lean_model_fixtures() {
    let table = include_str!("../../../proofs/fixtures/three-way-merge.tsv");
    let mut rows = 0;
    for row in table.lines().filter(|row| !row.is_empty()) {
        let [base, ours, theirs, expected]: [&str; 4] = row
            .split('\t')
            .collect::<Vec<_>>()
            .try_into()
            .expect("four columns");
        let merge = merge_text(&to_lines(base), &to_lines(ours), &to_lines(theirs));
        let actual: String = merge
            .hunks
            .iter()
            .map(|hunk| match hunk {
                Hunk::Clean(text) => from_lines(text),
                Hunk::Conflict(region) => {
                    let [o, b, t] =
                        [&region.ours, &region.base, &region.theirs].map(|side| from_lines(side));
                    format!("<{o}|{b}|{t}>")
                }
            })
            .collect();
        assert_eq!(actual, expected, "base={base} ours={ours} theirs={theirs}");
        rows += 1;
    }
    assert!(rows > 0, "fixture table is empty");
}
