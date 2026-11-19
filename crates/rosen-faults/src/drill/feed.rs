//! A local Collapse state feed endpoint for the drill (issue #406). The
//! project's `collapse` root names it, with channel lines on, so the
//! server's feed runs through every kill and partition. Afterwards every
//! body it received is scanned for each machine credential, its hash, the
//! join keys, the machines' addresses and pinned certificate fingerprint,
//! and the server's data directory, none of which may ever leave it.

use super::verify::Check;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the endpoint received.
#[derive(Default)]
struct Received {
    bodies: Vec<Vec<u8>>,
    /// The highest `through` stored, which it answers with, as Collapse does.
    through: u64,
}

pub struct Intake {
    address: SocketAddr,
    received: Arc<Mutex<Received>>,
}

impl Intake {
    /// Listens on a free local port until the drill exits.
    pub async fn start() -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let received = Arc::<Mutex<Received>>::default();
        let serving = Arc::clone(&received);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(answer(socket, Arc::clone(&serving)));
            }
        });
        Ok(Self { address, received })
    }

    /// The root to name as `collapse`; plain http is allowed to loopback.
    pub fn root(&self) -> String {
        format!("http://{}/drill/feed", self.address)
    }

    /// Scans every body received for `secrets`, each with what it is.
    pub fn check(&self, secrets: &[(String, String)]) -> Check {
        let received = self.received.lock().expect("feed intake lock");
        let mut leaks = Vec::new();
        let mut items = 0;
        let mut machines = 0;
        for body in &received.bodies {
            let text = String::from_utf8_lossy(body);
            for (what, secret) in secrets {
                if !secret.is_empty() && text.contains(secret.as_str()) {
                    leaks.push(what.clone());
                }
            }
            let batch: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            let sections = batch.as_object().into_iter().flatten();
            items += sections
                .filter_map(|(_, v)| v.as_array())
                .map(Vec::len)
                .sum::<usize>();
            machines += batch["machines"].as_array().map_or(0, Vec::len);
        }
        leaks.sort();
        leaks.dedup();
        let bodies = received.bodies.len();
        let ok = leaks.is_empty() && items > 0;
        let detail = if !leaks.is_empty() {
            format!("the state feed sent {}", leaks.join(", "))
        } else if items == 0 {
            format!("the state feed sent no items in {bodies} requests")
        } else {
            format!(
                "{bodies} requests, {items} items, {machines} machine lists, \
                 scanned for {} secrets",
                secrets.len()
            )
        };
        Check {
            name: "state_feed_sends_nothing_private",
            ok,
            detail,
        }
    }
}

/// Every value the feed must never send: for each enrolled machine its
/// credential, the credential's hash, its upstream address and pinned
/// fingerprint; each join key and its hash; the server's listen address
/// and data directory. Read before the drill forgets the credentials.
pub fn secrets(
    machine_dirs: &[std::path::PathBuf],
    join_keys: &[String],
    listen: SocketAddr,
    data_dir: &Path,
) -> Vec<(String, String)> {
    let digest = rosen_server::machines::keys::digest;
    let mut secrets = vec![("the server address".to_owned(), listen.to_string())];
    let data = data_dir.display().to_string();
    // A path is sent inside JSON with its backslashes escaped.
    secrets.push(("the data directory".into(), data.replace('\\', "\\\\")));
    for key in join_keys {
        secrets.push(("a join key".into(), key.clone()));
        let inner = base64url(key).and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let inner: serde_json::Value = inner.unwrap_or_default();
        if let Some(secret) = inner["secret"].as_str() {
            secrets.push(("a join key hash".into(), digest(secret)));
            secrets.push(("a join key secret".into(), secret.to_owned()));
        }
    }
    for dir in machine_dirs {
        let Ok(bytes) = std::fs::read(dir.join("upstream.json")) else {
            continue;
        };
        let upstream: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        let field = |name: &str| upstream[name].as_str().unwrap_or_default().to_owned();
        secrets.push(("a machine address".into(), field("address")));
        secrets.push(("a certificate fingerprint".into(), field("fingerprint")));
        let reference = field("credential");
        if let Ok(secret) = rosen_client::enrollment::resolve_reference(&reference) {
            secrets.push(("a machine credential hash".into(), digest(&secret)));
            secrets.push(("a machine credential".into(), secret));
        }
    }
    secrets
}

/// Decodes unpadded base64url, the encoding of a join key (R14), so the
/// scan can also look for the secret inside it.
fn base64url(text: &str) -> Option<Vec<u8>> {
    let mut bits: u32 = 0;
    let mut held = 0;
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    for c in text.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        held += 6;
        if held >= 8 {
            held -= 8;
            bytes.push((bits >> held) as u8);
        }
    }
    Some(bytes)
}

/// Answers one request: stores its body and reports the highest sequence
/// held. A malformed request is dropped; the server retries.
async fn answer(mut socket: TcpStream, received: Arc<Mutex<Received>>) {
    let Some(body) = read_request(&mut socket).await else {
        return;
    };
    let through = {
        let mut received = received.lock().expect("feed intake lock");
        let batch: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let through = batch["through"].as_u64().unwrap_or(0);
        received.through = received.through.max(through);
        received.bodies.push(body);
        received.through
    };
    let answer = serde_json::json!({ "highest_sequence": through }).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{answer}",
        answer.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

/// The body of one HTTP request with a `Content-Length`.
async fn read_request(socket: &mut TcpStream) -> Option<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 65536];
    let (head_end, length) = loop {
        let read = socket.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (end + 4, length);
        }
    };
    while request.len() < head_end + length {
        let read = socket.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    Some(request.split_off(head_end))
}
