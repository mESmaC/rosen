//! Risk: a mismatch with the proven R8 interval policy can reuse a receipt
//! across a shared build input, allowing unchecked code to land.
use rosen_server::landing::reach::{independent, Reach};

fn reach(cell: &str) -> Reach {
    cell.split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn interval_reuse_matches_proven_fixtures() {
    let table = include_str!("../../../proofs/fixtures/recheck.tsv");
    let mut checked = 0;
    for row in table.lines().skip(1) {
        let cells: Vec<_> = row.split('\t').collect();
        assert_eq!(cells.len(), 4);
        let ours = reach(cells[0]);
        let reuse = independent(&ours, &reach(cells[1])) && independent(&ours, &reach(cells[2]));
        assert_eq!(reuse, cells[3] == "1", "{row}");
        checked += 1;
    }
    assert_eq!(checked, 216);
}
