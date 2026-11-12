//! The daemon on a machine that is not the project's server (SPEC D8, §3
//! "Topology"): the tier-1 object cache.
//!
//! It serves its own machine's clients over the local transport, answers
//! object reads from its own store, which every workspace on the machine
//! shares, and fills that store from the server on demand. Every other
//! request goes to the server over TLS, so refs, claims, events and every
//! other piece of mutable state are read and written only on the server.
//! The cache holds no metadata store.

mod fill;
mod session;
mod upstream;

use crate::network::UpstreamConfig;
use crate::{ServerConfig, SERVER_NAME};
use rosen_protocol::{Listener, ServerStatus, PROTOCOL_VERSION};
use rosen_store::LooseObjectStore;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// State shared by every local connection of a cache daemon.
pub struct CacheState {
    pub config: ServerConfig,
    pub upstream: UpstreamConfig,
    /// The machine's object store, at the path `status` reports, so clients
    /// materialise workspaces from it directly (R3).
    pub store: LooseObjectStore,
    started: Instant,
    open_connections: AtomicU64,
    shutdown: watch::Sender<bool>,
}

impl CacheState {
    fn status(&self) -> ServerStatus {
        ServerStatus {
            server: SERVER_NAME.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            endpoint: self.config.endpoint.to_string(),
            data_dir: self.config.data_dir.display().to_string(),
            uptime_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            open_connections: self.open_connections.load(Ordering::SeqCst),
            last_event: 0,
            listen: None,
            fingerprint: None,
            upstream: Some(self.upstream.address.clone()),
        }
    }

    fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    async fn shutdown_requested(&self) {
        let mut receiver = self.shutdown.subscribe();
        let _ = receiver.wait_for(|stopping| *stopping).await;
    }

    fn track_connection(self: &Arc<Self>) -> ConnectionGuard {
        self.open_connections.fetch_add(1, Ordering::SeqCst);
        ConnectionGuard(Arc::clone(self))
    }
}

struct ConnectionGuard(Arc<CacheState>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.open_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Binds the local endpoint and serves as a cache in front of `upstream`
/// until shutdown is requested. Fails with [`io::ErrorKind::AddrInUse`]
/// when another daemon owns the endpoint. The server need not be reachable
/// at start: each client connection reaches it when it first needs to.
pub async fn serve(config: &ServerConfig, upstream: UpstreamConfig) -> io::Result<()> {
    let mut listener = Listener::bind(&config.endpoint)?;
    let store = LooseObjectStore::open(config.objects_dir())
        .map_err(|err| io::Error::other(err.to_string()))?;
    let state = Arc::new(CacheState {
        config: config.clone(),
        upstream,
        store,
        started: Instant::now(),
        open_connections: AtomicU64::new(0),
        shutdown: watch::Sender::new(false),
    });
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = state.shutdown_requested() => break,
        };
        match accepted {
            Ok(stream) => {
                tokio::spawn(session::serve(stream, Arc::clone(&state)));
            }
            Err(err) => {
                eprintln!("rosen-server: accept failed: {err}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    // Let queued answers, such as the one to `shutdown`, reach their clients.
    let deadline = Instant::now() + Duration::from_secs(2);
    while state.open_connections.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}
