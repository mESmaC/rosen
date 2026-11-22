//! SPEC.md R4 critical contract: deleting a workspace must not lose an edit.
//!
//! Risk: the snapshot window and a closed change can leave bytes only on
//! disk. Cleanup must store them first or name them and keep the workspace.
use rosen_client::protocol::{ChangeRecord, Listener, NewChange, Request};
use rosen_client::snapshot::{journal_snapshot, pending_journal, Dirty, Snapshotter};
use rosen_client::workspace::{deletion_permitted, pool::WarmPool, Options};
use rosen_client::{Client, Endpoint, Error};
use rosen_core::{AgentId, Commit, FileMode, Signature, Tree, TreeEntry};
use rosen_server::landing::Ledger;
use rosen_server::{ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::path::Path;
use std::time::Duration;

const DELETION_FIXTURES: &str = include_str!("../../../../proofs/fixtures/workspace-deletion.tsv");

#[tokio::test(flavor = "multi_thread")]
async fn deletion_stores_edits_or_keeps_and_names_them() {
    check_design_fixtures();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let store = LooseObjectStore::open(data.join("objects")).unwrap();
    let endpoint = start_server(&data, &store);
    let client = Client::connect(&endpoint).await.unwrap();
    ignored_build_output_does_not_block_cleanup(&client, dir.path()).await;
    abandon_stores_edits(&client, &store, &dir.path().join("workspace")).await;
    closed_edits_survive(&client, dir.path()).await;
    active_lease_keeps_workspace(&client, &dir.path().join("leased")).await;
    offline_journal_keeps_workspace(&client, &dir.path().join("journalled")).await;
    #[cfg(windows)]
    writable_handle_keeps_workspace(&client, &dir.path().join("writable")).await;
    #[cfg(windows)]
    readonly_git_packs_delete_cleanly(&client, dir.path()).await;
    #[cfg(windows)]
    pnpm_junctions_leave_targets_untouched(&client, dir.path()).await;
    #[cfg(any(windows, unix))]
    interrupted_cleanup_finishes(&client, &data, &dir.path().join("interrupted")).await;
    #[cfg(any(windows, unix))]
    orphaned_staged_folders_finish_in_one_pass(&client, &data, dir.path()).await;
    daemon_cleans_closed_workspace(&client, &endpoint, &data, &dir.path().join("automatic")).await;
}

async fn ignored_build_output_does_not_block_cleanup(client: &Client, root: &Path) {
    // R4 / #413: ignored caches are disposable, while genuinely unstored
    // scratch keeps the closed workspace with a bounded diagnostic.
    let path = root.join("build-output");
    let change = new_workspace(client, &path).await;
    for cache in ["proofs/.lake", "configured-cache"] {
        std::fs::create_dir_all(path.join(cache)).unwrap();
        std::fs::write(path.join(cache).join("output.bin"), b"build output").unwrap();
    }
    let snapshot = client.snapshot_workspace(&path, None).await.unwrap();
    assert_eq!(snapshot.tree, change.change.head, "build output was stored");

    let fixture = DELETION_FIXTURES
        .lines()
        .find_map(|line| line.strip_prefix("report\t"))
        .expect("missing R4 refusal fixture");
    let (scratch_count, expected_report) = fixture.split_once('\t').unwrap();
    let scratch_count: usize = scratch_count.parse().unwrap();
    let scratch = path.join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    for i in 0..scratch_count {
        std::fs::write(scratch.join(format!("edit-{i:02}")), b"unstored").unwrap();
    }
    close_without_snapshot(client, &change).await;
    let refusal = client
        .workspace_delete(&path)
        .await
        .unwrap_err()
        .to_string();
    assert!(refusal.ends_with(expected_report), "{refusal}");
    for i in 0..scratch_count {
        let edit = scratch.join(format!("edit-{i:02}"));
        assert_eq!(std::fs::read(&edit).unwrap(), b"unstored");
        std::fs::remove_file(edit).unwrap();
    }
    let reports = client.cleanup_workspaces().await.unwrap();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].error.is_none(), "{reports:?}");
    assert!(
        !path.exists(),
        "the ignored build output kept the workspace"
    );
}

