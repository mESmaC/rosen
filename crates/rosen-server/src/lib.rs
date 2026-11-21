//! The Rosen daemon (SPEC.md D8, §3).
//!
//! One daemon runs per machine. On the project's server machine it holds
//! the authoritative object store and SQLite metadata store, and does every
//! merge, rebase, land and history query on behalf of thin clients, over the
//! protocol in `rosen-protocol`, on the local transport and on a TLS network
//! listener (R14). On every other machine it runs as the tier-1 object cache
//! in [`cache`], forwarding every other request to the server.

pub mod bisect;
pub mod cache;
pub mod changes;
pub mod channel;
pub mod claims;
pub mod collapse_check;
#[cfg(test)]
mod collapse_fixtures;
pub mod collapse_health;
pub mod conflict_resolve;
pub mod conflicts;
pub mod connection;
pub mod events;
pub mod history;
pub mod import;
pub mod landing;
pub mod ledger_push;
pub mod listener;
pub mod load_limits;
pub mod machines;
pub mod metadata;
pub mod mirror;
pub mod network;
pub mod object_slices;
pub mod objects;
pub mod overlaps;
pub mod post_land;
pub mod project_config;
pub mod rebase;
pub mod receipts;
pub mod relay;
pub mod reviews;
pub mod rpc;
pub mod snapshots;
pub mod stacks;
pub mod state_push;
pub mod uploads;
pub mod workspaces;

use events::EventLog;
pub use landing::regenerate::enable_run_queue;
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

/// The git commit this server was built from, which `build.rs` reads from
/// `$ROSEN_BUILD_COMMIT` or the source checkout. `None` when neither told
/// it. [`SERVER_NAME`] is the same for every commit, so this is what tells a
/// client which build is running (#425).
pub const BUILD_COMMIT: Option<&str> = option_env!("ROSEN_SERVER_BUILD_COMMIT");

/// The path of this process's executable, for `status`.
pub(crate) fn executable() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    Some(path.display().to_string())
}

/// How long a stopping server waits for answers already queued to reach
/// their clients, once no request is still running.
const FLUSH_LIMIT: Duration = Duration::from_secs(2);

/// State shared by every connection.
pub struct ServerState {
    pub machine_agents: std::sync::Mutex<machines::manage::Presence>,
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
    pub(crate) transfers: load_limits::TransferLimits,
    pub(crate) overlap_edits: overlaps::edits::Cache,
    pub(crate) merge_results: history::merge::MergeCache,
    pub(crate) regenerations: landing::regenerate::RegenCache,
    /// Wakes the mirror task when the base appears without a land, as an
    /// import does (SPEC R1). A wake during a push is kept for the next wait.
    pub mirror_wake: tokio::sync::Notify,
    /// Remembers that an HTTPS mirror read succeeded via proactive basic auth
    /// so later reads in this server's lifetime use it directly (R1, #432).
    pub(crate) mirror_proactive_auth: mirror::proactive_auth::ProactiveAuth,
    /// Wakes the Collapse action relay when its setting changes or a state
    /// feed answer says actions wait (R14).
    pub relay_wake: Arc<relay::Wake>,
    /// Live health of the Collapse state feed, ledger push and relay (#434).
    pub collapse: collapse_health::Tracker,
    /// Live connected checkers by reported platform (R12, #440).
    pub checkers: Arc<post_land::checkers::CheckerRegistry>,
    /// Shared requests and mirror/recovery work; an operator rewrite takes
    /// exclusive access so no operation can retain an obsolete base view.
    pub maintenance: tokio::sync::RwLock<()>,
    open_connections: AtomicU64,
    /// Requests admitted and not yet answered, which shutdown waits for.
    requests_in_flight: AtomicU64,
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
            machine_agents: std::sync::Mutex::new(Default::default()),
            pool: config.warm_pool(),
            changes: changes::ChangeService::new(&config.data_dir),
            receipts: receipts::ReceiptService::new(config.clone()),
            landing: landing::LandingService::new(this.clone()),
            history: history::HistoryService::new(),
            workspace_io: load_limits::workspace_io(cap),
            project_config: project_config::ConfigService::new(config.clone()),
            transfers: load_limits::TransferLimits::default(),
            overlap_edits: overlaps::edits::Cache::default(),
            merge_results: history::merge::MergeCache::default(),
            regenerations: landing::regenerate::RegenCache::default(),
            events: EventLog::open(config.metadata_path()),
            config,
            started: Instant::now(),
            network: OnceLock::new(),
            mirror_wake: tokio::sync::Notify::new(),
            mirror_proactive_auth: mirror::proactive_auth::ProactiveAuth::default(),
            relay_wake: Arc::default(),
            collapse: collapse_health::Tracker::default(),
            checkers: Arc::default(),
            maintenance: tokio::sync::RwLock::new(()),
            open_connections: AtomicU64::new(0),
            requests_in_flight: AtomicU64::new(0),
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

