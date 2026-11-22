//! Risk: a mismatch with the proven R8 interval policy can reuse a receipt
//! across a shared build input, allowing unchecked code to land.
use rosen_core::ObjectId;
use rosen_server::landing::{
    reach::{independent, Reach},
    Ledger,
};

fn reach(cell: &str) -> Reach {
    cell.split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn interval_reuse_matches_proven_fixtures() {
    let table = include_str!("../../../../proofs/fixtures/recheck.tsv");
    let mut checked = 0;
    for row in table.lines().skip(1) {
        let cells: Vec<_> = row.split('\t').collect();
        assert_eq!(cells.len(), 5);
        let ours = reach(cells[0]);
        let history = [cells[1], cells[2]];
        let reuse = if history.contains(&"mirror") {
            history_reuse(&ours, history)
        } else {
            history.iter().all(|cell| independent(&ours, &reach(cell)))
        };
        assert_eq!(reuse, cells[3] == "1", "{row}");
        checked += 1;
    }
    assert_eq!(checked, 240);
}

// R1/R8: a mirror adoption must leave receipt reuse subject to every real
// land's reach, even when the adoption is the attempt's base.
fn history_reuse(ours: &Reach, history: [&str; 2]) -> bool {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("metadata.sqlite");
    let ledger = Ledger::open(&path).unwrap();
    let conn = rosen_server::metadata::open(&path).unwrap();
    let checkpoint = ObjectId::from_bytes([0; 20]);
    ledger.init_base("main", &checkpoint).unwrap();
    let after = ledger.base_seq("main", &checkpoint).unwrap().unwrap();
    let ids = [ObjectId::from_bytes([1; 20]), ObjectId::from_bytes([2; 20])];
    for (commit, cell) in ids.iter().zip(history) {
        let tier = if cell == "mirror" { "mirror" } else { "normal" };
        conn.execute(
            "INSERT INTO base_history(ref_name,commit_id,tier) VALUES ('main',?1,?2)",
            rusqlite::params![commit.to_string(), tier],
        )
        .unwrap();
    }
    ledger
        .lands_between("main", after, &ids[1])
        .unwrap()
        .into_iter()
        .all(|(id, _)| {
            let index = ids.iter().position(|commit| *commit == id).unwrap();
            assert_ne!(history[index], "mirror");
            independent(ours, &reach(history[index]))
        })
}
