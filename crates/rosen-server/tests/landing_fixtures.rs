//! Risk: the Rust land decision drifting from the proven Lean model would let
//! the server land unbuilt or unapproved code (SPEC R8). Every row of the
//! table exported from `proofs/Landing/Design.lean` must match `decide`.

use rosen_server::landing::rules::{decide, LandInput, ReceiptCheck};
use rosen_server::landing::{ChangeState, Tier};

fn bit(cell: &str) -> bool {
    match cell {
        "0" => false,
        "1" => true,
        other => panic!("bad bit {other}"),
    }
}

fn states(cell: &str) -> Vec<ChangeState> {
    match cell {
        // The model's `active` stands for both `open` and `ready`.
        "active" => vec![ChangeState::Open, ChangeState::Ready],
        "landed" => vec![ChangeState::Landed],
        "abandoned" => vec![ChangeState::Abandoned],
        other => panic!("bad state {other}"),
    }
}

fn receipt(cell: &str) -> ReceiptCheck {
    match cell {
        "missing" => ReceiptCheck::Missing,
        "invalid" => ReceiptCheck::Invalid,
        "valid" => ReceiptCheck::Valid,
        other => panic!("bad receipt {other}"),
    }
}

#[test]
fn decide_matches_lean_fixture_table() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../proofs/fixtures/landing.tsv"
    );
    let table = std::fs::read_to_string(path).expect("fixture table");
    let mut checked = 0;
    for line in table.lines().skip(1) {
        let cells: Vec<&str> = line.split('\t').collect();
        assert_eq!(cells.len(), 10, "row {line}");
        for state in states(cells[0]) {
            let input = LandInput {
                state,
                parent_landed: bit(cells[1]),
                has_conflicts: bit(cells[2]),
                approved: bit(cells[3]),
                tier: Tier::parse(cells[4]).expect("tier"),
                receipt: receipt(cells[5]),
                base_moved: bit(cells[6]),
                landed_since_all_independent: bit(cells[7]),
                merge_clean: bit(cells[8]),
            };
            assert_eq!(decide(&input).code(), cells[9], "row {line} as {state:?}");
            checked += 1;
        }
    }
    assert!(checked > 1000, "fixture table too small: {checked}");
}
