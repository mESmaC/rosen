#![cfg(windows)]
//! R14 risk: legacy inherited ACLs disclose the TLS key or let another user
//! redirect a daemon and collect its credential. Repair must preserve pins.
use rosen_server::network::{identity::identity_path, Identity, UpstreamConfig};
use std::{fs, path::Path, process::Command};

fn powershell(dir: &Path, script: &str) {
    let windows = std::env::var_os("WINDIR").expect("Windows sets WINDIR");
    let modules = Path::new(&windows).join("System32/WindowsPowerShell/v1.0/Modules");
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        // PowerShell 7's inherited module path can break Windows PowerShell's Get-Acl.
        .env("PSModulePath", modules)
        .env("ROSEN_ACL_TEST_DIR", dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn legacy_network_files_are_protected_on_load_without_changing_identity() {
    let dir = tempfile::tempdir().unwrap();
    assert!(UpstreamConfig::load(dir.path()).unwrap().is_none());
    let identity = Identity::load_or_create(dir.path()).unwrap();
    let path = identity_path(dir.path());
    let original = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::write(&path, &original).unwrap();
    let upstream = UpstreamConfig::path(dir.path());
    let config = format!(
        r#"{{"address":"127.0.0.1:7420","fingerprint":"{}","credential":"credman:test","extra":true}}"#,
        "00".repeat(32)
    );
    fs::write(&upstream, &config).unwrap();
    powershell(
        dir.path(),
        r#"
$ErrorActionPreference = 'Stop'
$dir = $env:ROSEN_ACL_TEST_DIR
$acl = Get-Acl -LiteralPath $dir
$everyone = [System.Security.Principal.SecurityIdentifier]::new('S-1-1-0')
$acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($everyone,'FullControl','ContainerInherit,ObjectInherit','None','Allow'))
Set-Acl -LiteralPath $dir -AclObject $acl
foreach ($name in @('network/identity.json','upstream.json')) {
    $path = Join-Path $dir $name
    icacls $path /grant '*S-1-1-0:F' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Could not grant legacy access' }
    if (!((Get-Acl -LiteralPath $path).Access | Where-Object IsInherited)) { throw 'Missing inherited access' }
}
"#,
    );
    for _ in 0..2 {
        let loaded = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(loaded.fingerprint(), identity.fingerprint());
        assert_eq!(loaded.private_key(), identity.private_key());
        let loaded = UpstreamConfig::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.address, "127.0.0.1:7420");
        assert_eq!(loaded.fingerprint, [0; 32]);
        assert_eq!(loaded.credential, "credman:test");
    }
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(fs::read_to_string(upstream).unwrap(), config);
    powershell(
        dir.path(),
        r#"
$ErrorActionPreference = 'Stop'
$user = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
foreach ($name in @('network/identity.json','upstream.json')) {
    $acl = Get-Acl -LiteralPath (Join-Path $env:ROSEN_ACL_TEST_DIR $name)
    if (!$acl.AreAccessRulesProtected -or @($acl.Access).Count -ne 2) { throw 'Not owner-only' }
    $sids = @($acl.Access | ForEach-Object {
        if ($_.IsInherited -or $_.AccessControlType -ne 'Allow' -or $_.FileSystemRights -ne 'FullControl') { throw 'Wrong rights' }
        $_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value
    })
    if ($user -notin $sids -or 'S-1-5-18' -notin $sids) { throw 'Wrong trustees' }
}
"#,
    );
}
