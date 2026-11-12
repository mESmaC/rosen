//! The Rosen daemon (SPEC.md D8, §3).
//!
//! One server runs per machine. It holds the authoritative object store and
//! SQLite metadata store, and does every merge, rebase, land and history
//! query on behalf of thin clients, over the protocol in `rosen-protocol`.

pub mod changes;
pub mod conflicts;
pub mod connection;
pub mod events;
pub mod listener;
pub mod rpc;

use events::EventLog;
use rosen_protocol::Endpoint;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// The server build name reported in handshakes and status.
pub const SERVER_NAME: &str = concat!("rosen-server ", env!("CARGO_PKG_VERSION"));

/// State shared by every connection.
pub struct ServerState {
    pub config: ServerConfig,
    pub started: Instant,
    pub events: EventLog,
    pub changes: changes::ChangeService,
    open_connections: AtomicU64,
    shutdown: watch::Sender<bool>,
}

impl ServerState {
    pub fn new(config: ServerConfig) -> Arc<Self> {
        Arc::new(Self {
            changes: changes::ChangeService::new(&config.data_dir),
            config,
            started: Instant::now(),
            events: EventLog::default(),
            open_connections: AtomicU64::new(0),
            shutdown: watch::Sender::new(false),
        })
    }

    pub fn open_connections(&self) -> u64 {
        self.open_connections.load(Ordering::SeqCst)
    }

    /// Asks the listener and every connection to stop.
    pub fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.shutdown.borrow()
    }

    /// Resolves once shutdown has been requested.
    pub async fn shutdown_requested(&self) {
        let mut receiver = self.shutdown.subscribe();
        // An Err means the sender is gone, which also means the server is ending.
        let _ = receiver.wait_for(|stopping| *stopping).await;
    }

    /// Counts a connection as open until the returned guard drops.
    pub(crate) fn track_connection(self: &Arc<Self>) -> ConnectionGuard {
        self.open_connections.fetch_add(1, Ordering::SeqCst);
        ConnectionGuard(Arc::clone(self))
    }

    /// Waits until every connection has closed, or `limit` passes, so replies
    /// already queued (such as the answer to `shutdown`) reach their clients.
    pub async fn drain_connections(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while self.open_connections() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

pub(crate) struct ConnectionGuard(Arc<ServerState>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.open_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Where a server keeps its state and where it listens.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Directory holding the object store and the metadata database.
    pub data_dir: PathBuf,
    /// The pipe or socket clients connect to.
    pub endpoint: Endpoint,
}

/// Binds the endpoint and serves until shutdown is requested. Fails with
/// [`std::io::ErrorKind::AddrInUse`] when another server owns the endpoint.
pub async fn serve(config: &ServerConfig) -> std::io::Result<()> {
    let listener = rosen_protocol::Listener::bind(&config.endpoint)?;
    let state = ServerState::new(config.clone());
    listener::serve(listener, Arc::clone(&state)).await;
    state.drain_connections(Duration::from_secs(2)).await;
    Ok(())
}