#[cfg(windows)]
async fn pnpm_junctions_leave_targets_untouched(client: &Client, root: &Path) {
    // R4 / #388: following an ignored junction can erase another workspace
    // or a shared pnpm store; refusing every junction leaks all prepared trees.
    let workspace = root.join("pnpm-workspace");
    let change = new_workspace(client, &workspace).await;
    let target = root.join("pnpm-store");
    std::fs::create_dir_all(target.join(".rosen")).unwrap();
    std::fs::write(target.join(".rosen/workspace.json"), b"outside marker").unwrap();
    std::fs::write(target.join("package.json"), b"outside package").unwrap();
    let modules = workspace.join("ui/node_modules");
    std::fs::create_dir_all(modules.join("@scope")).unwrap();
    for package in ["dompurify", "@scope/package"] {
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "New-Item -ItemType Junction -Path $env:ROSEN_TEST_LINK -Target $env:ROSEN_TEST_TARGET -ErrorAction Stop | Out-Null",
            ])
            .env("ROSEN_TEST_LINK", modules.join(package))
            .env("ROSEN_TEST_TARGET", &target)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    close_without_snapshot(client, &change).await;
    client.workspace_delete(&workspace).await.unwrap();
    assert!(!workspace.exists(), "pnpm junctions kept the workspace");
    assert_eq!(
        std::fs::read(target.join("package.json")).unwrap(),
        b"outside package"
    );
    assert_eq!(
        std::fs::read(target.join(".rosen/workspace.json")).unwrap(),
        b"outside marker"
    );
}

async fn abandon_stores_edits(client: &Client, store: &dyn ObjectStore, workspace: &Path) {
    let change = new_workspace(client, workspace).await;
    std::fs::write(workspace.join("main.rs"), b"durable edit\n").unwrap();
    std::fs::create_dir(workspace.join("build")).unwrap();
    std::fs::write(workspace.join("build/output"), b"ignored").unwrap();
    let abandoned = client
        .change_abandon(&change.change.id.to_string())
        .await
        .unwrap();
    assert!(
        !workspace.exists(),
        "abandon did not clean up the workspace"
    );
    let tree = store.read_tree(&abandoned.change.head).unwrap();
    let edited = tree.get(b"main.rs").unwrap();
    assert_eq!(store.read_blob(&edited.id).unwrap().data, b"durable edit\n");
    assert!(tree.get(b"build").is_none(), "ignored output was uploaded");
}

async fn closed_edits_survive(client: &Client, root: &Path) {
    let kept = root.join("kept");
    let sibling = root.join("sibling");
    let closed = new_workspace(client, &kept).await;
    let _sibling_change = new_workspace(client, &sibling).await;
    std::fs::write(kept.join("main.rs"), b"unsaved edit\n").unwrap();
    close_without_snapshot(client, &closed).await;
    let refusal = client
        .workspace_delete(&kept)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("main.rs"),
        "refusal did not name the file: {refusal}"
    );
    assert_eq!(
        std::fs::read(kept.join("main.rs")).unwrap(),
        b"unsaved edit\n"
    );
    assert!(
        sibling.join("main.rs").exists(),
        "cleanup touched another workspace"
    );
}

async fn active_lease_keeps_workspace(client: &Client, workspace: &Path) {
    new_workspace(client, workspace).await;
    let lease = Snapshotter::open_offline(workspace).unwrap();
    client.workspace_delete(workspace).await.unwrap_err();
    assert!(
        workspace.join("main.rs").exists(),
        "active watcher lease was ignored"
    );
    drop(lease);
}

async fn offline_journal_keeps_workspace(client: &Client, workspace: &Path) {
    let change = new_workspace(client, workspace).await;
    let original = std::fs::read(workspace.join("main.rs")).unwrap();
    let state = Snapshotter::open_offline(workspace).unwrap();
    std::fs::write(workspace.join("main.rs"), b"intermediate offline edit\n").unwrap();
    let (state, result) = journal_snapshot(state, Dirty::Full).await;
    assert!(matches!(result, Err(Error::Disconnected)));
    drop(state);
    assert_eq!(pending_journal(workspace).unwrap(), 1);
    // A matching disk hash does not make the overwritten intermediate edit
    // durable: its unacknowledged journal entry must still keep the workspace.
    std::fs::write(workspace.join("main.rs"), &original).unwrap();
    close_without_snapshot(client, &change).await;
    let refusal = client
        .workspace_delete(workspace)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("main.rs"),
        "journal refusal did not name the file: {refusal}"
    );
    assert_eq!(pending_journal(workspace).unwrap(), 1);
    assert_eq!(std::fs::read(workspace.join("main.rs")).unwrap(), original);
}

