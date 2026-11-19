# Times the cargo runs agents and Rosen's land checks wait on (#399), for the
# `timings` job in ci.yml. Each run passes --timings, so cargo also writes an
# HTML report per run under $CARGO_TARGET_DIR/cargo-timings.
#
# The variants change build settings through environment variables only, so
# one dispatch compares them on the same runner without a config change landing:
#   default               the settings the repository builds with today
#   rust-lld              links with the LLD shipped with rustc instead of link.exe
#   rust-lld-line-tables  rust-lld, plus line-tables-only debug info in dev and test
#
# Since #423 the repository builds with both settings (.cargo/config.toml and
# Cargo.toml's dev profile), so the three variants now build alike. Their
# spread across one run shows the runner's noise.
param(
    [Parameter(Mandatory)]
    [ValidateSet('default', 'rust-lld', 'rust-lld-line-tables')]
    [string]$Variant
)

$ErrorActionPreference = 'Stop'

if ($Variant -ne 'default') {
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = 'rust-lld.exe'
}
if ($Variant -eq 'rust-lld-line-tables') {
    # The test profile inherits dev, so this covers clippy and test builds too.
    $env:CARGO_PROFILE_DEV_DEBUG = 'line-tables-only'
}

$rows = [System.Collections.Generic.List[object]]::new()

function Invoke-TimedCargo([string]$CheckSet, [string]$Step, [string[]]$CargoArgs) {
    Write-Host "::group::$Step (cargo $($CargoArgs -join ' '))"
    $watch = [Diagnostics.Stopwatch]::StartNew()
    & cargo @CargoArgs
    $exitCode = $LASTEXITCODE
    $watch.Stop()
    Write-Host '::endgroup::'
    $rows.Add([pscustomobject]@{
            CheckSet = $CheckSet
            Step     = $Step
            Seconds  = [math]::Round($watch.Elapsed.TotalSeconds, 1)
        })
    if ($exitCode -ne 0) { throw "$Step failed with exit code $exitCode" }
}

# A real edit, not only a new modification time, as an agent's change would be.
function Edit-CrateRoot([string]$Path) {
    Add-Content -Path $Path -Value "// build timings edit $([guid]::NewGuid())"
}

function Measure-CrateEdit([string]$Crate) {
    Edit-CrateRoot "crates/$Crate/src/lib.rs"
    Invoke-TimedCargo "edit $Crate" 'build' @('build', '--workspace', '--locked', '--timings')
    Invoke-TimedCargo "edit $Crate" 'clippy' @('clippy', '--workspace', '--all-targets', '--locked', '--timings', '--', '-D', 'warnings')
}

function Write-TimingsSummary {
    $lines = @(
        "### Build timings: $Variant",
        '',
        '| Check set | Step | Seconds |',
        '|---|---|---:|'
    )
    $lines += $rows | ForEach-Object { "| $($_.CheckSet) | $($_.Step) | $($_.Seconds) |" }
    # Totals in run order; Group-Object would sort them by name.
    $lines += $rows.CheckSet | Select-Object -Unique | ForEach-Object {
        $set = $_
        $total = ($rows | Where-Object CheckSet -EQ $set | Measure-Object Seconds -Sum).Sum
        "| $set | **total** | **$([math]::Round($total, 1))** |"
    }
    $lines | Out-File -Append -Encoding utf8 $env:GITHUB_STEP_SUMMARY
    $reports = Join-Path $env:CARGO_TARGET_DIR 'cargo-timings'
    New-Item -ItemType Directory -Force $reports | Out-Null
    $rows | ConvertTo-Csv -Delimiter "`t" -NoTypeInformation |
        Out-File -Encoding utf8 (Join-Path $reports "timings-$Variant.tsv")
}

try {
    # Cold: the pre-land set on an empty target directory, then the post-land set.
    Invoke-TimedCargo 'pre-land, cold' 'fmt' @('fmt', '--all', '--', '--check')
    Invoke-TimedCargo 'pre-land, cold' 'build' @('build', '--workspace', '--locked', '--timings')
    Invoke-TimedCargo 'pre-land, cold' 'clippy' @('clippy', '--workspace', '--all-targets', '--locked', '--timings', '--', '-D', 'warnings')
    Invoke-TimedCargo 'post-land' 'test build' @('test', '--workspace', '--locked', '--no-run', '--timings')
    Invoke-TimedCargo 'post-land' 'test run' @('test', '--workspace', '--locked')
    # One-crate edits on the warm target: the bottom of the graph, which every
    # crate depends on, and the server, where most changes land.
    Measure-CrateEdit 'rosen-core'
    Measure-CrateEdit 'rosen-server'
}
finally {
    Write-TimingsSummary
}
