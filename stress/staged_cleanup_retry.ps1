param(
    [Parameter(Mandatory)]
    [ValidateSet('MarkerRemoved', 'Utf8Bom', 'PlainIgnore', 'ReadOnlyPack')]
    [string] $Scenario,
    [string] $BinDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target/debug')
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$bin = (Resolve-Path -LiteralPath $BinDir).Path
$rosen = Join-Path $bin 'rosen.exe'
$server = Join-Path $bin 'rosen-server.exe'
$git = (Get-Command git.exe -All | Where-Object Source -NotMatch '\\velocity-agent-' | Select-Object -First 1).Source
if (!$git) { throw 'Git is required to seed the isolated fixture' }
$fixture = New-Item -ItemType Directory -Path (Join-Path ([IO.Path]::GetTempPath()) ('rosen-staged-' + [guid]::NewGuid()))
$data = Join-Path $fixture.FullName 'data'
$root = Join-Path $fixture.FullName 'workspaces'
$repo = Join-Path $fixture.FullName 'repo'
$workspace = Join-Path $root 'work'
$endpoint = 'rosen-staged-scout-' + [guid]::NewGuid()
$env:ROSEN_CHANGE = $null
$env:ROSEN_MANAGED_WORKSPACE = $null
$env:ROSEN_AGENT = 'staged-scout'

function Invoke-Rosen {
    & $rosen --endpoint $endpoint @args
}

$daemon = $null
try {
    New-Item -ItemType Directory -Path $repo, $root | Out-Null
    Push-Location -LiteralPath $repo
    try {
        & $git init --quiet
        & $git config core.autocrlf false
        $encoding = [Text.UTF8Encoding]::new($Scenario -eq 'Utf8Bom')
        [IO.File]::WriteAllText((Join-Path $repo '.gitignore'), "cache/`n", $encoding)
        [IO.File]::WriteAllText((Join-Path $repo 'tracked.txt'), "stored`n")
        [IO.File]::WriteAllText((Join-Path $repo 'rosen.toml'), "[warm_pool]`nsize = 0`n")
        & $git add --all
        & $git -c user.name=Scout -c user.email=scout@example.invalid commit --quiet -m base
    } finally {
        Pop-Location
    }
    $daemon = Start-Process -FilePath $server -WorkingDirectory $root -Environment @{Path = (Split-Path -Parent $git) + ';' + $env:Path} -ArgumentList @('--endpoint', $endpoint, '--data-dir', ('"{0}"' -f $data), '--log', 'server.log') -WindowStyle Hidden -PassThru
    Push-Location -LiteralPath $root
    try {
        $end = [DateTime]::UtcNow.AddSeconds(10)
        do {
            $PSNativeCommandUseErrorActionPreference = $false
            $status = Invoke-Rosen server status 2>$null
            $PSNativeCommandUseErrorActionPreference = $true
            if ($LASTEXITCODE -eq 0) { break }
            Start-Sleep -Milliseconds 100
        } while ([DateTime]::UtcNow -lt $end -and !$daemon.HasExited)
        if ($LASTEXITCODE -ne 0) { throw 'Fixture daemon did not start' }
        $status | Select-String '^commit:'
        Invoke-Rosen import $repo | Out-Null
        Invoke-Rosen change new --workspace $workspace --no-pool | Out-Null
        $registration = @(Get-ChildItem -LiteralPath (Join-Path $data 'workspaces') -Filter '*.json')
        if ($registration.Count -ne 1) { throw 'Expected one fixture registration' }
        $change = (Get-Content -LiteralPath $registration[0].FullName -Raw | ConvertFrom-Json).change
        $staged = Join-Path $root ('.rosen-delete-{0}-{1}' -f $change, $registration[0].BaseName)

        if ($Scenario -eq 'ReadOnlyPack') {
            $pack = Join-Path $workspace 'cache/clone/.git/objects/pack/pack-01.pack'
            New-Item -ItemType Directory -Path (Split-Path -Parent $pack) | Out-Null
            [IO.File]::WriteAllText($pack, 'pack bytes')
            [IO.File]::SetAttributes($pack, [IO.FileAttributes]::ReadOnly)
            Invoke-Rosen change abandon $change
            if ((Test-Path -LiteralPath $workspace) -or (Test-Path -LiteralPath $staged)) {
                throw 'A read-only pack kept the workspace or staged folder'
            }
            $daemon.Refresh()
            $before = $daemon.TotalProcessorTime
            Start-Sleep -Seconds 3
            $daemon.Refresh()
            Write-Output ('PASS read-only pack deleted, idle daemon used {0} ms CPU in 3 s' -f ($daemon.TotalProcessorTime - $before).TotalMilliseconds)
            return
        }
        if ($Scenario -eq 'MarkerRemoved') {
            # Simulate crashing after verified user files and the marker were
            # removed, immediately before removing the empty directories.
            New-Item -ItemType Directory -Path $staged, (Join-Path $staged '.rosen') | Out-Null
            Rename-Item -LiteralPath $workspace -NewName 'original-outside-staging'
        } else {
            New-Item -ItemType Directory -Path (Join-Path $workspace 'cache') | Out-Null
            [IO.File]::WriteAllText((Join-Path $workspace 'cache/output.bin'), 'ignored output')
            [IO.File]::Delete((Join-Path $workspace '.gitignore'))
            Move-Item -LiteralPath $workspace -Destination $staged
        }

        Invoke-Rosen change abandon $change
        if (Test-Path -LiteralPath $staged) {
            Invoke-Rosen workspace list
            Write-Output "FAIL $Scenario left the staged folder after cleanup"
            exit 1
        }
        Write-Output "PASS $Scenario removed the staged folder"
    } finally {
        Pop-Location
    }
} finally {
    if ($null -ne $daemon -and !$daemon.HasExited) {
        Stop-Process -Id $daemon.Id
        $daemon.WaitForExit()
    }
    $resolvedFixture = (Resolve-Path -LiteralPath $fixture.FullName).Path
    if ([IO.Path]::GetDirectoryName($resolvedFixture) -eq [IO.Path]::TrimEndingDirectorySeparator([IO.Path]::GetTempPath()) -and $fixture.Name.StartsWith('rosen-staged-')) {
        Remove-Item -LiteralPath $resolvedFixture -Recurse -Force
    }
}
