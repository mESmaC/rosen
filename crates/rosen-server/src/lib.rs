//! The Rosen daemon (SPEC.md D8, §3).
//!
//! One daemon runs per machine. On the project's server machine it holds
//! the authoritative object store and SQLite metadata store, and does every
//! merge, rebase, land and history query on behalf of thin clients, over the
//! protocol in `rosen-protocol`, on the local transport and on a TLS network
//! listener (R14). On every other machine it runs as the tier-1 object cache
//! in [`cache`], forwarding every other request to the server.

pub mod cache;
pub mod changes;
pub mod claims;
pub mod conflicts;
pub mod connection;
pub mod events;
pub mod history;
pub mod import;
pub mod landing;
pub mod listener;
pub mod load_limits;
pub mod machines;
pub mod metadata;
pub mod mirror;
pub mod network;
pub mod objects;
pub mod overlaps;
pub mod post_land;
pub mod project_config;
pub mod rebase;
pub mod receipts;
pub mod reviews;
pub mod rpc;
pub mod snapshots;
pub mod stacks;
pub mod workspaces;

use events::EventLog;
use network::{Identity, NetworkOptions};
use rosen_protocol::Endpoint;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// The server build name reported in handshakes and status.
pub const SERVER_NAME: &str = concat!("rosen-server ", env!("CARGO_PKG_VERSION"));

/// State shared by every connection.
pub struct ServerState {
    pub config: ServerConfig,
    pub pool: Arc<rosen_client::workspace::pool::WarmPool>,
    pub started: Instant,
    pub events: EventLog,
    pub changes: changes::ChangeService,
    pub receipts: receipts::ReceiptService,
    pub landing: landing::LandingService,
    /// The TLS identity and bound address, once the network listener is up.
    network: OnceLock<NetworkInfo>,
    pub history: history::HistoryService,
    /// Shared by workspace reads, lists, diffs and writes across all transports.
    pub workspace_io: Arc<tokio::sync::Semaphore>,
    pub project_config: project_config::ConfigService,
    open_connections: AtomicU64,
    shutdown: watch::Sender<bool>,
}

impl ServerState {
    pub fn new(config: ServerConfig) -> Arc<Self> {
        Self::with_workspace_io(
            config,
            std::num::NonZeroUsize::new(load_limits::DEFAULT_WORKSPACE_IO).unwrap(),
        )
    }

    /// Creates a server with an explicit positive workspace I/O cap.
    pub fn with_workspace_io(config: ServerConfig, cap: std::num::NonZeroUsize) -> Arc<Self> {
        // Cyclic because landing's adapters reach back into this state.
        let state = Arc::new_cyclic(|this| Self {
            pool: rosen_client::workspace::pool::WarmPool::new(&config.data_dir),
            changes: changes::ChangeService::new(&config.data_dir),
            receipts: receipts::ReceiptService::new(config.clone()),
            landing: landing::LandingService::new(this.clone()),
            history: history::HistoryService::new(),
            workspace_io: load_limits::workspace_io(cap),
            project_config: project_config::ConfigService::new(config.clone()),
            events: EventLog::open(config.metadata_path()),
            config,
            started: Instant::now(),
            network: OnceLock::new(),
            open_connections: AtomicU64::new(0),
            shutdown: watch::Sender::new(false),
        });
        let base = import::base::LedgerBase::new(state.config.clone());
        state.changes.set_base_source(Arc::new(base));
        state.events.set_overlap_owners(overlap_owners(&state));
        state
    }

    /// Records the network listener once it is bound.
    pub fn set_network(&self, identity: Identity, address: SocketAddr) {
        let _ = self.network.set(NetworkInfo { identity, address });
    }

    /// The network listener's address and certificate fingerprint (R14).
    pub fn network_status(&self) -> Option<(SocketAddr, String)> {
        let info = self.network.get()?;
        Some((
            info.address,
            network::identity::hex(&info.identity.fingerprint()),
        ))
    }

