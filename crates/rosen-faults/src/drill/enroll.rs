//! Enrolls each simulated machine the way a real one joins (R14): the
//! server mints a join key, and the machine exchanges it over TLS for its
//! own credential. The key names the machine's partition proxy, so the
//! enrolled daemon reaches the server only through it.

use anyhow::{anyhow, Context};
use rosen_protocol::{CreateJoinKey, Endpoint, MachineRole};
use std::net::SocketAddr;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Enrolls `dir` as machine `name`. Afterwards a daemon started on `dir`
/// with no network flags finds its server in `upstream.json`.
pub async fn enroll(server: &Endpoint, proxy: SocketAddr, dir: &Path, name: &str) -> anyhow::Result<()> {
    let client = rosen_client::Client::connect(server).await?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let key = client
        .create_join_key(CreateJoinKey {
            address: proxy.to_string(),
            expires_at: now + 600,
            use_limit: 1,
            role: MachineRole::Agent,
        })
        .await
        .context("creating a join key")?;
    rosen_client::enrollment::join(dir, &key, name)
        .await
        .with_context(|| format!("enrolling {name}"))?;
    Ok(())
}

/// Removes the machine credential `enroll` stored in the user's credential
/// vault. Every drill enrolls new machines, so without this each run would
/// leave entries behind.
pub fn forget(dir: &Path) -> anyhow::Result<()> {
    let path = dir.join("upstream.json");
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let reference = json["credential"]
        .as_str()
        .ok_or_else(|| anyhow!("{}: no credential", path.display()))?;
    if let Some(target) = reference.strip_prefix("credman:") {
        delete(target)?;
    }
    Ok(())
}

#[cfg(windows)]
fn delete(target: &str) -> anyhow::Result<()> {
    Ok(rosen_platform::credentials::delete(target)?)
}

#[cfg(not(windows))]
fn delete(_: &str) -> anyhow::Result<()> {
    Ok(())
}
