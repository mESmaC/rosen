//! SPEC.md §5 contract (D7): Velocity runs an agent's whole flow through
//! `rosen-client` alone: channel join and piece claim, workspaces (one
//! stacked), path claims, messages, review, receipts, land, rebases and
//! events, with no git worktree, GitHub PR or local AIRC file.
//!
//! Risk: a step Velocity cannot reach through a session sends it back to
//! raw requests with hand-filled identities, or to git and GitHub, for every
//! agent it launches. This runs every step as Velocity calls it.

use rosen_client::protocol::channel::{Intent, PieceOutcome};
use rosen_client::protocol::{
    CommentTarget, EventKind, LandStatus, Listener, ReadAnswer, ReadRequest,
};
use rosen_client::{AgentId, ChangeState, ChannelMembership, Endpoint, Session, WorkspaceSpec};
use rosen_core::receipt::PRE_LAND_CHECK_SET;
use rosen_core::{Commit, FileMode, Signature, Tree, TreeEntry};
use rosen_server::landing::Ledger;
use rosen_server::{ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::path::Path;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_goes_from_claim_to_land_through_sessions_alone() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = start_server(&dir.path().join("data"));
    let author = open(&endpoint, "author").await;
    let reviewer = author.as_agent(AgentId::new("reviewer").unwrap());
    let mine = author.join_channel("thread-1", "machine-a").await.unwrap();
    let theirs = reviewer.join_channel("thread-2", "machine-b").await;
    let theirs = theirs.unwrap();
    let (parent, child) = claim_and_stack(&mine, &theirs, dir.path()).await;

    // Review in place of a PR: request it, comment, resolve.
    let ask = "please review";
    let sent = mine.send(&["reviewer"], Intent::Request, ask).await;
    sent.unwrap();
    let inbox = theirs.unread().await.unwrap();
    assert!(inbox.iter().any(|message| message.text == ask));
    let line = CommentTarget::Line {
        path: "main.rs".into(),
        line: 1,
    };
    let thread = reviewer.comment(&parent, line, "ok", None).await.unwrap();
    author.resolve_thread(thread.id).await.unwrap();

    land_stack(&reviewer, &author, &parent, &child, dir.path()).await;

    // Velocity's teardown: workspaces go, the piece is released.
    author
        .delete_workspace(&dir.path().join("work"))
        .await
        .unwrap();
    author
        .delete_workspace(&dir.path().join("child"))
        .await
        .unwrap();
    assert_eq!(mine.release_piece(None).await.unwrap().len(), 1);
    let left = author.my_changes(Some(ChangeState::Landed)).await.unwrap();
    assert_eq!(left.len(), 2);
}

/// AIRC: the first piece claim wins, with its files as path claims. Then a
/// stacked change takes over where the first stops, in its own workspace.
/// Returns the two change IDs, bottom first.
async fn claim_and_stack(
    mine: &ChannelMembership,
    theirs: &ChannelMembership,
    scratch: &Path,
) -> (String, String) {
    let author = mine.session();
    let root = scratch.join("work");
    let parent = author.create_workspace(WorkspaceSpec::new(&root)).await;
    let parent = parent.unwrap().change.change.id.to_string();
    let files = vec!["main.rs".to_owned()];
    let task = "§5: the piece";
    let claim = mine.claim_work(Some("task:1"), task, "vel/a", &parent, &files);
    assert!(claim.await.unwrap().granted());
    let refused = theirs.claim_piece(Some("task:1"), task, "vel/b").await;
    assert!(matches!(refused.unwrap(), PieceOutcome::Refused { .. }));
    let overlap = author.overlaps(None, &files).await.unwrap();
    assert!(overlap
        .overlaps
        .iter()
        .any(|o| o.change.to_string() == parent));

    std::fs::write(
        root.join("main.rs"),
        b"fn main() { one() }
",
    )
    .unwrap();
    author.snapshot(&root, Some("one".into())).await.unwrap();
    let child_root = scratch.join("child");
    let child = WorkspaceSpec::new(&child_root).on(parent.clone());
    let child = author.create_workspace(child).await.unwrap();
    std::fs::write(
        child_root.join("next.rs"),
        b"fn next() {}
",
    )
    .unwrap();
    author.snapshot(&child_root, None).await.unwrap();
    (parent, child.change.change.id.to_string())
}

