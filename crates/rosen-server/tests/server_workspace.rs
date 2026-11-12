//! Risk (SPEC R3/D17): concurrent direct file writes must both survive in
//! stored snapshots, and an edit after a rebase must retain the rebased head.
use rosen_core::{AgentId, ChangeParent, Commit, Signature, Tree};
use rosen_protocol::{
    DiffRequest, NewChange, Outcome, ReadAnswer, ReadRequest, Request, Response, WorkspaceBytes,
    WorkspaceWrite,
};
use rosen_server::{
    changes::{self, BaseSource, BaseTip},
    rpc::dispatch,
    ServerConfig, ServerState,
};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

struct Base(BaseTip);
impl BaseSource for Base {
    fn base_tip(&self) -> Result<BaseTip, String> {
        Ok(self.0)
    }
}

fn setup() -> (tempfile::TempDir, Arc<ServerState>, String, BaseTip) {
    let temp = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: temp.path().into(),
        endpoint: rosen_protocol::Endpoint::new("workspace-contract"),
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let tree = store.put_tree(&Tree::default()).unwrap();
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
    let tip = BaseTip { commit, tree };
    state.changes.set_base_source(Arc::new(Base(tip)));
    let id = state
        .changes
        .create(&NewChange {
            owner: AgentId::new("editor").unwrap(),
            on: None,
        })
        .unwrap()
        .change
        .id
        .to_string();
    (temp, state, id, tip)
}

async fn write(state: &Arc<ServerState>, id: &str, path: &str, data: Option<&[u8]>) -> Outcome {
    let request = Request::WorkspaceWrite(WorkspaceWrite {
        change: id.into(),
        path: path.into(),
        data: data.map(|d| WorkspaceBytes(d.into())),
    });
    // Exercise the wire encoding too: binary bytes must not be decoded as text.
    let wire = serde_json::to_vec(&request).unwrap();
    dispatch(state, serde_json::from_slice(&wire).unwrap()).await
}

async fn read(state: &Arc<ServerState>, id: &str, path: &str) -> ReadAnswer {
    match dispatch(
        state,
        Request::ReadFile(ReadRequest {
            at: Some(id.into()),
            path: path.into(),
        }),
    )
    .await
    {
        Outcome::Ok(Response::File(answer)) => answer,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn concurrent_edits_survive_and_follow_rebases_without_checkout() {
    let (_temp, state, id, tip) = setup();
    write_pair(&state, &id).await;
    assert!(
        matches!(read(&state, &id, "dir").await, ReadAnswer::Directory { entries, .. } if entries.len()==2)
    );
    assert!(
        matches!(read(&state, &id, "dir/b").await, ReadAnswer::File { data, .. } if data == [0,255])
    );
    let before = state.changes.show(&id).unwrap().change;
    let diff = dispatch(
        &state,
        Request::Diff(DiffRequest {
            to: Some(id.clone()),
            ..Default::default()
        }),
    )
    .await;
    assert!(matches!(diff, Outcome::Ok(Response::Diff(d)) if d.files.len()==2));
    state
        .changes
        .with_conn(|conn| {
            changes::rebase(conn, before.id, ChangeParent::Base(tip.commit), tip.tree)
        })
        .unwrap();
    // Recreating an earlier snapshot tree after a rebase must move the head.
    write_pair(&state, &id).await;
    let after = state.changes.show(&id).unwrap().change;
    assert_eq!(after.head, before.head);
    assert_eq!(&after.snapshots[..2], &before.snapshots);
    assert!(matches!(
        write(&state, &id, "dir/a", None).await,
        Outcome::Ok(Response::Snapshot(_))
    ));
    assert!(
        matches!(read(&state, &id, "dir").await, ReadAnswer::Directory {entries, ..} if entries.len()==1)
    );
    state.changes.abandon(&id).unwrap();
    assert!(matches!(
        write(&state, &id, "dir/a", Some(b"x")).await,
        Outcome::Error(_)
    ));
}

async fn write_pair(state: &Arc<ServerState>, id: &str) {
    let (a, b) = tokio::join!(
        write(state, id, "dir/a", Some(b"a\n")),
        write(state, id, "dir/b", Some(&[0, 255]))
    );
    for answer in [a, b] {
        assert!(matches!(answer, Outcome::Ok(Response::Snapshot(s)) if s.recorded));
    }
}
