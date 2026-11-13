//! Risk (SPEC R12): choosing the wrong culprit reverts another owner's work.
use rosen_core::ObjectId;
use rosen_protocol::{BisectResult, ErrorCode};
use rosen_server::{bisect, metadata};

fn oid(index: usize) -> ObjectId {
    ObjectId::from_bytes([index as u8; 20])
}

#[test]
fn bisection_fixtures() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("metadata.sqlite");
    let mut conn = metadata::open(&path).unwrap();
    for index in 0..=20 {
        conn.execute(
            "INSERT INTO base_history(ref_name,commit_id,change_id,tier)
            VALUES ('main',?1,?2,'normal')",
            rusqlite::params![oid(index).to_string(), format!("change-{index}")],
        )
        .unwrap();
    }
    for row in include_str!("../../../proofs/fixtures/bisection.tsv")
        .lines()
        .skip(1)
    {
        let v: Vec<usize> = row.split('\t').map(|x| x.parse().unwrap()).collect();
        conn.execute("DELETE FROM bisections", []).ok();
        let mut state = bisect::begin(&mut conn, "main", oid(v[0]), oid(v[1]), "batch").unwrap();
        assert_eq!(state.job.as_ref().unwrap().commit, oid(v[3]), "{row}");
        let mut first = true;
        while let Some(job) = state.job {
            let result = BisectResult {
                passed: job.commit < oid(v[2]),
                job,
                output: "failure".into(),
            };
            state = bisect::submit(&mut conn, &result).unwrap();
            assert_eq!(
                bisect::submit(&mut conn, &result).unwrap_err().code,
                ErrorCode::Refused
            );
            if first {
                let bounds: (usize, usize) = conn
                    .query_row("SELECT low,high FROM bisections", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .unwrap();
                assert_eq!((bounds.0 + v[0], bounds.1 + v[0]), (v[4], v[5]));
                first = false;
            }
            drop(conn);
            conn = metadata::open(&path).unwrap();
            state = bisect::next(&conn, state.id).unwrap();
        }
        let culprit = state.culprit.unwrap();
        assert_eq!(culprit.commit, oid(v[2]));
        assert_eq!(culprit.parent, oid(v[2] - 1));
        assert_eq!(culprit.change, format!("change-{}", v[2]));
    }
    conn.execute("DELETE FROM bisections", []).unwrap();
    let state = bisect::begin(&mut conn, "main", oid(0), oid(1), "failed once").unwrap();
    let flaky = bisect::submit(
        &mut conn,
        &BisectResult {
            job: state.job.unwrap(),
            passed: true,
            output: "flaky".into(),
        },
    )
    .unwrap();
    assert!(flaky.flaky && flaky.culprit.is_none() && flaky.job.is_none());
}
