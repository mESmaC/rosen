//! Risk (SPEC R12): choosing the wrong culprit reverts another owner's work.
use rosen_core::ObjectId;
use rosen_protocol::{BisectResult, ErrorCode};
use rosen_server::{bisect, metadata};
use std::path::{Path, PathBuf};

/// Rows replay on this many databases at once. Every row still runs; one
/// database per shard keeps their sessions apart.
const SHARDS: usize = 8;

fn oid(index: usize) -> ObjectId {
    ObjectId::from_bytes([index as u8; 20])
}

/// Opens the metadata as the server does, then stops its fsyncs. The table
/// checks the search and its resumption on a new connection, not power-loss
/// durability, and an fsync per commit made the replay take over 100 s on
/// Windows (#423).
fn open(path: &Path) -> rusqlite::Connection {
    let conn = metadata::open(path).unwrap();
    conn.pragma_update(None, "synchronous", "OFF").unwrap();
    conn
}

/// A database whose landed history is commits 0 to 20, each from its own change.
fn history(dir: &Path) -> (PathBuf, rusqlite::Connection) {
    let path = dir.join("metadata.sqlite");
    let conn = open(&path);
    for index in 0..=20 {
        conn.execute(
            "INSERT INTO base_history(ref_name,commit_id,change_id,tier)
            VALUES ('main',?1,?2,'normal')",
            rusqlite::params![oid(index).to_string(), format!("change-{index}")],
        )
        .unwrap();
    }
    (path, conn)
}

#[test]
fn bisection_fixtures() {
    let rows: Vec<&str> = include_str!("../../../../proofs/fixtures/bisection.tsv")
        .lines()
        .skip(1)
        .collect();
    std::thread::scope(|scope| {
        for shard in 0..SHARDS {
            let rows = &rows;
            scope.spawn(move || {
                let temp = tempfile::tempdir().unwrap();
                let (path, mut conn) = history(temp.path());
                for row in rows.iter().skip(shard).step_by(SHARDS) {
                    check_row(&mut conn, &path, row);
                }
            });
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let (_path, mut conn) = history(temp.path());
    let state = bisect::begin(&mut conn, "main", oid(0), oid(1), "failed once").unwrap();
    let flaky = bisect::submit(
        &mut conn,
        &BisectResult {
            job: state.job.unwrap(),
            passed: true,
            output: "flaky".into(),
            submitter: None,
        },
    )
    .unwrap();
    assert!(flaky.flaky && flaky.culprit.is_none() && flaky.job.is_none());
}

/// Bisects one row's range to its culprit. Each answer is refused when sent
/// twice, and the search resumes on a new connection after every step.
fn check_row(conn: &mut rusqlite::Connection, path: &Path, row: &str) {
    let v: Vec<usize> = row.split('\t').map(|x| x.parse().unwrap()).collect();
    conn.execute("DELETE FROM bisections", []).ok();
    let mut state = bisect::begin(conn, "main", oid(v[0]), oid(v[1]), "batch").unwrap();
    assert_eq!(state.job.as_ref().unwrap().commit, oid(v[3]), "{row}");
    let mut first = true;
    while let Some(job) = state.job {
        let result = BisectResult {
            passed: job.commit < oid(v[2]),
            job,
            output: "failure".into(),
            submitter: None,
        };
        state = bisect::submit(conn, &result).unwrap();
        assert_eq!(
            bisect::submit(conn, &result).unwrap_err().code,
            ErrorCode::Refused
        );
        if first {
            let bounds: (usize, usize) = conn
                .query_row("SELECT low,high FROM bisections", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap();
            assert_eq!((bounds.0 + v[0], bounds.1 + v[0]), (v[4], v[5]), "{row}");
            first = false;
        }
        *conn = open(path);
        state = bisect::next(conn, state.id).unwrap();
    }
    let culprit = state.culprit.unwrap();
    assert_eq!(culprit.commit, oid(v[2]), "{row}");
    assert_eq!(culprit.parent, oid(v[2] - 1), "{row}");
    assert_eq!(culprit.change, format!("change-{}", v[2]));
}
