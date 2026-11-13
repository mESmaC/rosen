//! Risk (SPEC §2 "Overlap", R6, R8): landing rebases and notifies exactly
//! the changes that overlap a landed one. A missed overlap leaves a change
//! building on a stale base unnoticed; a false one rebases unrelated work.
//! Runs real snapshots through the server, including a stacked change whose
//! edits must be carried onto the base's lines before the 3-line test.
use rosen_core::{AgentId, ChangeId, Commit, FileMode, PathPattern, Signature, Tree, TreeEntry};
use rosen_protocol::{ClaimRequest, NewChange, Request, WorkspaceBytes, WorkspaceWrite};
use rosen_server::changes::{BaseSource, BaseTip};
use rosen_server::overlaps::{find, Subject};
use rosen_server::{rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

struct Base(BaseTip);
impl BaseSource for Base {
    fn base_tip(&self) -> Result<BaseTip, String> {
        Ok(self.0)
    }
}

fn lines(edited: &[usize]) -> Vec<u8> {
    let mut text = String::new();
    for line in 1..=20 {
        let mark = if edited.contains(&line) {
            "edited"
        } else {
            "line"
        };
        text.push_str(&format!("{mark} {line}\n"));
    }
    text.into_bytes()
}

fn setup() -> (tempfile::TempDir, Arc<ServerState>) {
    let temp = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: temp.path().into(),
        endpoint: rosen_protocol::Endpoint::new("overlap-contract"),
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let blob = store.put_blob(&lines(&[])).unwrap();
    let entry = TreeEntry::new(FileMode::Regular, "lib.rs", blob).unwrap();
    let tree = store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap();
    let who = Signature::new("test", "test@test", 0);
    let commit = store
        .put_commit(&Commit {
            tree,
            parents: vec![],
            author: who.clone(),
            committer: who,
            extra_headers: vec![],
            message: b"base\n".to_vec(),
        })
        .unwrap();
    state
        .changes
        .set_base_source(Arc::new(Base(BaseTip { commit, tree })));
    (temp, state)
}

async fn change(state: &Arc<ServerState>, on: Option<ChangeId>, edit: Option<Vec<u8>>) -> ChangeId {
    let owner = AgentId::new("agent").unwrap();
    let on = on.map(|id| id.to_string());
    let id = state
        .changes
        .create(&NewChange { owner, on })
        .unwrap()
        .change
        .id;
    if let Some(data) = edit {
        let write = Request::WorkspaceWrite(WorkspaceWrite {
            change: id.to_string(),
            path: "lib.rs".into(),
            data: Some(WorkspaceBytes(data)),
        });
        dispatch(state, write).await;
    }
    id
}

async fn record(
    state: &Arc<ServerState>,
    kind: fn(ClaimRequest) -> Request,
    id: ChangeId,
    path: &str,
) {
    let request = ClaimRequest {
        change: id.to_string(),
        paths: vec![path.into()],
    };
    dispatch(state, kind(request)).await;
}

fn overlaps(state: &ServerState, subject: Subject) -> Vec<ChangeId> {
    let found = find(state, &subject).unwrap();
    found.into_iter().map(|overlap| overlap.change).collect()
}

fn sorted(mut ids: Vec<ChangeId>) -> Vec<ChangeId> {
    ids.sort();
    ids
}

#[tokio::test]
async fn edits_within_three_lines_overlap_and_farther_ones_do_not() {
    let (_temp, state) = setup();
    let line5 = change(&state, None, Some(lines(&[5]))).await;
    // Lines 6-8 sit between: exactly 3 lines apart.
    let line9 = change(&state, None, Some(lines(&[9]))).await;
    // Lines 10-14 sit between it and line 9: 5 lines, too far.
    let line15 = change(&state, None, Some(lines(&[15]))).await;
    // Stacked on line5: its parent's file differs from the base's, so its
    // line 12 edit is compared in the base's lines: 2 lines from 9 and 15.
    let stacked = change(&state, Some(line5), Some(lines(&[5, 12]))).await;
    let docs = change(&state, None, None).await;
    let guide = change(&state, None, None).await;
    record(&state, Request::ClaimPaths, docs, "docs/**").await;
    // The claim above refuses a second one, so this change shares the path.
    record(&state, Request::SharePaths, guide, "docs/guide.md").await;

    let of = |id: ChangeId| overlaps(&state, Subject::Change(id.to_string()));
    assert_eq!(of(line5), vec![line9]);
    assert_eq!(of(line9), sorted(vec![line5, stacked]));
    assert_eq!(of(line15), vec![stacked]);
    assert_eq!(of(stacked), sorted(vec![line9, line15]));
    assert_eq!(of(docs), vec![guide]);

    let path = |text: &str| Subject::Paths(vec![PathPattern::new(text).unwrap()]);
    let editors = sorted(vec![line5, line9, line15, stacked]);
    assert_eq!(overlaps(&state, path("lib.rs")), editors);
    assert_eq!(
        overlaps(&state, path("docs/guide.md")),
        sorted(vec![docs, guide])
    );
    assert!(overlaps(&state, path("README.md")).is_empty());
    reverted_regions(&state, line5).await;
}

async fn reverted_regions(state: &Arc<ServerState>, line5: ChangeId) {
    // R6 risk: stale region state would keep a reverted edit overlapping.
    let id = line5.to_string();
    let response = dispatch(state, Request::EditRegions { change: id.clone() }).await;
    let rosen_protocol::Outcome::Ok(rosen_protocol::Response::EditRegions(wire)) = response else {
        panic!("{response:?}")
    };
    assert_eq!(
        wire.files[0].regions,
        rosen_protocol::Regions::Lines(vec![rosen_protocol::EditSpan { start: 4, end: 5 },])
    );
    let view = rosen_server::overlaps::edits::read(state, &id).unwrap();
    assert_eq!(
        view.files["lib.rs"].regions,
        rosen_merge::FileEdit::Lines(vec![rosen_merge::LineSpan { start: 4, end: 5 },])
    );
    dispatch(
        state,
        Request::WorkspaceWrite(WorkspaceWrite {
            change: id.clone(),
            path: "lib.rs".into(),
            data: Some(WorkspaceBytes(lines(&[]))),
        }),
    )
    .await;
    assert!(rosen_server::overlaps::edits::read(state, &id)
        .unwrap()
        .files
        .is_empty());
    let restarted = ServerState::new(state.config.clone());
    assert!(rosen_server::overlaps::edits::read(&restarted, &id)
        .unwrap()
        .files
        .is_empty());
}
