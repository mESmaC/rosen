param([string]$Root = (Split-Path -Parent $PSScriptRoot))
$ErrorActionPreference = 'Stop'
$testPath = Join-Path $Root 'crates/rosen-server/tests/review_367_reach.rs'
if (Test-Path -LiteralPath $testPath) { throw 'Refusing to overwrite an existing test' }
$source = Get-Content -LiteralPath (Join-Path $Root 'crates/rosen-server/tests/land_rpc.rs') -Raw
$helpers = $source.Substring(0, $source.IndexOf('// Risk (R8 step 1)'))
$cases = @'
// Risk: SPEC §2 and R8 forbid reusing a receipt across a changed input of its unit.
async fn seed_files(state: &Arc<ServerState>, root: ObjectId, files: &[(&str, &str)]) -> ObjectId {
    let seed = new_change(state).await;
    let mut head = root;
    for (path, text) in files {
        head = write(state, &seed, path, text.as_bytes()).await;
    }
    receipt(state, &seed, head, root).await;
    match land(state, &seed).await.outcome {
        LandStatus::Landed { commit, .. } => commit,
        other => panic!("seed did not land: {other:?}"),
    }
}

async fn check_recheck(files: &[(&str, &str)], code_path: &str, input_path: &str, input: &str) {
    let (_temp, state, root) = setup();
    let base = seed_files(&state, root, files).await;
    let pending = new_change(&state).await;
    let edit_input = new_change(&state).await;
    let own_head = write(&state, &pending, code_path, b"pub fn after() {}\n").await;
    receipt(&state, &pending, own_head, base).await;
    let input_head = write(&state, &edit_input, input_path, input.as_bytes()).await;
    receipt(&state, &edit_input, input_head, base).await;
    assert!(matches!(land(&state, &edit_input).await.outcome, LandStatus::Landed { .. }));
    let answer = land(&state, &pending).await;
    eprintln!("pending code={code_path}; intervening input={input_path}; outcome={:?}", answer.outcome);
    assert_eq!(refused(&answer), Some("needs_recheck"));
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_unit_name_collision_must_not_drop_manifest_consumer() {
    let files = [
        ("rosen.toml", "[build_units.a]\npaths=['crates/b/**']\n[build_units.b]\npaths=['crates/a/**']\n"),
        ("Cargo.toml", "[workspace]\nmembers=['crates/*']\n"),
        ("crates/a/Cargo.toml", "[package]\nname='a'\nversion='0.1.0'\n"),
        ("crates/b/Cargo.toml", "[package]\nname='b'\nversion='0.1.0'\n"),
        ("crates/a/src/lib.rs", "pub fn before() {}\n"),
        ("crates/b/src/lib.rs", "pub fn before() {}\n"),
    ];
    check_recheck(&files, "crates/b/src/lib.rs", "crates/b/Cargo.toml",
        "[package]\nname='b'\nversion='0.2.0'\n").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn implicit_workspace_member_must_not_drop_inherited_dependency_consumer() {
    let files = [
        ("Cargo.toml", "[workspace]\nmembers=['crates/a']\n[workspace.dependencies]\nexternal='1'\n"),
        ("crates/a/Cargo.toml", "[package]\nname='a'\nversion='0.1.0'\n[dependencies]\nhelper={path='../helper'}\n"),
        ("crates/helper/Cargo.toml", "[package]\nname='helper'\nversion='0.1.0'\n[dependencies]\nexternal={workspace=true}\n"),
        ("crates/a/src/lib.rs", "pub fn before() {}\n"),
        ("crates/helper/src/lib.rs", "pub fn helper() {}\n"),
    ];
    check_recheck(&files, "crates/a/src/lib.rs", "Cargo.toml",
        "[workspace]\nmembers=['crates/a']\n[workspace.dependencies]\nexternal='2'\n").await;
}
'@
try {
    Set-Content -LiteralPath $testPath -Value ($helpers + $cases) -Encoding utf8
    Push-Location $Root
    try {
        rosen run --class landing -- cargo test -p rosen-server --test review_367_reach -- --nocapture
        $result = $LASTEXITCODE
    } finally { Pop-Location }
} finally { Remove-Item -LiteralPath $testPath }
exit $result
