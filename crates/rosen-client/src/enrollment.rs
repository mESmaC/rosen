//! Zero-touch enrollment and automatic credential renewal (SPEC R14).
use crate::{Client, Error};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rosen_protocol::{
    frame::{read_frame, write_frame},
    ClientFrame, CreateJoinKey, JoinKey, MachineCredential, Outcome, Request, Response,
    ServerFrame, SUPPORTED_VERSIONS,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
static RENEWAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const RENEW_BEFORE: u64 = 7 * 24 * 3600;
#[derive(Clone, Serialize, Deserialize)]
struct Enrollment {
    address: String,
    fingerprint: [u8; 32],
    credential: MachineCredential,
}
fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn target(dir: &Path) -> io::Result<String> {
    let absolute = std::fs::canonicalize(dir)?;
    Ok(format!(
        "rosen:{}",
        URL_SAFE_NO_PAD.encode(Sha256::digest(absolute.to_string_lossy().as_bytes()))
    ))
}
#[cfg(windows)]
fn store(target: &str, value: &Enrollment) -> io::Result<()> {
    rosen_platform::credentials::write(target, &serde_json::to_vec(value).map_err(invalid)?)
}
#[cfg(windows)]
fn load(target: &str) -> io::Result<Enrollment> {
    serde_json::from_slice(&rosen_platform::credentials::read(target)?).map_err(invalid)
}
#[cfg(not(windows))]
fn store(_: &str, _: &Enrollment) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}
#[cfg(not(windows))]
fn load(_: &str) -> io::Result<Enrollment> {
    Err(io::ErrorKind::Unsupported.into())
}
/// Pins TLS before sending the join secret. Only the machine credential is retained.
pub async fn join(dir: &Path, encoded: &str, name: &str) -> io::Result<MachineCredential> {
    let key: JoinKey = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).map_err(invalid)?)
        .map_err(invalid)?;
    std::fs::create_dir_all(dir)?;
    let target = target(dir)?;
    let response = exchange(
        &key.address,
        key.fingerprint,
        Request::MachineEnroll {
            key: key.secret,
            name: name.into(),
        },
    )
    .await?;
    save(
        dir,
        &target,
        Enrollment {
            address: key.address,
            fingerprint: key.fingerprint,
            credential: response.clone(),
        },
    )?;
    Ok(response)
}
pub async fn pending(
    dir: &Path,
    address: &str,
    pin: [u8; 32],
    name: &str,
    role: rosen_protocol::MachineRole,
) -> io::Result<MachineCredential> {
    std::fs::create_dir_all(dir)?;
    let response = exchange(
        address,
        pin,
        Request::MachinePending {
            name: name.into(),
            role,
        },
    )
    .await?;
    save(
        dir,
        &target(dir)?,
        Enrollment {
            address: address.into(),
            fingerprint: pin,
            credential: response.clone(),
        },
    )?;
    Ok(response)
}
fn save(dir: &Path, target: &str, value: Enrollment) -> io::Result<()> {
    store(target, &value)?;
    let json = serde_json::json!({"address":value.address,"fingerprint":hex(&value.fingerprint),"credential":format!("credman:{target}")});
    let path = dir.join("upstream.json");
    atomicwrites::AtomicFile::new(&path, atomicwrites::AllowOverwrite)
        .write(|f| io::Write::write_all(f, &serde_json::to_vec(&json).map_err(invalid)?))
        .map_err(invalid)?;
    Ok(())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
/// Resolves a reference without ever writing a credential to the config file.
pub fn resolve_reference(reference: &str) -> io::Result<String> {
    match reference.strip_prefix("credman:") {
        Some(target) => Ok(load(target)?.credential.secret),
        None => Ok(reference.to_owned()),
    }
}
/// Called hourly by the cache daemon, including while its upstream link is idle.
pub async fn renew(dir: &Path) -> io::Result<()> {
    let _guard = RENEWAL.lock().await;
    let target = target(dir)?;
    let mut value = load(&target)?;
    if value.credential.expires_at.saturating_sub(now()) > RENEW_BEFORE {
        return Ok(());
    }
    value.credential = exchange(
        &value.address,
        value.fingerprint,
        Request::MachineRotate {
            credential: value.credential.secret,
        },
    )
    .await?;
    store(&target, &value)
}
async fn exchange(address: &str, pin: [u8; 32], request: Request) -> io::Result<MachineCredential> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut stream = crate::pinned_tls::connect(address, pin).await?;
        write_frame(
            &mut stream,
            &ClientFrame::Hello {
                versions: SUPPORTED_VERSIONS.to_vec(),
                client: "rosen-enrollment".into(),
                credential: None,
            },
        )
        .await?;
        match read_frame::<_, ServerFrame>(&mut stream).await? {
            Some(ServerFrame::Welcome { .. }) => {}
            _ => return Err(invalid("enrollment handshake refused")),
        }
        write_frame(&mut stream, &ClientFrame::Request { id: 1, request }).await?;
        match read_frame::<_, ServerFrame>(&mut stream).await? {
            Some(ServerFrame::Response {
                outcome: Outcome::Ok(Response::MachineCredential(value)),
                ..
            }) => Ok(value),
            _ => Err(invalid("enrollment refused")),
        }
    })
    .await
    .map_err(invalid)?
}
impl Client {
    pub async fn create_join_key(&self, req: CreateJoinKey) -> Result<String, Error> {
        match self.request(Request::JoinKeyCreate(req)).await? {
            Response::JoinKeyCreated { key } => Ok(key),
            other => Err(crate::connection::unexpected(other)),
        }
    }
}

