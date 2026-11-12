//! Risk: a wrong glob overlap test lets two changes both own a path, or
//! refuses a claim no path could share (SPEC R6). Every row of the table
//! exported from `proofs/Claims/Export.lean`, which decides overlap by brute
//! force over paths, must match `PathPattern::overlaps` and `blockers`.

use rosen_core::{blockers, ChangeId, HeldClaim, PathPattern};

#[test]
fn claims_match_the_lean_fixture_table() {
    let table = include_str!("../../../proofs/fixtures/claims.tsv");
    let holder: ChangeId = "k".repeat(32).parse().expect("valid change id");
    let claimant: ChangeId = "l".repeat(32).parse().expect("valid change id");
    let mut rows = 0;
    for line in table.lines().skip(1) {
        let cells: Vec<&str> = line.split('\t').collect();
        let [held, requested, overlap, granted] = cells[..] else {
            panic!("bad row {line:?}");
        };
        let held = PathPattern::new(held).expect("fixture pattern");
        let requested = PathPattern::new(requested).expect("fixture pattern");
        assert_eq!(held.overlaps(&requested), overlap == "1", "overlap: {line}");
        let claims = [HeldClaim {
            change: holder,
            pattern: held,
        }];
        let refused = !blockers(claimant, std::slice::from_ref(&requested), &claims).is_empty();
        assert_eq!(!refused, granted == "1", "granted: {line}");
        rows += 1;
    }
    assert!(rows > 0, "the fixture table is empty");
}
