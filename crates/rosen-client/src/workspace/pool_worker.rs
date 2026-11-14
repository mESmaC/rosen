//! Event-driven refill; one worker per daemon serializes refresh work and
//! runs at most one prepare at a time (SPEC R3).
use super::pool::{refresh, restore_tracked, seed_slot, Prep, Slot, WarmPool};
use super::MaterialiseError;
use rosen_store::LooseObjectStore;
use super::{prepare, template, Method, Options};
use crate::{Client, Error};
use rosen_core::ObjectId;
use rosen_protocol::{Endpoint, Event, EventKind, LogRequest};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio::time::Instant;

/// Bounds on the delay before a refused workspace cleanup is retried.
const CLEANUP_RETRY_MIN: Duration = Duration::from_secs(1);
const CLEANUP_RETRY_MAX: Duration = Duration::from_secs(60);

/// The base tip a refill pass brought the pool to.
struct Base {
    commit: ObjectId,
    tree: ObjectId,
    /// The prepare commands in the base's configuration.
    commands: Vec<String>,
}

pub(super) async fn run(pool: Arc<WarmPool>, endpoint: Endpoint) {
    // Only unused ready directories live here. Taken workspaces are moved
    // outside this root, so restart cleanup cannot remove an agent's files.
    loop {
        match reset_ready_root(&pool).await {
            Ok(()) => break,
            Err(error) => eprintln!("rosen warm pool startup: {error}"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let root = pool.state.lock().await.root.clone();
    let block_clones = tokio::task::spawn_blocking(move || template::block_clones(&root))
        .await
        .unwrap_or(false);
    pool.state.lock().await.block_clones = block_clones;
    loop {
        if let Err(error) = connected(&pool, &endpoint).await {
            eprintln!("rosen warm pool: {error}");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn connected(pool: &Arc<WarmPool>, endpoint: &Endpoint) -> Result<(), Error> {
    let client = Client::connect(endpoint).await?;
    // Subscribe before reading base: a concurrent land is either reflected
    // in the read or queued in the stream, never missed between the two.
    let events = client.subscribe(None).await?;
    // A reader task drains the stream however long a pass takes. Read only
    // between passes, more than a buffer of events during one pass stalled
    // the connection reader, and with it the reply the pass was awaiting
    // (#203). The JoinSet aborts the reader when this connection is left.
    let flags = Arc::new(EventFlags::default());
    let mut reader = tokio::task::JoinSet::new();
    reader.spawn(read_events(events, Arc::clone(&flags)));
    // Clean up at every (re)connect: events missed while disconnected are
    // covered by reading each registration's current change state.
    let mut schedule = CleanupSchedule::default();
    loop {
        schedule.take_queued(&flags)?;
        if schedule.due {
            schedule.ran(cleanup(&client).await?);
        }
        let base = match refill(pool, &client).await {
            Ok(base) => base,
            Err(error) if matches!(error, Error::Disconnected | Error::NotRunning(_)) => {
                return Err(error)
            }
            Err(error) => {
                eprintln!("rosen warm pool refill: {error}");
                // Retry transient store/index contention even if no later
                // land arrives to wake the worker. Successful reads are
                // event-driven.
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        // One prepare per pass, then clean up and refill again, so a land
        // that arrived during a long prepare is followed before the next one
        // starts.
        if let Some(base) = &base {
            if prepare_next(pool, base).await {
                continue;
            }
        }
        // A pool that is off waits like one that knows its base: a land
        // or a resize, which wakes it, is what can turn it on.
        let settled = base.is_some() || pool.state.lock().await.size == 0;
        wait_for_work(pool, &flags, &mut schedule, settled).await?;
    }
}

/// Waits until a refill or cleanup pass has something to do: a land or
/// revert moved the base, a change may have closed or reopened, a take or
/// resize woke the pool, or a cleanup retry fell due. Snapshots, claims and
/// reviews are most of the stream, and a refill pass costs requests to the
/// server, so they are skipped (#87). Until the pool is `settled`, as before
/// an import, which publishes no event, any event is a reason to look again.
async fn wait_for_work(
    pool: &WarmPool,
    flags: &EventFlags,
    schedule: &mut CleanupSchedule,
    settled: bool,
) -> Result<(), Error> {
    loop {
        let arrived = flags.take();
        schedule.due |= arrived.closes;
        if arrived.closes || arrived.moves || (arrived.any && !settled) {
            return Ok(());
        }
        if flags.ended.load(SeqCst) {
            return Err(Error::Disconnected);
        }
        let retry_at = schedule.retry_at;
        tokio::select! {
            _ = flags.arrived.notified() => {}
            _ = pool.wake.notified() => return Ok(()),
            _ = sleep_until(retry_at), if retry_at.is_some() => {
                schedule.due = true;
                return Ok(());
            }
        }
    }
}

/// What the event stream carried since the worker last looked. The worker
/// acts only on event kinds, so a few flags stand for any number of events
/// and the reader never has to wait for the worker.
#[derive(Default)]
struct EventFlags {
    closes: AtomicBool,
    moves: AtomicBool,
    any: AtomicBool,
    /// The stream ended: the connection dropped.
    ended: AtomicBool,
    arrived: Notify,
}

struct Arrived {
    closes: bool,
    moves: bool,
    any: bool,
}

impl EventFlags {
    fn note(&self, kind: EventKind) {
        self.closes.fetch_or(may_close_or_reopen(kind), SeqCst);
        self.moves.fetch_or(moves_base(kind), SeqCst);
        self.any.store(true, SeqCst);
        self.arrived.notify_one();
    }

    /// What arrived since the last take, clearing it.
    fn take(&self) -> Arrived {
        Arrived {
            closes: self.closes.swap(false, SeqCst),
            moves: self.moves.swap(false, SeqCst),
            any: self.any.swap(false, SeqCst),
        }
    }
}

async fn read_events(mut events: mpsc::Receiver<Event>, flags: Arc<EventFlags>) {
    while let Some(event) = events.recv().await {
        flags.note(event.kind);
    }
    flags.ended.store(true, SeqCst);
    flags.arrived.notify_one();
}

/// Events after which the base may hold a new tree. A revert is a land too.
fn moves_base(kind: EventKind) -> bool {
    matches!(kind, EventKind::Landed | EventKind::Reverted)
}

/// When the daemon next runs guarded cleanup of landed and abandoned
/// workspaces (R4).
struct CleanupSchedule {
    due: bool,
    retry_at: Option<Instant>,
    delay: Duration,
}

impl Default for CleanupSchedule {
    fn default() -> Self {
        Self {
            due: true,
            retry_at: None,
            delay: CLEANUP_RETRY_MIN,
        }
    }
}

impl CleanupSchedule {
    /// A refusal on a closed change usually needs a person, and each retry
    /// rehashes the kept workspace, so retries back off. A pass with no
    /// refusal resets the delay. The retry is a fixed deadline: frequent
    /// unrelated events must not keep postponing it.
    fn ran(&mut self, refused: bool) {
        self.due = false;
        if refused {
            self.retry_at = Some(Instant::now() + self.delay);
            self.delay = (self.delay * 2).min(CLEANUP_RETRY_MAX);
        } else {
            self.retry_at = None;
            self.delay = CLEANUP_RETRY_MIN;
        }
    }

    /// Notes events that arrived while the worker was busy, without
    /// waiting. A pass that ran a prepare skips the wait below, so without
    /// this a steady run of prepares postpones cleanup after every land (R4).
    fn take_queued(&mut self, flags: &EventFlags) -> Result<(), Error> {
        self.due |= flags.take().closes;
        if flags.ended.load(SeqCst) {
            return Err(Error::Disconnected);
        }
        if self.retry_at.is_some_and(|at| at <= Instant::now()) {
            self.due = true;
        }
        Ok(())
    }
}

/// Events that can move a change into or out of `landed` or `abandoned`.
/// Snapshots, claims and reviews cannot, so they do not rehash kept
/// workspaces on every write anywhere in the project.
fn may_close_or_reopen(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Landed | EventKind::Updated | EventKind::Reverted
    )
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn refill(pool: &Arc<WarmPool>, client: &Client) -> Result<Option<Base>, Error> {
    if !shrink(pool, client).await? {
        return Ok(None);
    }
    let Some(base) = read_base(client).await? else {
        return Ok(None);
    };
    let store = client.local_store().await?;
    let mut prose = ProseSince::new(client, base.commit);
    let block_clones = pool.state.lock().await.block_clones;
    if block_clones {
        sync_template(pool, client, &store, &base, &mut prose).await?;
    }
    refresh_ready(pool, &store, &base, &mut prose).await?;
    if block_clones {
        drop_unprepared(pool, &base).await?;
    }
    fill(pool, client, &store, &base).await?;
    Ok(Some(base))
}

/// Removes ready slots over the target size. Answers false when the pool is
/// off, after removing the template too, so a disabled pool holds no disk.
async fn shrink(pool: &WarmPool, client: &Client) -> Result<bool, Error> {
    // Read on every pass: a land can change the configured size.
    let configured = client.base_config().await?.warm_pool.size as usize;
    let mut state = pool.state.lock().await;
    state.size = state.requested.unwrap_or(configured);
    while state.ready.len() > state.size {
        let slot = state.ready.pop().expect("excess slot");
        std::fs::remove_dir_all(slot.path)?;
    }
    if state.size > 0 {
        return Ok(true);
    }
    drop(state);
    if let Some(slot) = pool.template.write().await.take() {
        std::fs::remove_dir_all(slot.path)?;
    }
    Ok(false)
}

async fn read_base(client: &Client) -> Result<Option<Base>, Error> {
    let log = client
        .log(LogRequest {
            limit: Some(1),
            ..LogRequest::default()
        })
        .await?;
    let Some(entry) = log.entries.first() else {
        return Ok(None);
    };
    client.ensure_local(entry.tree).await?;
    let config = client.project_config(Some(entry.commit)).await?;
    Ok(Some(Base {
        commit: entry.commit,
        tree: entry.tree,
        commands: config.config.prepare.commands,
    }))
}

/// Answers, once per earlier base commit, whether only prose landed from it
/// to the base tip: then a workspace prepared there is still prepared.
struct ProseSince<'a> {
    client: &'a Client,
    tip: ObjectId,
    known: HashMap<ObjectId, bool>,
}

impl<'a> ProseSince<'a> {
    fn new(client: &'a Client, tip: ObjectId) -> Self {
        Self {
            client,
            tip,
            known: HashMap::new(),
        }
    }

    async fn only_prose(&mut self, since: ObjectId) -> Result<bool, Error> {
        if since == self.tip {
            return Ok(true);
        }
        if let Some(&known) = self.known.get(&since) {
            return Ok(known);
        }
        let prose = self.client.landed_prose(since, self.tip).await?;
        self.known.insert(since, prose);
        Ok(prose)
    }

    /// The state a workspace prepared as `prep` at `commit` has at the tip.
    /// A failed prepare is retried now that the base has moved.
    async fn carried(&mut self, prep: Prep, commit: ObjectId) -> Result<Prep, Error> {
        if commit == self.tip {
            return Ok(prep);
        }
        if prep == Prep::Done && self.only_prose(commit).await? {
            return Ok(Prep::Done);
        }
        Ok(Prep::Needed)
    }
}

/// Brings the template to the base tip, making it on first use. The write
/// lock keeps takes from cloning it while its files change.
async fn sync_template(
    pool: &WarmPool,
    client: &Client,
    store: &rosen_store::LooseObjectStore,
    base: &Base,
    prose: &mut ProseSince<'_>,
) -> Result<(), Error> {
    let mut template = pool.template.write().await;
    if let Some(slot) = template.as_ref() {
        if slot.commit == base.commit {
            return Ok(());
        }
        let prep = prose.carried(slot.prep, slot.commit).await?;
        template::refresh_in_place(store, &mut template, base.tree).await?;
        if let Some(slot) = template.as_mut() {
            slot.commit = base.commit;
            slot.prep = prep;
            return Ok(());
        }
    }
    let path = template::path(&pool.state.lock().await.root);
    if path.exists() {
        // Left by a failed refresh or make; never handed to an agent.
        std::fs::remove_dir_all(&path)?;
    }
    let report = client
        .materialise_tree(base.tree, &path, Options::default())
        .await?;
    *template = Some(Slot {
        path,
        tree: base.tree,
        commit: base.commit,
        files: report.files,
        prep: Prep::Needed,
    });
    Ok(())
}

/// Brings each ready slot behind the base to the tip, one at a time. Only
/// the slot being refreshed is out of the pool: holding the pool lock for
/// the whole pass made every take wait for every slot (#191).
async fn refresh_ready(
    pool: &WarmPool,
    store: &rosen_store::LooseObjectStore,
    base: &Base,
    prose: &mut ProseSince<'_>,
) -> Result<(), Error> {
    loop {
        let mut slot = {
            let mut state = pool.state.lock().await;
            let behind = state.ready.iter().position(|slot| {
                (slot.commit != base.commit || slot.tree != base.tree) && slot.prep != Prep::Running
            });
            let Some(index) = behind else {
                return Ok(());
            };
            state.ready.remove(index)
        };
        let prep = match prose.carried(slot.prep, slot.commit).await {
            Ok(prep) => prep,
            Err(error) => {
                pool.state.lock().await.ready.push(slot);
                return Err(error);
            }
        };
        slot.prep = prep;
        slot.commit = base.commit;
        let (store, tree) = (store.clone(), base.tree);
        let updated = tokio::task::spawn_blocking(move || {
            if slot.tree == tree || refresh(&store, &mut slot, tree).is_ok() {
                Some(slot)
            } else {
                // A failed partial refresh is never offered to an agent.
                let _ = std::fs::remove_dir_all(&slot.path);
                None
            }
        })
        .await
        .map_err(|error| Error::Unexpected(error.to_string()))?;
        if let Some(slot) = updated {
            pool.state.lock().await.ready.push(slot);
        }
    }
}

/// Once the template is prepared at the tip, swaps every unprepared ready
/// slot for a fresh clone of it, which `fill` makes next. Until then the
/// unprepared slots stay, so a take still gets a workspace at once.
async fn drop_unprepared(pool: &WarmPool, base: &Base) -> Result<(), Error> {
    let template = pool.template.read().await;
    if !matches!(template::source(&template, base.tree), Some((_, true))) {
        return Ok(());
    }
    drop(template);
    let mut state = pool.state.lock().await;
    let (keep, stale): (Vec<Slot>, Vec<Slot>) = std::mem::take(&mut state.ready)
        .into_iter()
        .partition(|slot| slot.prep == Prep::Done);
    state.ready = keep;
    drop(state);
    for slot in stale {
        std::fs::remove_dir_all(&slot.path)?;
    }
    Ok(())
}

async fn fill(
    pool: &Arc<WarmPool>,
    client: &Client,
    store: &rosen_store::LooseObjectStore,
    base: &Base,
) -> Result<(), Error> {
    let mut state = pool.state.lock().await;
    // Bound one refill pass so a busy launch cannot starve landed events.
    let missing = state.size.saturating_sub(state.ready.len());
    for _ in 0..missing {
        if state.ready.len() >= state.size {
            break;
        }
        state.next += 1;
        let path = state
            .root
            .join(format!("{}-{}", std::process::id(), state.next));
        if path.exists() {
            continue;
        }
        // Existing ready slots remain available while a replacement copies.
        drop(state);
        let slot = make_slot(pool, client, base, path).await?;
        // Seed the snapshot tree cache now, so a take does not write it.
        let (seed_store, seed_path, tree) = (store.clone(), slot.path.clone(), slot.tree);
        tokio::task::spawn_blocking(move || seed_slot(&seed_store, &seed_path, tree))
            .await
            .map_err(|error| Error::Unexpected(error.to_string()))?;
        state = pool.state.lock().await;
        state.ready.push(slot);
    }
    Ok(())
}

/// Makes one ready slot: a block clone of the template when there is one,
/// otherwise a plain copy that is prepared on its own later.
async fn make_slot(
    pool: &WarmPool,
    client: &Client,
    base: &Base,
    path: std::path::PathBuf,
) -> Result<Slot, Error> {
    // Held through a clone so the worker's own template writes wait.
    let mut template = Some(pool.template.read().await);
    let source = template
        .as_deref()
        .and_then(|t| template::source(t, base.tree));
    if source.is_none() {
        template = None;
    }
    let prepared = source.as_ref().is_some_and(|(_, prepared)| *prepared);
    let options = Options {
        source: source.map(|(source, _)| source),
        ..Options::default()
    };
    let cloning = options.source.is_some();
    let report = client.materialise_tree(base.tree, &path, options).await?;
    drop(template);
    if cloning && report.method != Method::BlockClone {
        stop_template(pool, report.fallback.as_deref()).await?;
    }
    let prep = if prepared && report.method == Method::BlockClone {
        Prep::Done
    } else {
        Prep::Needed
    };
    Ok(Slot {
        path,
        tree: base.tree,
        commit: base.commit,
        files: report.files,
        prep,
    })
}

/// The volume refused a template clone after all: stop keeping a template
/// and prepare each ready workspace on its own.
async fn stop_template(pool: &WarmPool, why: Option<&str>) -> Result<(), Error> {
    eprintln!(
        "rosen warm pool: template clone fell back to a copy ({}); preparing each workspace instead",
        why.unwrap_or("unknown reason")
    );
    pool.state.lock().await.block_clones = false;
    if let Some(slot) = pool.template.write().await.take() {
        std::fs::remove_dir_all(slot.path)?;
    }
    Ok(())
}

/// Runs the prepare commands in one workspace that needs them: the template
/// when there is one, otherwise a ready slot. Answers whether it ran.
async fn prepare_next(pool: &WarmPool, base: &Base) -> bool {
    if pool.state.lock().await.block_clones {
        return prepare_template(pool, base).await;
    }
    let path = {
        let mut state = pool.state.lock().await;
        let next = state
            .ready
            .iter_mut()
            .find(|slot| slot.prep == Prep::Needed && slot.commit == base.commit);
        let Some(slot) = next else {
            return false;
        };
        // Running keeps takes away while the commands write into it.
        slot.prep = Prep::Running;
        slot.path.clone()
    };
    let prep = run_prepare(pool, &path, base).await;
    let mut state = pool.state.lock().await;
    let Some(index) = state.ready.iter().position(|slot| slot.path == path) else {
        return true;
    };
    match prep {
        Some(prep) => state.ready[index].prep = prep,
        None => {
            let slot = state.ready.remove(index);
            drop(state);
            drop_unsafe(&slot.path);
        }
    }
    true
}

/// Removes a workspace whose tracked directory a prepare replaced with a
/// link. `remove_dir_all` removes a link without following it, so nothing
/// outside the workspace is touched; a failure only leaks the directory.
fn drop_unsafe(path: &std::path::Path) {
    if let Err(error) = std::fs::remove_dir_all(path) {
        eprintln!("rosen warm pool: removing {}: {error}", path.display());
    }
}

async fn prepare_template(pool: &WarmPool, base: &Base) -> bool {
    let path = {
        let mut template = pool.template.write().await;
        match template.as_mut() {
            Some(slot) if slot.prep == Prep::Needed && slot.commit == base.commit => {
                slot.prep = Prep::Running;
                slot.path.clone()
            }
            _ => return false,
        }
    };
    let prep = run_prepare(pool, &path, base).await;
    let mut template = pool.template.write().await;
    match prep {
        Some(prep) => {
            if let Some(slot) = template.as_mut() {
                slot.prep = prep;
            }
        }
        // A later pass builds a new template.
        None => {
            if let Some(slot) = template.take() {
                drop_unsafe(&slot.path);
            }
        }
    }
    true
}

/// Runs the prepare commands in `path`, which holds `base.tree`, then puts
/// back any tracked file they changed, so the workspace an agent takes
/// matches the base exactly (#168). `None` means the workspace must be
/// dropped: a prepare left a link at a tracked directory, and a `Failed`
/// slot would still be handed out.
async fn run_prepare(pool: &WarmPool, path: &std::path::Path, base: &Base) -> Option<Prep> {
    if let Err(why) = prepare::run(path, &base.commands).await {
        eprintln!("rosen warm pool: {why}");
        return Some(Prep::Failed);
    }
    let objects = pool.data_dir.join("objects");
    let (root, tree) = (path.to_path_buf(), base.tree);
    let restored = tokio::task::spawn_blocking(move || {
        let store = LooseObjectStore::open(&objects).map_err(MaterialiseError::Store)?;
        let changed = restore_tracked(&store, &root, tree)?;
        // The tracked files now hold the tree again, and after the prepare
        // most are old enough for their stats to be trusted (#174).
        seed_slot(&store, &root, tree);
        Ok::<_, MaterialiseError>(changed)
    })
    .await;
    let error = match restored {
        Ok(Ok(changed)) => {
            if !changed.is_empty() {
                eprintln!(
                    "rosen warm pool: prepare changed tracked files in {}; restored {}",
                    path.display(),
                    changed.join(", ")
                );
            }
            return Some(Prep::Done);
        }
        Ok(Err(error)) => error,
        Err(join) => {
            eprintln!("rosen warm pool: restoring tracked files stopped: {join}");
            return Some(Prep::Failed);
        }
    };
    eprintln!("rosen warm pool: restoring tracked files in {}: {error}", path.display());
    match error {
        MaterialiseError::TrackedDirNotDir(_) => None,
        _ => Some(Prep::Failed),
    }
}

async fn reset_ready_root(pool: &WarmPool) -> Result<(), Error> {
    let root = std::path::absolute(&pool.state.lock().await.root)?;
    tokio::task::spawn_blocking(move || match std::fs::remove_dir_all(&root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    })
    .await
    .map_err(|error| Error::Unexpected(error.to_string()))??;
    Ok(())
}

async fn cleanup(client: &Client) -> Result<bool, Error> {
    let mut pending = false;
    for report in client.cleanup_workspaces().await? {
        if let Some(error) = report.error {
            pending = true;
            eprintln!("rosen workspace cleanup {}: {error}", report.path.display());
        }
    }
    Ok(pending)
}
