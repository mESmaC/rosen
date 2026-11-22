//! Risk: the Rust land decision drifting from the proven Lean model would let
//! the server bypass configured gates or revert an advisory or after-land test
//! failure (SPEC R7, R8, R12). Replay the fixture exported from the Lean model.

use rosen_core::{Platform, ProjectConfig, ReviewMode};
use rosen_protocol::CheckKind;

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

fn review(cell: &str) -> ReviewMode {
    match cell {
        "before-land" => ReviewMode::BeforeLand,
        "after-land" => ReviewMode::AfterLand,
        other => panic!("bad review mode {other}"),
    }
}

fn receipt(cell: &str) -> ReceiptCheck {
    match cell {
        "missing" => ReceiptCheck::Missing,
        "invalid" => ReceiptCheck::Invalid,
        "valid" => ReceiptCheck::Valid,
        "same_tree" => ReceiptCheck::SameTree,
        other => panic!("bad receipt {other}"),
    }
}

fn check_failure(cells: &[&str]) {
    let kind = if bit(cells[2]) {
        CheckKind::Build
    } else {
        CheckKind::Test
    };
    let mut config = ProjectConfig {
        review: review(cells[1]),
        ..ProjectConfig::default()
    };
    let set = match kind {
        CheckKind::Build => &mut config.checks.pre_land,
        CheckKind::Test => &mut config.checks.post_land,
    };
    let platform = match cells[3] {
        "unscoped" => None,
        "required" | "advisory" => {
            set.platforms.push(Platform::parse("windows").unwrap());
            set.platform_commands
                .insert(Platform::parse("linux").unwrap(), vec!["check".into()]);
            Some(if cells[3] == "required" {
                "windows"
            } else {
                "linux"
            })
        }
        other => panic!("bad platform role {other}"),
    };
    assert_eq!(
        rosen_server::post_land::failures::confirmed_failure_reverts(&config, kind, platform),
        bit(cells[4]),
        "row {cells:?}"
    );
}

#[test]
fn decide_matches_lean_fixture_table() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../proofs/fixtures/landing.tsv"
    );
    let table = std::fs::read_to_string(path).expect("fixture table");
    let mut checked = 0;
    let mut failures_checked = 0;
    for line in table.lines().skip(1) {
        let cells: Vec<&str> = line.split('\t').collect();
        if matches!(cells[0], "contention" | "regeneration") {
            continue; // Checked by landing_race.rs and after_land.rs.
        }
        if cells[0] == "failure" {
            failures_checked += 1;
            check_failure(&cells);
            continue;
        }
        assert_eq!(cells.len(), 12, "row {line}");
        for state in states(cells[0]) {
            let input = LandInput {
                state,
                parent_landed: bit(cells[1]),
                has_conflicts: bit(cells[2]),
                approved: bit(cells[3]),
                review: review(cells[4]),
                recheck: match cells[5] {
                    "reach" => rosen_core::RecheckMode::Reach,
                    "merge" => rosen_core::RecheckMode::Merge,
                    other => panic!("bad recheck mode {other}"),
                },
                tier: Tier::parse(cells[6]).expect("tier"),
                receipt: receipt(cells[7]),
                base_moved: bit(cells[8]),
                landed_since_all_independent: bit(cells[9]),
                merge_clean: bit(cells[10]),
            };
            assert_eq!(decide(&input).code(), cells[11], "row {line} as {state:?}");
            checked += 1;
        }
    }
    assert_eq!(failures_checked, 12);
    assert!(checked > 1000, "fixture table too small: {checked}");
}
