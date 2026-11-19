//! Checks `merge_text` against the fixture table exported from the Lean model
//! (`proofs/ThreeWayMerge/Fixtures.lean`).
//!
//! Risk (SPEC R5, §7): the proof covers the model, not the Rust code. If the
//! two cut or resolve chunks differently, a merge can silently drop or
//! duplicate lines while the proof still checks.

use rosen_merge::{merge_file, merge_text, FileMerge, Hunk, MergePolicy};

/// One fixture character per line, as the model writes them.
fn to_lines(column: &str, newline: &str) -> Vec<u8> {
    column
        .bytes()
        .flat_map(|line| std::iter::once(line).chain(newline.bytes()))
        .collect()
}

fn from_lines(text: &[u8], newline: &str) -> String {
    text.split_inclusive(|&byte| byte == b'\n')
        .map(|line| {
            let body = line.strip_suffix(newline.as_bytes()).expect("base newline");
            assert_eq!(body.len(), 1, "one fixture character per line");
            body[0] as char
        })
        .collect()
}

fn fixture_result(columns: [&str; 3], endings: [&str; 3]) -> String {
    let files = std::array::from_fn::<_, 3, _>(|i| to_lines(columns[i], endings[i]));
    let newline = (0..3)
        .find(|&i| !files[i].is_empty())
        .map_or(endings[0], |i| endings[i]);
    merge_text(&files[0], &files[1], &files[2])
        .hunks
        .iter()
        .map(|hunk| match hunk {
            Hunk::Clean(text) => from_lines(text, newline),
            Hunk::Conflict(region) => {
                let [o, b, t] = [&region.ours, &region.base, &region.theirs]
                    .map(|side| from_lines(side, newline));
                format!("<{o}|{b}|{t}>")
            }
        })
        .collect()
}

#[test]
fn merge_matches_lean_model_fixtures() {
    let table = include_str!("../../../../proofs/fixtures/three-way-merge.tsv");
    let mut rows = 0;
    for row in table.lines().filter(|row| !row.is_empty()) {
        let [base, ours, theirs, expected]: [&str; 4] = row
            .split('\t')
            .collect::<Vec<_>>()
            .try_into()
            .expect("four columns");
        // R5 risk (#391): a newline rewrite must not erase the model's anchors.
        for mask in 0..8 {
            let endings = std::array::from_fn(|i| if mask & (1 << i) == 0 { "\n" } else { "\r\n" });
            let actual = fixture_result([base, ours, theirs], endings);
            assert_eq!(actual, expected, "row={row:?} endings={endings:?}");
        }
        rows += 1;
    }
    assert!(rows > 0, "fixture table is empty");
}

#[test]
fn newline_rewrite_keeps_modules_below_crate_docs() {
    // R5 risk (#391): misplaced registrations make the landed crate fail to compile.
    let base = "//! Crate docs\n\npub mod alpha;\npub mod beta;\npub mod omega;";
    let ours = base.replace("pub mod alpha;", "pub mod alpha;\npub mod ours;");
    let theirs = base.replace("pub mod omega;", "pub mod theirs;\npub mod omega;");
    let expected = ours.replace("pub mod omega;", "pub mod theirs;\npub mod omega;");
    for newline in ["\n", "\r\n"] {
        let base = base.replace('\n', newline).into_bytes();
        let ours = ours.replace('\n', "\r\n").into_bytes();
        let theirs = theirs.as_bytes();
        for policy in [MergePolicy::Text, MergePolicy::RegistrationList] {
            let merged = merge_file(Some(&base), Some(&ours), Some(theirs), policy);
            assert_eq!(
                merged,
                FileMerge::Resolved(Some(expected.replace('\n', newline).into_bytes()))
            );
        }
    }
}