    /// Revalidates a machine's credential, expiry and revocation status. It
    /// reads SQLite, so it runs on the blocking pool: on an async worker it
    /// stalled every connection that worker carries, once per network frame
    /// (#156).
    pub async fn credential_matches(self: &Arc<Self>, given: &str) -> bool {
        let (state, given) = (Arc::clone(self), given.to_owned());
        tokio::task::spawn_blocking(move || {
            machines::authenticate(&state.config.data_dir, &given).unwrap_or(false)
        })
        .await
        .unwrap_or(false)
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

    pub fn requests_in_flight(&self) -> u64 {
        self.requests_in_flight.load(Ordering::SeqCst)
    }

    /// Counts a request as in flight until the returned guard drops.
    pub(crate) fn track_request(self: &Arc<Self>) -> RequestGuard {
        self.requests_in_flight.fetch_add(1, Ordering::SeqCst);
        RequestGuard(Arc::clone(self))
    }

    /// Waits until every request the server is running has its answer
    /// queued. A stopping server reads no more frames, and a request it had
    /// read but not started is refused at once, so nothing new runs for long.
    /// There is no limit: a land cut off part-way leaves its client unable
    /// to tell whether the base moved (#425), and every request ends on its
    /// own.
    pub async fn drain_requests(&self) {
        let running = self.requests_in_flight();
        if running > 0 {
            eprintln!("rosen-server: stopping once {running} running request(s) are answered");
        }
        while self.requests_in_flight() > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// Counts one request as in flight until dropped: when it is answered, or
/// its task ends any other way.
pub(crate) struct RequestGuard(Arc<ServerState>);

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.0.requests_in_flight.fetch_sub(1, Ordering::SeqCst);
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
    /// A directory on the volume agents' workspaces live on, such as a Dev
    /// Drive, to keep the warm pool in (SPEC R3). `None` keeps it in the
    /// data directory, whose volume a take must share to be served.
    pub warm_pool_dir: Option<PathBuf>,
}

impl ServerConfig {
    /// The warm pool's ready root, which the daemon removes whole on start.
    /// Under a configured directory it is a subdirectory named for this
    /// data directory, so the operator's own files there are never removed
    /// and daemons of several projects can share one setting.
    pub fn warm_pool_root(&self) -> PathBuf {
        let Some(dir) = &self.warm_pool_dir else {
            return self.data_dir.join("warm-pool");
        };
        let data_dir =
            std::path::absolute(&self.data_dir).unwrap_or_else(|_| self.data_dir.clone());
        let owner =
            rosen_core::ObjectId::hash_object("warm-pool", data_dir.as_os_str().as_encoded_bytes());
        dir.join(format!("rosen-warm-pool-{}", &owner.to_hex()[..16]))
    }

    /// The daemon's warm pool, at [`ServerConfig::warm_pool_root`].
    pub fn warm_pool(&self) -> Arc<rosen_client::workspace::pool::WarmPool> {
        rosen_client::workspace::pool::WarmPool::with_root(&self.data_dir, &self.warm_pool_root())
    }

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

    /// Chunked blob uploads in progress (SPEC R2), one file each until the
    /// last slice arrives and the blob is stored.
    pub fn uploads_dir(&self) -> PathBuf {
        self.data_dir.join("uploads")
    }

    /// Checked, inflated copies of objects too large for one frame, which
    /// `GetObjectChunk` reads slices from (SPEC D8).
    pub fn slices_dir(&self) -> PathBuf {
        self.data_dir.join("slices")
    }

    /// Scratch directories for server-side generated-artifact regeneration
    /// at land (R5, R8, #428). Cleared on start and removed after each run.
    pub fn regen_scratch_dir(&self) -> PathBuf {
        self.data_dir.join("regen-tmp")
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
    // Only once the endpoint is this server's: a daemon that loses a start
    // race must not remove the copies the winner is serving.
    object_slices::clear(&config.slices_dir());
    landing::regenerate::clear_scratch(&config.regen_scratch_dir());
    // Created with the project's first server start (R14), so the
    // fingerprint daemons pin never changes.
    let identity = Identity::load_or_create(&config.data_dir)?;
    let network = match &options.listen {
        Some(address) => Some(bind_network(address, identity).await?),
        None => None,
    };
    let state = ServerState::new(config.clone());
    // Opening the log announces its stored tip, so status and the mirror
    // read a restarted server's real tip without touching SQLite themselves.
    // Events a stopped server staged with their cause but never stored are
    // delivered before any client can publish newer ones (R9, #118).
    let opening = Arc::clone(&state);
    let _ = tokio::task::spawn_blocking(move || {
        opening.events.last_seq();
        opening.events.deliver_staged();
    })
    .await;
    let pool_worker = state.pool.start(config.endpoint.clone());
    // Finishes post-land work (claim release, rebases) a crash cut short,
    // off the accept path so clients can connect meanwhile, then keeps
    // retrying post-land work that fails later.
    let recovering = Arc::clone(&state);
    let landing_recovery = tokio::spawn(async move {
        let first = Arc::clone(&recovering);
        let replayed = tokio::task::spawn_blocking(move || {
            let _running = first.maintenance.blocking_read();
            if let Err(err) = bisect::recover(&first) {
                eprintln!("rosen-server: confirmed bisection recovery failed: {err}");
            }
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
    let ledger_push = tokio::spawn(ledger_push::run(Arc::clone(&state)));
    let state_push = tokio::spawn(state_push::run(Arc::clone(&state)));
    let relay = tokio::spawn(relay::run(Arc::clone(&state)));
    let network_task = network.map(|(tcp, acceptor, identity, address)| {
        state.set_network(identity, address);
        tokio::spawn(network::serve(tcp, acceptor, Arc::clone(&state)))
    });
    listener::serve(listener, Arc::clone(&state)).await;
    if let Some(task) = network_task {
        let _ = task.await;
    }
    // Both listeners are closed, so nothing new arrives. What was already
    // running is answered before the process goes (#425).
    state.drain_requests().await;
    state.drain_connections(FLUSH_LIMIT).await;
    // A push cut short here is retried at the next start: the mirror's
    // pushed commit is only recorded once the remote accepts it.
    mirror.abort();
    // A send cut short here is resent at the next start: the cursor only
    // moves once the endpoint acknowledges a batch.
    ledger_push.abort();
    // Likewise for the state feed: its cursor moves only on acknowledgement,
    // and the next start resends everything live anyway.
    state_push.abort();
    // An action cut short here is settled at the next start: it was marked
    // running before it ran, and its outcome is posted from the store.
    relay.abort();
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
