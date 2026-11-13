use rosen_core::ObjectId;
use rosen_protocol::BisectResult;
use rosen_server::{bisect, metadata};

fn main() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("metadata.sqlite");
    let mut conn = metadata::open(&path).unwrap();
    for index in 0..=16u8 {
        conn.execute(
            "INSERT INTO base_history (ref_name,commit_id,change_id,tier)
            VALUES ('main',?1,?2,'normal')",
            rusqlite::params![
                ObjectId::from_bytes([index; 20]).to_string(),
                format!("change-{index}")
            ],
        )
        .unwrap();
    }
    let mut state = bisect::begin(
        &mut conn,
        "main",
        ObjectId::from_bytes([0; 20]),
        ObjectId::from_bytes([16; 20]),
        "batch failure",
    )
    .unwrap();
    while let Some(job) = state.job {
        let index = job.commit.as_bytes()[0];
        println!("checking commit {index}, step {}", job.step);
        state = bisect::submit(
            &mut conn,
            &BisectResult {
                job,
                passed: index < 7,
                output: "injected failure".into(),
            },
        )
        .unwrap();
        drop(conn);
        conn = metadata::open(&path).unwrap();
        state = bisect::next(&conn, state.id).unwrap();
    }
    let culprit = state.culprit.unwrap();
    assert_eq!(culprit.commit, ObjectId::from_bytes([7; 20]));
    println!(
        "confirmed {} with parent {} passing",
        culprit.change, culprit.parent
    );
}
