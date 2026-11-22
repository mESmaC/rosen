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
    assert!(
        rows.iter().any(|r| r.starts_with("platform\t")),
        "expected platform rows in bisection.tsv"
    );
    assert!(
        rows.iter().any(|r| {
            r.strip_prefix("platform\t")
                .is_some_and(|rest| rest.split('\t').nth(9).is_some_and(|pos| pos != "0"))
        }),
        "expected known-batch platform rows in bisection.tsv"
    );
    std::thread::scope(|scope| {
        for shard in 0..SHARDS {
            let rows = &rows;
            scope.spawn(move || {
                let temp = tempfile::tempdir().unwrap();
                let (path, mut conn) = history(temp.path());
                for row in rows.iter().skip(shard).step_by(SHARDS) {
                    if let Some(mirror_row) = row.strip_prefix("mirror\t") {
                        check_mirror_row(&mut conn, mirror_row);
                    } else if let Some(platform_row) = row.strip_prefix("platform\t") {
                        check_platform_row(&mut conn, &path, platform_row);
                    } else {
                        check_row(&mut conn, &path, row);
                    }
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
            durations: None,
        },
    )
    .unwrap();
    assert!(flaky.flaky && flaky.culprit.is_none() && flaky.job.is_none());
}

// R1/R12: adoption commits must not be offered as potential culprits.
fn check_mirror_row(conn: &mut rusqlite::Connection, row: &str) {
    let cells: Vec<_> = row.split('\t').collect();
    let parse = |ids: &str| {
        ids.split(',')
            .map(|id| oid(id.parse().unwrap()))
            .collect::<Vec<_>>()
    };
    let low = oid(cells[0].parse().unwrap());
    let high = oid(cells[1].parse().unwrap());
    let mirrors = parse(cells[2]);
    for mirror in &mirrors {
        conn.execute(
            "UPDATE base_history SET tier='mirror' WHERE commit_id=?1",
            [mirror.to_string()],
        )
        .unwrap();
    }
    conn.execute("DELETE FROM bisections", []).ok();
    let state = bisect::begin(conn, "main", low, high, "batch").unwrap();
    let captured: String = conn
        .query_row(
            "SELECT commits FROM bisections WHERE id=?1",
            [state.id],
            |r| r.get(0),
        )
        .unwrap();
    let commits: Vec<(ObjectId, Option<String>)> = serde_json::from_str(&captured).unwrap();
    let actual: Vec<_> = commits.into_iter().map(|(commit, _)| commit).collect();
    assert_eq!(actual, parse(cells[3]), "{row}");
    for mirror in &mirrors {
        conn.execute(
            "UPDATE base_history SET tier='normal' WHERE commit_id=?1",
            [mirror.to_string()],
        )
        .unwrap();
    }
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
            durations: None,
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

fn parse_kind(cell: &str) -> rosen_protocol::CheckKind {
    match cell {
        "build" => rosen_protocol::CheckKind::Build,
        "test" => rosen_protocol::CheckKind::Test,
        other => panic!("bad check kind {other}"),
    }
}

fn seed_known_batch(
    conn: &rusqlite::Connection,
    cp: usize,
    culprit: (usize, &str, &str),
    known: (usize, &str, &str),
    row: &str,
) {
    let (rec_pos, rec_kind, rec_plat) = culprit;
    let (known_pos, known_kind, known_plat) = known;
    if known_pos == 0 {
        return;
    }
    let k_kind = parse_kind(known_kind);
    let k_plat = (!known_plat.is_empty()).then_some(known_plat);
    if rec_pos > 0 && (known_kind, known_plat) == (rec_kind, rec_plat) {
        let covered = bisect::begin_for_in(
            conn,
            "main",
            (oid(cp), oid(known_pos)),
            "prior failure",
            k_kind,
            k_plat,
            None,
        )
        .unwrap();
        let phase: String = conn
            .query_row(
                "SELECT phase FROM bisections WHERE id=?1",
                [covered.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "known", "{row}");
        assert!(covered.job.is_none() && covered.culprit.is_none(), "{row}");
    } else {
        // Cross-kind/platform covered batch control without a matching culprit in `s1`'s scope.
        let _ = bisect::next(conn, -1);
        let commits =
            serde_json::to_string(&vec![(oid(cp), None::<String>), (oid(known_pos), None)])
                .unwrap();
        conn.execute(
            "INSERT INTO bisections(ref_name,failed,commits,low,high,phase,step,output,kind,platform)
             VALUES ('main',?1,?2,0,1,'known',0,'prior failure',?3,?4)",
            rusqlite::params![oid(known_pos).to_hex(), commits, known_kind, known_plat],
        )
        .unwrap();
    }
}

fn seed_prior_records(
    conn: &rusqlite::Connection,
    cp: usize,
    culprit: (usize, &str, &str),
    known: (usize, &str, &str),
    row: &str,
) {
    let (rec_pos, rec_kind, rec_plat) = culprit;
    rosen_server::post_land::failures::ensure_schema(conn).unwrap();
    conn.execute("DELETE FROM bisections", []).ok();
    conn.execute("DELETE FROM post_land_failures", []).ok();
    if rec_pos > 0 {
        let req_rev = i32::from(rec_kind == "build");
        conn.execute(
            "INSERT INTO post_land_failures(session,change_id,commit_id,kind,platform,output,requires_revert)
             VALUES (999,?1,?2,?3,?4,'prior failure',?5)",
            rusqlite::params![
                format!("change-{rec_pos}"),
                oid(rec_pos).to_hex(),
                rec_kind,
                rec_plat,
                req_rev
            ],
        )
        .unwrap();
    }
    seed_known_batch(conn, cp, culprit, known, row);
}

fn drive_platform_session(
    conn: &mut rusqlite::Connection,
    path: &Path,
    mut state: rosen_protocol::BisectState,
    bounds: (usize, usize, usize, usize),
    row: &str,
) {
    let (bound, culprit_pos, next_low, next_high) = bounds;
    let mut first = true;
    while let Some(job) = state.job {
        let result = BisectResult {
            passed: job.commit < oid(culprit_pos),
            job,
            output: "failure".into(),
            submitter: None,
            durations: None,
        };
        state = bisect::submit(conn, &result).unwrap();
        if first {
            let b: (usize, usize) = conn
                .query_row(
                    "SELECT low,high FROM bisections WHERE id=?1",
                    [state.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!((b.0 + bound, b.1 + bound), (next_low, next_high), "{row}");
            first = false;
        }
        *conn = open(path);
        state = bisect::next(conn, state.id).unwrap();
    }
    let culprit = state.culprit.unwrap();
    assert_eq!(culprit.commit, oid(culprit_pos), "{row}");
    assert_eq!(culprit.parent, oid(culprit_pos - 1), "{row}");
}

fn assert_wrong_scope_refused(
    conn: &mut rusqlite::Connection,
    job: &rosen_protocol::BisectJob,
    kind2: rosen_protocol::CheckKind,
    plat2: Option<&str>,
    row: &str,
) {
    let mut wrong = job.clone();
    wrong.kind = kind2;
    wrong.platform = plat2.map(str::to_owned);
    let rejected = bisect::submit(
        conn,
        &BisectResult {
            job: wrong,
            passed: false,
            output: "wrong scope".into(),
            submitter: None,
            durations: None,
        },
    );
    assert_eq!(rejected.unwrap_err().code, ErrorCode::Refused, "{row}");
}

// R12 (#440, #461, #473): bisection sessions are keyed by (failed, kind, platform),
// isolate culprit and known-batch boundaries per scope, and refuse mismatched job scopes.
fn check_platform_row(conn: &mut rusqlite::Connection, path: &Path, row: &str) {
    let c: Vec<&str> = row.split('\t').collect();
    assert_eq!(c.len(), 19, "bad platform row: {row}");
    let (kind1, plat1) = (parse_kind(c[0]), (!c[1].is_empty()).then_some(c[1]));
    let (kind2, plat2) = (parse_kind(c[2]), (!c[3].is_empty()).then_some(c[3]));
    let n = |i: usize| c[i].parse::<usize>().unwrap();
    let (cp, failed) = (n(4), n(5));
    seed_prior_records(conn, cp, (n(6), c[7], c[8]), (n(9), c[10], c[11]), row);
    let range = (oid(cp), oid(failed));
    let s1 = bisect::begin_for_in(conn, "main", range, "batch", kind1, plat1, None).unwrap();
    let s2 = bisect::begin_for_in(conn, "main", range, "batch", kind2, plat2, None).unwrap();
    assert_eq!(s1.id == s2.id, c[13] == "1", "{row}");
    assert_eq!((s1.kind, s1.platform.as_deref()), (kind1, plat1), "{row}");
    let captured: String = conn
        .query_row("SELECT commits FROM bisections WHERE id=?1", [s1.id], |r| {
            r.get(0)
        })
        .unwrap();
    let commits: Vec<(ObjectId, Option<String>)> = serde_json::from_str(&captured).unwrap();
    let (bound, mid, next_low, next_high) = (n(15), n(16), n(17), n(18));
    assert_eq!(commits[0].0, oid(bound), "{row}");
    assert_eq!(s1.job.as_ref().unwrap().commit, oid(mid), "{row}");
    if c[14] != "1" {
        assert_wrong_scope_refused(conn, s1.job.as_ref().unwrap(), kind2, plat2, row);
    }
    drive_platform_session(conn, path, s1, (bound, n(12), next_low, next_high), row);
    conn.execute("DELETE FROM post_land_failures", []).ok();
}