    /// Revalidates a machine's credential, expiry and revocation status.
    pub fn credential_matches(&self, given: &str) -> bool {
        machines::authenticate(&self.config.data_dir, given).unwrap_or(false)
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

struct NetworkInfo {
    identity: Identity,
    address: SocketAddr,
}

/// The owners of changes whose edits overlap an event's change, which R9
/// counts among its audience. Weak, because the log lives in the state.
fn overlap_owners(state: &Arc<ServerState>) -> Box<events::OverlapOwners> {
    let state = Arc::downgrade(state);
    Box::new(move |change| {
        let Some(state) = state.upgrade() else {
            return Ok(Vec::new());
        };
        let subject = overlaps::Subject::Change(change.to_string());
        match overlaps::find(&state, &subject) {
            Ok(found) => Ok(found.into_iter().map(|overlap| overlap.owner).collect()),
            // No such change has no edits, so nothing overlaps it.
            Err(overlaps::OverlapError::Change(changes::ChangeError::NotFound(_))) => {
                Ok(Vec::new())
            }
            Err(err) => Err(err.to_string()),
        }
    })
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
    serve_with(config, NetworkOptions::default()).await
}

/// [`serve`], also listening on the network or running as a cache in front
/// of another machine's server, as `options` say.
pub async fn serve_with(config: &ServerConfig, options: NetworkOptions) -> std::io::Result<()> {
    if let Some(upstream) = options.upstream {
        return cache::serve(config, upstream).await;
    }
    let listener = rosen_protocol::Listener::bind(&config.endpoint)?;
    // Created with the project's first server start (R14), so the
    // fingerprint daemons pin never changes.
    let identity = Identity::load_or_create(&config.data_dir)?;
    let network = match &options.listen {
        Some(address) => Some(bind_network(address, identity).await?),
        None => None,
    };
    let state = ServerState::new(config.clone());
    let pool_worker = state.pool.start(config.endpoint.clone());
    // Finishes post-land work (claim release, rebases) a crash cut short,
    // off the accept path so clients can connect meanwhile, then keeps
    // retrying post-land work that fails later.
    let recovering = Arc::clone(&state);
    let landing_recovery = tokio::spawn(async move {
        let first = Arc::clone(&recovering);
        let replayed = tokio::task::spawn_blocking(move || {
            if let Err(err) = post_land::recover(&first) {
                eprintln!("rosen-server: green notification recovery failed: {err}");
            }
            claims::recover(&first)
        })
        .await;
        if let Ok(Err(err)) = replayed {
            eprintln!("rosen-server: claim release recovery failed: {err}");
        }
        landing::service::keep_recovering(recovering).await;
    });
    let mirror = tokio::spawn(mirror::run(Arc::clone(&state)));
    let network_task = network.map(|(tcp, acceptor, identity, address)| {
        state.set_network(identity, address);
        tokio::spawn(network::serve(tcp, acceptor, Arc::clone(&state)))
    });
    listener::serve(listener, Arc::clone(&state)).await;
    if let Some(task) = network_task {
        let _ = task.await;
    }
    state.drain_connections(Duration::from_secs(2)).await;
    // A push cut short here is retried at the next start: the mirror's
    // pushed commit is only recorded once the remote accepts it.
    mirror.abort();
    landing_recovery.abort();
    pool_worker.abort();
    Ok(())
}

type BoundNetwork = (
    tokio::net::TcpListener,
    tokio_rustls::TlsAcceptor,
    Identity,
    SocketAddr,
);

async fn bind_network(address: &str, identity: Identity) -> std::io::Result<BoundNetwork> {
    let acceptor = network::tls::acceptor(&identity)?;
    let tcp = tokio::net::TcpListener::bind(address).await?;
    let bound = tcp.local_addr()?;
    Ok((tcp, acceptor, identity, bound))
}
