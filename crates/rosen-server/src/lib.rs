//! The Rosen daemon (SPEC §3): one per machine, holding the project and answering
//! the protocol in `rosen-protocol` over the local transport.

pub mod connection;
pub mod events;
pub mod listener;
pub mod rpc;

use events::EventLog;
use rosen_protocol::Endpoint;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// The server build name reported in handshakes and status.
pub const SERVER_NAME: &str = concat!("rosen-server ", env!("CARGO_PKG_VERSION"));

/// State shared by every connection.
pub struct ServerState {
    pub endpoint: Endpoint,
    pub started: Instant,
    pub events: EventLog,
    open_connections: AtomicU64,
    shutdown: watch::Sender<bool>,
}

impl ServerState {
    pub fn new(endpoint: Endpoint) -> Arc<Self> {
        Arc::new(Self {
            endpoint,
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

/// Binds the endpoint and serves until shutdown is requested.
pub async fn run(endpoint: Endpoint) -> std::io::Result<()> {
    let listener = rosen_protocol::Listener::bind(&endpoint)?;
    let state = ServerState::new(endpoint);
    listener::serve(listener, Arc::clone(&state)).await;
    state.drain_connections(Duration::from_secs(2)).await;
    Ok(())
}