#[cfg(windows)]
async fn writable_handle_keeps_workspace(client: &Client, workspace: &Path) {
    let change = new_workspace(client, workspace).await;
    let original = std::fs::read(workspace.join("main.rs")).unwrap();
    // Keep disk identical to the snapshot: only the live writable handle
    // should prevent deletion, even after the directory has been staged.
    let writer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(workspace.join("main.rs"))
        .unwrap();
    close_without_snapshot(client, &change).await;
    client.workspace_delete(workspace).await.unwrap_err();
    let parent = workspace.parent().unwrap();
    let stage_prefix = format!(".rosen-delete-{}-", change.change.id);
    let kept = std::iter::once(workspace.to_path_buf())
        .chain(
            std::fs::read_dir(parent)
                .unwrap()
                .map(|entry| entry.unwrap().path()),
        )
        .find(|path| {
            (path == workspace
                || path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&stage_prefix)))
                && path.join("main.rs").exists()
        })
        .expect("deletion removed a file with a live writable handle");
    assert_eq!(std::fs::read(kept.join("main.rs")).unwrap(), original);
    drop(writer);
}
#[cfg(windows)]
fn set_readonly(path: &Path) {
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(windows)]
async fn readonly_git_packs_delete_cleanly(client: &Client, root: &Path) {
    // R4 / #430: git pack files carry FILE_ATTRIBUTE_READONLY on Windows and
    // must not strand `.rosen-delete-*` folders or warm-pool/staging trees.
    let workspace = root.join("readonly-packs");
    let change = new_workspace(client, &workspace).await;
    let pack_dir = workspace.join("proofs/.lake/packages/aeneas/.git/objects/pack");
    std::fs::create_dir_all(&pack_dir).unwrap();
    for ext in ["idx", "pack", "rev"] {
        let pack = pack_dir.join(format!("pack-55c6.{ext}"));
        std::fs::write(&pack, b"pack bytes").unwrap();
        set_readonly(&pack);
    }
    set_readonly(&workspace.join("main.rs"));
    close_without_snapshot(client, &change).await;
    client.workspace_delete(&workspace).await.unwrap();
    assert!(!workspace.exists(), "read-only files kept the workspace");
    let leftovers: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!(".rosen-delete-{}-", change.change.id))
        })
        .collect();
    assert!(leftovers.is_empty(), "leftover staged dirs: {leftovers:?}");
}

#[cfg(any(windows, unix))]
async fn orphaned_staged_folders_finish_in_one_pass(client: &Client, data: &Path, root: &Path) {
    // R4 / #430: leftover `.rosen-delete-*` folders (even unregistered or
    // missing `.gitignore` from an earlier interrupted pass) finish in one
    // cleanup pass, while foreign-project folders stay untouched.
    let workspace = root.join("orphaned-staged");
    let change = new_workspace(client, &workspace).await;
    let cache = workspace.join("proofs/.lake/config/0");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("lakefile.olean"), b"olean").unwrap();
    #[cfg(windows)]
    {
        let pack = workspace.join("proofs/.lake/packages/Qq/.git/objects/pack/pack-00.idx");
        std::fs::create_dir_all(pack.parent().unwrap()).unwrap();
        std::fs::write(&pack, b"pack").unwrap();
        set_readonly(&pack);
    }
    close_without_snapshot(client, &change).await;
    // Remove registration and `.gitignore` to simulate a pre-upgrade orphan.
    for entry in std::fs::read_dir(data.join("workspaces"))
        .unwrap()
        .flatten()
    {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains(&change.change.id.to_string())
        {
            std::fs::remove_file(path).unwrap();
        }
    }
    std::fs::remove_file(workspace.join(".gitignore")).unwrap();
    let staged = root.join(format!(
        ".rosen-delete-{}-{}",
        change.change.id,
        "a".repeat(40)
    ));
    std::fs::rename(&workspace, &staged).unwrap();
    let foreign = root.join(format!(
        ".rosen-delete-{}-{}",
        rosen_core::ChangeId::from_bytes([9; 16]),
        "b".repeat(40)
    ));
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("keep.txt"), b"other project").unwrap();
    let reports = client.cleanup_workspaces().await.unwrap();
    let finished = reports
        .iter()
        .find(|report| report.path.file_name() == staged.file_name());
    assert!(finished.is_some_and(|r| r.error.is_none()), "{reports:?}");
    assert!(!staged.exists(), "orphaned staged dir survived cleanup");
    assert!(foreign.join("keep.txt").exists(), "foreign dir was removed");
    std::fs::remove_dir_all(&foreign).unwrap();
}

