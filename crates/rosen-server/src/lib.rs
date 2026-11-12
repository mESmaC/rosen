//! The Rosen daemon (SPEC.md D8, §3).
//!
//! One server runs per machine. It holds the authoritative object store and
//! SQLite metadata store, and does every merge, rebase, land and history
//! query on behalf of thin clients, over the protocol in `rosen-protocol`.

pub mod changes;
pub mod claims;
pub mod conflicts;
pub mod connection;
pub mod events;
pub mod import;
pub mod landing;
pub mod listener;
pub mod mirror;
pub mod receipts;
pub mod reviews;
pub mod rpc;
pub mod snapshots;

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
    pub receipts: receipts::ReceiptService,
    open_connections: AtomicU64,
    shutdown: watch::Sender<bool>,
}

impl ServerState {
    pub fn new(config: ServerConfig) -> Arc<Self> {
        let state = Arc::new(Self {
            changes: changes::ChangeService::new(&config.data_dir),
            receipts: receipts::ReceiptService::new(config.clone()),
            events: EventLog::open(config.metadata_path()),
            config,
            started: Instant::now(),
            open_connections: AtomicU64::new(0),
            shutdown: watch::Sender::new(false),
        });
        let base = import::base::LedgerBase::new(state.config.clone());
        state.changes.set_base_source(Arc::new(base));
        state
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

impl ServerConfig {
    /// The loose object store (SPEC D5): every imported, snapshotted and
    /// landed object.
    pub fn objects_dir(&self) -> PathBuf {
        self.data_dir.join("objects")
    }

    /// The SQLite metadata store (SPEC D13): the base ref, changes, claims,
    /// receipts, reviews and conflicts.
    pub fn metadata_path(&self) -> PathBuf {
        self.data_dir.join("metadata.sqlite")
    }

    /// Temporary clones of URL import sources. On the same volume as the
    /// store, and removed when each import ends.
    pub fn import_scratch_dir(&self) -> PathBuf {
        self.data_dir.join("import-tmp")
    }

    /// The bare repository the mirror pushes from (SPEC R1). It holds no
    /// objects of its own: git reads them from the object store.
    pub fn mirror_staging_dir(&self) -> PathBuf {
        self.data_dir.join("mirror.git")
    }
}

/// Binds the endpoint and serves until shutdown is requested. Fails with
/// [`std::io::ErrorKind::AddrInUse`] when another server owns the endpoint.
pub async fn serve(config: &ServerConfig) -> std::io::Result<()> {
    let listener = rosen_protocol::Listener::bind(&config.endpoint)?;
    let state = ServerState::new(config.clone());
    let mirror = tokio::spawn(mirror::run(Arc::clone(&state)));
    listener::serve(listener, Arc::clone(&state)).await;
    state.drain_connections(Duration::from_secs(2)).await;
    // A push cut short here is retried at the next start: the mirror's
    // pushed commit is only recorded once the remote accepts it.
    mirror.abort();
    Ok(())
}
