param([string]$Root = (Split-Path -Parent $PSScriptRoot))

$ErrorActionPreference = 'Stop'
$testPath = Join-Path $Root 'crates/rosen-server/tests/review_396_mirror_results.rs'
if (Test-Path -LiteralPath $testPath) { throw 'Refusing to overwrite an existing test' }

$sourcePath = Join-Path $Root 'crates/rosen-server/tests/it/mirror_push.rs'
if (!(Test-Path -LiteralPath $sourcePath)) {
    $sourcePath = Join-Path $Root 'crates/rosen-server/tests/mirror_push.rs'
}
$source = Get-Content -LiteralPath $sourcePath -Raw
$anchor = '    let next = land(state, 5, None);'
if (!$source.Contains($anchor)) { throw 'Could not find the adoption scenario in mirror_push.rs' }
$check = @'
    runtime.block_on(async {
        use rosen_server::rpc::dispatch;

        let Outcome::Ok(Response::PostLand(work)) =
            dispatch(state, Request::PostLandBuildNext).await
        else {
            panic!("expected the regular build queue");
        };
        let mut job = work.job.expect("the ported land needs a build");
        assert_eq!(job.commit, ported);
        job.commit = answer.new_base;
        let failed = dispatch(
            state,
            Request::PostLandFailed { job: job.clone(), output: "test build failure".into() },
        )
        .await;
        let passed = dispatch(state, Request::PostLandPassed(job)).await;
        eprintln!("adoption={} failed={failed:?} passed={passed:?}", answer.new_base);
        assert!(
            matches!(failed, Outcome::Error(err) if err.code == ErrorCode::InvalidArgument)
                && matches!(passed, Outcome::Error(err) if err.code == ErrorCode::InvalidArgument),
            "R1 excludes mirror adoption from post-land checks and bisection"
        );
    });
'@

try {
    Set-Content -LiteralPath $testPath -Value ($source.Replace($anchor, "$check`n$anchor")) -Encoding utf8
    Push-Location $Root
    try {
        rosen run --class verify --full -- cargo test -p rosen-server --locked --test review_396_mirror_results lands_reach_the_mirror_as_fast_forwards_only -- --nocapture
        $result = $LASTEXITCODE
    } finally { Pop-Location }
} finally { Remove-Item -LiteralPath $testPath }
exit $result