/// Issue #90: a crash after verified removal deleted some tracked files of
/// the staged directory must not leave a cleanup that refuses forever.
#[cfg(any(windows, unix))]
async fn interrupted_cleanup_finishes(client: &Client, data: &Path, workspace: &Path) {
    let change = new_workspace(client, workspace).await;
    close_without_snapshot(client, &change).await;
    // The registration file's name is the identity that names the staging
    // directory beside the workspace.
    let identity = std::fs::read_dir(data.join("workspaces"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && std::fs::read_to_string(path)
                    .unwrap()
                    .contains(&change.change.id.to_string())
        })
        .expect("workspace was not registered");
    let staged = workspace.with_file_name(format!(
        ".rosen-delete-{}-{}",
        change.change.id,
        identity.file_stem().unwrap().to_string_lossy()
    ));
    std::fs::rename(workspace, &staged).unwrap();
    std::fs::remove_file(staged.join("main.rs")).unwrap();
    // Earlier scenarios' kept workspaces are refused in the same pass.
    let reports = client.cleanup_workspaces().await.unwrap();
    let report = reports
        .iter()
        .find(|report| report.path.ends_with(workspace.file_name().unwrap()))
        .expect("interrupted cleanup was not attempted");
    assert!(
        report.error.is_none(),
        "interrupted cleanup was refused: {report:?}"
    );
    assert!(!staged.exists(), "staged workspace was kept");
}

async fn daemon_cleans_closed_workspace(
    client: &Client,
    endpoint: &Endpoint,
    data: &Path,
    path: &Path,
) {
    let baseline = client.status().await.unwrap().open_connections;
    let pool = WarmPool::new(data);
    pool.set_size(0).await;
    let worker = pool.start(endpoint.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.status().await.unwrap().open_connections <= baseline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon worker never connected");
    let change = new_workspace(client, path).await;
    close_without_snapshot(client, &change).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon did not clean the closed workspace after its event");
    worker.abort();
    let _ = worker.await;
}

async fn close_without_snapshot(client: &Client, change: &ChangeRecord) {
    client
        .request(Request::ChangeAbandon {
            change: change.change.id.to_string(),
        })
        .await
        .unwrap();
}

fn check_design_fixtures() {
    for line in DELETION_FIXTURES.lines() {
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with("dirty")
            || line.starts_with("report\t")
        {
            continue;
        }
        let columns: Vec<_> = line.split('\t').collect();
        assert_eq!(columns.len(), 5, "malformed deletion fixture: {line}");
        let values: Vec<_> = columns
            .iter()
            .map(|value| match *value {
                "1" | "true" => true,
                "0" | "false" => false,
                other => panic!("invalid fixture value: {other}"),
            })
            .collect();
        let mismatch = usize::from(values[2] && !values[3]);
        assert_eq!(
            deletion_permitted(usize::from(values[0]), usize::from(values[1]), mismatch),
            values[4],
            "{line}"
        );
    }
}

async fn new_workspace(client: &Client, path: &Path) -> ChangeRecord {
    let owner = AgentId::new("deletion-test").unwrap();
    let change = client
        .change_new(NewChange { owner, on: None })
        .await
        .unwrap();
    client
        .materialise_change(&change, path, Options::default())
        .await
        .unwrap();
    change
}

fn start_server(data: &Path, store: &dyn ObjectStore) -> Endpoint {
    let entries = vec![
        file(
            store,
            ".gitignore",
            b"build/\nnode_modules/\n/proofs/.lake/\n",
        ),
        file(store, "main.rs", b"fn main() {}\n"),
        file(store, "rosen.toml", b"ignore = [\"/configured-cache/\"]\n"),
    ];
    let tree = store.put_tree(&Tree::new(entries).unwrap()).unwrap();
    let endpoint = if cfg!(windows) {
        Endpoint::new(format!("rosen-deletion-test-{}", std::process::id()))
    } else {
        Endpoint::new(data.join("sock").join("s.sock").to_string_lossy())
    };
    let listener = Listener::bind(&endpoint).unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: data.to_path_buf(),
        endpoint: endpoint.clone(),
        warm_pool_dir: None,
    });
    let author = Signature::new("test", "test@test", 0);
    let commit = store
        .put_commit(&Commit {
            tree,
            parents: vec![],
            author: author.clone(),
            committer: author,
            extra_headers: vec![],
            message: b"base\n".to_vec(),
        })
        .unwrap();
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    ledger.init_base("main", &commit).unwrap();
    tokio::spawn(rosen_server::listener::serve(listener, state));
    endpoint
}

fn file(store: &dyn ObjectStore, name: &str, bytes: &[u8]) -> TreeEntry {
    let blob = store.put_blob(bytes).unwrap();
    TreeEntry::new(FileMode::Regular, name, blob).unwrap()
}
