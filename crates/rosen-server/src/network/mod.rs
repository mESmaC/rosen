//! The network listener (SPEC R14, §1 "Transport"): the server accepts
//! daemons from other machines on a TCP port with TLS, alongside the local
//! transport, and serves them the same protocol.
//!
//! A network peer presents its own machine credential in its
//! hello (an anonymous peer can only enroll); a local client needs none, because the local transport is already
//! limited to the OS user.

pub mod identity;
pub mod tls;

use crate::connection::{self, Access};
use crate::ServerState;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

pub use identity::{Fingerprint, Identity};

/// The environment variable naming the address the server listens on, for
/// a daemon started by a client, which passes no `--listen` flag.
pub const LISTEN_ENV: &str = "ROSEN_LISTEN";

/// How a daemon reaches the network, beyond its local transport.
#[derive(Debug, Clone, Default)]
pub struct NetworkOptions {
    /// Run as the project's server and listen here, for example
    /// `0.0.0.0:7420`.
    pub listen: Option<String>,
    /// Run as a machine's tier-1 cache in front of this server (D8).
    pub upstream: Option<UpstreamConfig>,
}

/// Where a cache daemon's server is and how it proves itself to it.
#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    /// `host:port` of the server's network listener.
    pub address: String,
    /// The server certificate's fingerprint. The daemon talks to no other.
    pub fingerprint: Fingerprint,
    /// The credential or Credential Manager reference resolved for each hello.
    pub credential: String,
}

impl UpstreamConfig {
    /// The file a machine's daemon reads its server from, in its data
    /// directory, so a daemon a client starts with no flags still finds it.
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("upstream.json")
    }

    /// Reads `upstream.json`: `{"address", "fingerprint", "credential"}`.
    /// `None` when the file does not exist: the daemon is the server.
    pub fn load(data_dir: &Path) -> io::Result<Option<Self>> {
        let bytes = match std::fs::read(Self::path(data_dir)) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let invalid = |what: String| io::Error::new(io::ErrorKind::InvalidData, what);
        let json: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|err| invalid(format!("upstream.json: {err}")))?;
        let field = |name: &str| {
            json[name]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid(format!("upstream.json: missing {name:?}")))
        };
        Ok(Some(Self {
            address: field("address")?,
            fingerprint: identity::parse_fingerprint(&field("fingerprint")?).map_err(invalid)?,
            credential: field("credential")?,
        }))
    }
}

/// Accepts network peers until shutdown is requested. Each gets its own
/// task, and its TLS handshake runs there, so a slow peer never holds up
/// the accept loop.
pub async fn serve(listener: TcpListener, acceptor: TlsAcceptor, state: Arc<ServerState>) {
    let _advertisement = state.network_status().and_then(|(address, fingerprint)| {
        match rosen_client::discovery::advertise(address, &fingerprint) {
            Ok(advertisement) => Some(advertisement),
            Err(err) => {
                eprintln!("rosen-server: DNS-SD announcement failed: {err}");
                None
            }
        }
    });
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = state.shutdown_requested() => return,
        };
        match accepted {
            Ok((tcp, _peer)) => {
                let _ = tcp.set_nodelay(true);
                let acceptor = acceptor.clone();
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let handshake =
                        tokio::time::timeout(tls::HANDSHAKE_TIMEOUT, acceptor.accept(tcp));
                    match handshake.await {
                        Ok(Ok(stream)) => {
                            let stream: rosen_protocol::BoxStream = Box::new(stream);
                            connection::serve_with_access(stream, state, Access::Network).await;
                        }
                        Ok(Err(err)) => eprintln!("rosen-server: TLS handshake failed: {err}"),
                        Err(_) => eprintln!("rosen-server: TLS handshake timed out"),
                    }
                });
            }
            Err(err) => {
                // A failed accept (a peer that reset mid-connect) must not stop
                // the listener; back off briefly so a persistent error cannot spin.
                eprintln!("rosen-server: network accept failed: {err}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}