/// Lands the stack bottom first, watching the author's events, and applies
/// the server's rebase of the upper change at the author's step boundary.
async fn land_stack(
    reviewer: &Session,
    author: &Session,
    parent: &str,
    child: &str,
    scratch: &Path,
) {
    let watcher = author.new_connection().await.unwrap();
    let mut events = watcher.events(None).await.unwrap();
    check_approve_and_land(reviewer, author, parent, scratch).await;
    let seen = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            if event.kind == EventKind::Landed && event.change.as_deref() == Some(parent) {
                return;
            }
        }
    });
    seen.await.expect("no landed event for the author");
    let applied = author.apply_rebase(&scratch.join("child")).await.unwrap();
    let record = author.client().change_show(child).await.unwrap();
    assert_eq!(applied, record.change.head);
    check_approve_and_land(reviewer, author, child, scratch).await;
    let read = ReadRequest {
        at: None,
        path: "next.rs".into(),
    };
    let landed = author.client().read_file(read).await.unwrap();
    assert!(
        matches!(landed, ReadAnswer::File { data, .. } if data == b"fn next() {}
")
    );
}

/// The build queue checks the merge result, PR duty approves, and the
/// author lands, again while another land left the receipt stale.
async fn check_approve_and_land(
    reviewer: &Session,
    author: &Session,
    change: &str,
    scratch: &Path,
) {
    for round in 0..20 {
        let build = scratch.join(format!("check-{change}-{round}"));
        let checkout = reviewer.check_out_merge_result(change, &build).await;
        let merge = checkout.unwrap().merge;
        assert!(build.join("main.rs").exists(), "nothing to build");
        let set = PRE_LAND_CHECK_SET;
        let receipt = reviewer.submit_receipt(change, merge.head, merge.base, set);
        receipt.await.unwrap();
        reviewer.approve(change, merge.head).await.unwrap();
        match author.land(change).await.unwrap().outcome {
            LandStatus::Landed { .. } => return,
            LandStatus::Refused { reason, .. } if reason == "receipt_stale" => {}
            other => panic!("{change} did not land: {other:?}"),
        }
    }
    panic!("{change} never landed")
}

async fn open(endpoint: &Endpoint, agent: &str) -> Session {
    let agent = AgentId::new(agent).unwrap();
    let builder = Session::builder().endpoint(endpoint.clone()).agent(agent);
    builder.start_daemon(false).open().await.unwrap()
}

/// A server whose base holds `main.rs`, made without git.
fn start_server(data: &Path) -> Endpoint {
    let endpoint = if cfg!(windows) {
        Endpoint::new(format!("rosen-velocity-flow-{}", std::process::id()))
    } else {
        Endpoint::new(data.join("s.sock").to_string_lossy())
    };
    let state = ServerState::new(ServerConfig {
        data_dir: data.to_path_buf(),
        endpoint: endpoint.clone(),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let blob = store.put_blob(b"fn main() {}\n").unwrap();
    let entry = TreeEntry::new(FileMode::Regular, "main.rs", blob).unwrap();
    let who = Signature::new("test", "test@test", 0);
    let commit = Commit {
        tree: store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap(),
        parents: vec![],
        author: who.clone(),
        committer: who,
        extra_headers: vec![],
        message: b"base\n".to_vec(),
    };
    let base = store.put_commit(&commit).unwrap();
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    ledger.init_base("main", &base).unwrap();
    let listener = Listener::bind(&endpoint).unwrap();
    tokio::spawn(rosen_server::listener::serve(listener, state));
    endpoint
}
