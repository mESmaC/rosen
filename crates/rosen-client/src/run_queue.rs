//! The machine's run queue (SPEC R3 "Run queue"): one line for every heavy
//! command on a machine, so builds take turns instead of all running at once.
//!
//! Many agents compiling side by side each assume they have every core, and
//! all finish late together. The queue gives each kind of run a class:
//! `landing` (the checks a land waits on), `build` (an agent's own builds and
//! tests), `verify` (the full tests after a land, and bisection) and
//! `prepare` (the warm pool's prepare commands), most important first.
//!
//! - Each class has a number of slots: as many runs of it go at once.
//!   Slots set class ceilings. All classes share CPU and memory admission.
//! - A run starts when a slot of its class is free for it, no more important
//!   run is going, and nothing sorts ahead of it in line: every more
//!   important waiter, and earlier waiters of its own class, which take the
//!   free slots first.
//! - Each turn needs room for its CPU and memory budgets after reserving
//!   memory for other applications and charging all live turns.
//!   Priority aging never bypasses this gate; running turns are not stopped.
//! - A less important run that is already going is never stopped.
//!   With capacity to spare a landing check starts
//!   beside a prepare; the caller runs `verify` and `prepare` work at low
//!   process priority, and the landing check gets the cores.
//! - Priority alone cannot hold a run forever: after its patience a waiter stops
//!   yielding to more important classes and needs only a free slot of its
//!   own class and sufficient memory.
//!   A stuck landing check then cannot hold every agent's build, and steady
//!   agent builds cannot keep the tests after a land from running when memory
//!   permits.
//! - A run keeps room for the first waiter that precedes it and has a free
//!   slot of its class: it starts only if it fits beside that waiter. A big
//!   turn blocked on memory then sees running turns drain instead of small
//!   ones filling every gap. An aged waiter precedes every later arrival.
//!
//! The queue is files in one folder per machine and user, with operating
//! system file locks, so it needs no daemon and survives any crash: a
//! waiter holds its ticket file locked and a runner one of its class's slot
//! lock files, and the system releases both when their process dies.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often a waiter looks at the line again.
pub const POLL: Duration = Duration::from_millis(50);
/// How long a waiter waits before it says what holds it, beside its ticket
/// and in `rosen run`'s wait line.
pub const STARVING: Duration = Duration::from_secs(5 * 60);
/// How often a starving waiter rewrites what holds it; memory readings change
/// on every poll.
const REPOST: Duration = Duration::from_secs(5);
/// Counts this process's tickets.
static TICKETS: AtomicU64 = AtomicU64::new(0);
/// Whether this process's pool worker waits in the machine's queue.
static PREPARES_QUEUE: AtomicBool = AtomicBool::new(false);

/// Makes this process's warm pool prepares wait in the machine's queue. The
/// daemon turns it on at start. A process that only embeds a pool worker,
/// such as a test, leaves it off and never waits on the machine's real runs.
pub fn queue_prepares() {
    PREPARES_QUEUE.store(true, Ordering::Relaxed);
}

pub(crate) fn prepares_queue() -> bool {
    PREPARES_QUEUE.load(Ordering::Relaxed)
}
/// How old an unlisted ticket is before it is taken for a dead process's.
const ABANDONED: Duration = Duration::from_secs(60);
/// Names another folder for the queue, for tests and for a run that must not
/// wait in the machine's real line.
pub const QUEUE_ENV: &str = "ROSEN_RUN_QUEUE";
/// Set on every process a turn runs, to the name of the turn's class. A run
/// started beneath one is part of that turn: `rosen run` there runs at once,
/// where waiting would wait on its own ancestor, which holds the class it
/// would wait for. A build script that queues its own steps can then be run
/// through `rosen run` whole.
pub const HELD_ENV: &str = "ROSEN_RUN_HELD";
/// Thread limits used by a turn's commands.
pub const THREAD_VARIABLES: [&str; 3] =
    ["CARGO_BUILD_JOBS", "RUST_TEST_THREADS", "LEAN_NUM_THREADS"];

/// The class whose turn an ancestor of this process holds, if any.
pub fn held() -> Option<RunClass> {
    std::env::var(HELD_ENV)
        .ok()
        .and_then(|class| RunClass::parse(&class))
}

/// What a run is for, most important first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RunClass {
    /// The pre-land checks of a change that is waiting to land.
    Landing,
    /// An agent's own build or test run.
    Build,
    /// The full tests after a land, and bisection steps.
    Verify,
    /// The warm pool's prepare commands.
    Prepare,
}

impl RunClass {
    pub const ALL: [RunClass; 4] = [Self::Landing, Self::Build, Self::Verify, Self::Prepare];

    fn rank(self) -> u8 {
        self as u8
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Landing => "landing",
            Self::Build => "build",
            Self::Verify => "verify",
            Self::Prepare => "prepare",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.name() == name)
    }

    /// How long a waiter of this class yields to more important classes.
    /// Landing has nothing more important to yield to.
    fn patience(self) -> Duration {
        match self {
            Self::Landing => Duration::ZERO,
            Self::Build => Duration::from_secs(3 * 60),
            Self::Verify => Duration::from_secs(2 * 60),
            Self::Prepare => Duration::from_secs(10 * 60),
        }
    }

    /// How many runs of this class go at once on this machine unless it sets
    /// its own count. The tests after a land and the pool's prepares are
    /// background work, kept to one so they never crowd out the rest.
    pub fn default_slots(self) -> u32 {
        match self {
            // Asked of the system once: a waiter looks every poll.
            Self::Landing | Self::Build => *MACHINE_SLOTS.get_or_init(|| {
                u32::try_from(machine_cores().div_ceil(CORES_PER_SLOT)).unwrap_or(u32::MAX)
            }),
            Self::Verify | Self::Prepare => 1,
        }
    }

    pub fn budget(self) -> RunBudget {
        self.budget_on(machine_cores())
    }

    fn budget_on(self, cores: usize) -> RunBudget {
        let cap = match self {
            Self::Prepare => 2,
            _ => CORES_PER_SLOT,
        };
        let jobs = (cores / 2).clamp(1, cap);
        // Leave a GiB for linkers, runners and other non-Cargo descendants.
        let memory = (jobs as u64 + 1) * GIB;
        RunBudget { jobs, memory }
    }
}

static MACHINE_SLOTS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// Maximum jobs per heavy turn, based on the R3 build-throughput ramp.
const CORES_PER_SLOT: usize = 5;
const GIB: u64 = 1 << 30;
const MEMORY_FLOOR: u64 = 4 * GIB;

fn machine_cores() -> usize {
    std::thread::available_parallelism().map_or(1, |cores| cores.get())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunBudget {
    pub jobs: usize,
    pub memory: u64,
}

impl RunBudget {
    /// A budget from whole jobs and GiB.
    pub fn new(jobs: usize, memory_gib: u64) -> Self {
        Self {
            jobs,
            memory: memory_gib.saturating_mul(GIB),
        }
    }

    /// This budget, held within `cap` and to at least one job and one GiB,
    /// so a caller can ask for less than its class's cap but never more.
    fn within(self, cap: Self) -> Self {
        Self {
            jobs: self.jobs.clamp(1, cap.jobs),
            memory: self.memory.clamp(GIB.min(cap.memory), cap.memory),
        }
    }

    fn charge(&mut self, budget: Self) {
        self.jobs = self.jobs.saturating_add(budget.jobs);
        self.memory = self.memory.saturating_add(budget.memory);
    }
}

/// A run in the queue, as [`RunQueue::line`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub class: RunClass,
    /// What the run is, as its owner described it.
    pub note: String,
    pub running: bool,
    /// What a waiter blocked for [`STARVING`] or longer waits for.
    pub waiting_for: Option<String>,
}

/// A current sample of the memory gate, in bytes. With no reading the
/// queue cannot establish room to start a new turn.
#[derive(Clone, Copy, Debug)]
pub struct MemoryHeadroom {
    pub available: Option<u64>,
    pub reserve: Option<u64>,
}

impl MemoryHeadroom {
    pub fn fits(self, bytes: u64) -> bool {
        self.available.is_some_and(|available| {
            available >= self.reserve.unwrap_or(MEMORY_FLOOR).saturating_add(bytes)
        })
    }

    /// Whether a single turn of `bytes` fits when no turn is running: uses the
    /// 4 GiB floor instead of a larger reserve, so the reserve throttles
    /// concurrency without deadlocking an idle machine.
    pub fn fits_idle(self, bytes: u64) -> bool {
        self.available
            .is_some_and(|available| available >= MEMORY_FLOOR.saturating_add(bytes))
    }

    /// Highest whole-GiB `reserve-gib` setting (`>= 4`) that would fit
    /// `budget_bytes` under the concurrency reserve, or `None` when it already
    /// fits or available memory is missing or below `budget_bytes + 4 GiB`.
    pub fn suggested_reserve_gib(self, budget_bytes: u64) -> Option<u64> {
        if self.fits(budget_bytes) {
            return None;
        }
        let max_gib = self.available?.checked_sub(budget_bytes)? / GIB;
        (max_gib >= MEMORY_FLOOR / GIB).then_some(max_gib)
    }

    fn reserve_for(self, idle_head: bool) -> u64 {
        if idle_head {
            MEMORY_FLOOR
        } else {
            self.reserve.unwrap_or(MEMORY_FLOOR)
        }
    }

    /// Whether `bytes` would fit once live turns charged `running` end and
    /// give back at most what they were charged. With nothing running, that
    /// head waiter only needs the floor, not the concurrency reserve.
    fn fits_once_drained(self, running: u64, bytes: u64) -> bool {
        let available = self
            .available
            .map(|available| available.saturating_add(running));
        Self { available, ..self }.fits_idle(bytes)
    }
}

/// Reported when the head waiter has waited at least [`STARVING`] while no
/// turn is running (SPEC R3 "Memory reserve").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdleStall {
    pub class: RunClass,
    pub waited: Duration,
    pub waiting_count: usize,
    pub budget: RunBudget,
    pub available: Option<u64>,
    pub reserve: Option<u64>,
    pub reason: Option<String>,
    /// Any `reserve-gib` setting (`>= 4`) that would unblock the idle head
    /// waiter. Always `None` because idle head admission uses the fixed 4 GiB
    /// floor rather than the concurrency reserve.
    pub suggested_reserve_gib: Option<u64>,
}

impl IdleStall {
    pub fn summary(&self) -> String {
        let detail = self
            .reason
            .clone()
            .unwrap_or_else(|| self.idle_memory_detail());
        let fix = self.suggested_reserve_gib.map_or_else(String::new, |gib| {
            format!("; unblock with `rosen run --slots reserve-gib={gib}`")
        });
        format!(
            "run queue stalled with no turn running ({} waiting); head {} waiter waited {}s ({detail}){fix}",
            self.waiting_count,
            self.class.name(),
            self.waited.as_secs(),
        )
    }

    fn idle_memory_detail(&self) -> String {
        let needed = MEMORY_FLOOR.saturating_add(self.budget.memory);
        match self.available {
            None => format!(
                "needs {} with {} idle floor, no reading available",
                gib(needed),
                gib(MEMORY_FLOOR),
            ),
            Some(available) if available < needed => format!(
                "needs {} with {} idle floor, {} available",
                gib(needed),
                gib(MEMORY_FLOOR),
                gib(available),
            ),
            Some(available) => format!(
                "no wait reason recorded; {} available covers {} idle threshold",
                gib(available),
                gib(needed),
            ),
        }
    }
}

/// One machine's queue folder.
#[derive(Clone, Debug)]
pub struct RunQueue {
    dir: PathBuf,
}

impl RunQueue {
    /// The queue every process of this user on this machine shares.
    pub fn machine() -> io::Result<Self> {
        let dir = match std::env::var_os(QUEUE_ENV) {
            Some(dir) => PathBuf::from(dir),
            None => machine_dir()?,
        };
        Ok(Self::at(dir))
    }

    /// A queue in `dir`, which is created when first waited in.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Takes a place in line for a run of `class`. `note` says what the run
    /// is, for those waiting behind it.
    pub fn wait(&self, class: RunClass, note: &str) -> io::Result<Waiter> {
        self.wait_for(class, None, note)
    }

    /// Like [`RunQueue::wait`], for a run that needs only `budget`, held
    /// within its class's cap. The waiter is admitted and charged on it.
    pub fn wait_with(&self, class: RunClass, budget: RunBudget, note: &str) -> io::Result<Waiter> {
        self.wait_for(class, Some(budget), note)
    }

    fn wait_for(
        &self,
        class: RunClass,
        requested: Option<RunBudget>,
        note: &str,
    ) -> io::Result<Waiter> {
        std::fs::create_dir_all(&self.dir)?;
        let arrived = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        // Two waits by one process in the same instant still get two names.
        let process = format!(
            "{}-{}",
            std::process::id(),
            TICKETS.fetch_add(1, Ordering::Relaxed)
        );
        // Made under another name and locked before it joins the line, so a
        // ticket in line that is found unlocked always means its owner died.
        let unlisted = self.dir.join(format!("{arrived}-{process}.new"));
        let ticket = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&unlisted)?;
        ticket.lock()?;
        // Sorts into queue order: class, then arrival, then process. A
        // requested budget follows, so waiters behind can keep room for it.
        let requested_suffix = requested.map_or_else(String::new, |budget| {
            format!("~{}x{}", budget.jobs, budget.memory)
        });
        let path = self.dir.join(format!(
            "{}-{arrived:030}-{process}{requested_suffix}.wait",
            class.rank()
        ));
        if let Err(error) = std::fs::rename(&unlisted, &path) {
            let _ = std::fs::remove_file(&unlisted);
            return Err(error);
        }
        Ok(Waiter {
            queue: self.clone(),
            class,
            requested,
            note: note.to_owned(),
            path,
            _ticket: ticket,
            since: Instant::now(),
            report: Mutex::default(),
        })
    }

    /// Waits in line on this thread and returns the turn. Blocking: call it
    /// off an async runtime's threads.
    pub fn turn(&self, class: RunClass, note: &str) -> io::Result<Turn> {
        Self::take_turn(self.wait(class, note)?)
    }

    /// Like [`RunQueue::turn`], for a run that needs only `budget`, held
    /// within its class's cap. Its budget file records that budget, so other
    /// waiters count it, and the turn's thread variables follow it.
    pub fn turn_with(&self, class: RunClass, budget: RunBudget, note: &str) -> io::Result<Turn> {
        Self::take_turn(self.wait_with(class, budget, note)?)
    }

    fn take_turn(waiter: Waiter) -> io::Result<Turn> {
        loop {
            if let Some(turn) = waiter.try_start()? {
                return Ok(turn);
            }
            std::thread::sleep(POLL);
        }
    }

    /// The runs going now, then those waiting, in queue order.
    pub fn line(&self) -> io::Result<Vec<Entry>> {
        let mut line = Vec::new();
        for (class, slot) in self.running()? {
            let note = std::fs::read_to_string(self.note_path(class, slot)).unwrap_or_default();
            line.push(Entry {
                class,
                note,
                running: true,
                waiting_for: None,
            });
        }
        for (name, class) in self.waiting()? {
            // A waiter's note is known only once it runs: its ticket is
            // locked, and a locked file can't be read on Windows.
            let waiting_for = std::fs::read_to_string(self.dir.join(reason_name(&name)))
                .ok()
                .filter(|reason| !reason.is_empty());
            line.push(Entry {
                class,
                note: String::new(),
                running: false,
                waiting_for,
            });
        }
        Ok(line)
    }

    /// Reports an idle stall when no turn is running and the head waiter has
    /// waited at least [`STARVING`].
    pub fn idle_stall(&self) -> io::Result<Option<IdleStall>> {
        self.idle_stall_on(machine_cores(), now_nanos(), self.memory_headroom())
    }

    fn idle_stall_on(
        &self,
        cores: usize,
        now: u128,
        memory: MemoryHeadroom,
    ) -> io::Result<Option<IdleStall>> {
        if !self.running()?.is_empty() {
            return Ok(None);
        }
        let waiting = self.waiting()?;
        let Some((ticket, name)) = waiting
            .iter()
            .filter_map(|(name, _)| Some((Ticket::parse(name)?, name)))
            .min_by_key(|(ticket, _)| ticket.precedence(now))
        else {
            return Ok(None);
        };
        let waited = nanos_to_duration(now.saturating_sub(ticket.arrived));
        let reason = std::fs::read_to_string(self.dir.join(reason_name(name)))
            .ok()
            .filter(|reason| !reason.is_empty());
        if waited < STARVING && reason.is_none() {
            return Ok(None);
        }
        let budget = admitted_budget(ticket.class, ticket.requested, cores);
        // Idle head admission uses MEMORY_FLOOR (4 GiB) rather than the
        // concurrency reserve, and `reserve-gib` cannot go below MEMORY_FLOOR,
        // so no reserve change can unblock an idle head waiter.
        Ok(Some(IdleStall {
            class: ticket.class,
            waited,
            waiting_count: waiting.len(),
            budget,
            available: memory.available,
            reserve: memory.reserve,
            reason,
            suggested_reserve_gib: None,
        }))
    }

    /// The live tickets' file names with their classes, in queue order. A
    /// ticket whose owner died is removed on the way.
    fn waiting(&self) -> io::Result<Vec<(String, RunClass)>> {
        let mut tickets = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(tickets),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let Some(class) = ticket_class(&name) else {
                self.remove_abandoned(&name);
                continue;
            };
            if owner_is_gone(&self.dir.join(&name)) {
                let _ = std::fs::remove_file(self.dir.join(&name));
                let _ = std::fs::remove_file(self.dir.join(reason_name(&name)));
                continue;
            }
            tickets.push((name, class));
        }
        tickets.sort();
        Ok(tickets)
    }

    /// Removes a ticket that never joined the line because its process died
    /// first. One being made right now is unlocked for an instant, so only
    /// an old one is taken for abandoned.
    fn remove_abandoned(&self, name: &str) {
        // A waiter writes its reason only while its ticket is in line, and
        // removes the ticket first when it leaves.
        if let Some(stem) = name.strip_suffix(REASON_SUFFIX) {
            if !self.dir.join(format!("{stem}.wait")).exists() {
                let _ = std::fs::remove_file(self.dir.join(name));
            }
            return;
        }
        let path = self.dir.join(name);
        let old = std::fs::metadata(&path)
            .and_then(|data| data.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > ABANDONED);
        if name.ends_with(".new") && old && owner_is_gone(&path) {
            let _ = std::fs::remove_file(path);
        }
    }

    // Include slots above a lowered class limit until their turns end.
    fn running(&self) -> io::Result<Vec<(RunClass, u32)>> {
        let mut slots = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(slots),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let Some(slot) = RunClass::ALL
                .into_iter()
                .find_map(|class| lock_slot(&name, class).map(|slot| (class, slot)))
            else {
                continue;
            };
            match File::open(self.dir.join(&name)) {
                Ok(lock) if !unlocked(&lock) => slots.push(slot),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        slots.sort_unstable();
        Ok(slots)
    }

    /// How many runs of `class` go at once: the machine's own count, set with
    /// [`RunQueue::set_slots`], or the class's default.
    pub fn slots(&self, class: RunClass) -> u32 {
        self.slot_overrides()
            .into_iter()
            .find(|(overridden, _)| *overridden == class)
            .map_or_else(|| class.default_slots(), |(_, slots)| slots)
    }

    /// The machine's available memory and reserve for other applications.
    pub fn memory_headroom(&self) -> MemoryHeadroom {
        let reserve = std::fs::read_to_string(self.dir.join(RESERVE_FILE))
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .filter(|bytes| *bytes > 0)
            .unwrap_or_else(|| {
                rosen_platform::memory::physical_memory().map_or(0, |bytes| bytes / 5)
            })
            .max(MEMORY_FLOOR);
        MemoryHeadroom {
            available: rosen_platform::memory::available_memory(),
            reserve: Some(reserve),
        }
    }

    /// Sets the machine's reserve in bytes, or restores the RAM-based default.
    /// All classes and projects see the change on their next admission poll.
    pub fn set_memory_reserve(&self, bytes: Option<u64>) -> io::Result<()> {
        if bytes.is_some_and(|bytes| bytes < MEMORY_FLOOR) {
            return Err(io::Error::other(
                "the memory reserve must be at least 4 GiB",
            ));
        }
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(RESERVE_FILE);
        let Some(bytes) = bytes else {
            return match std::fs::remove_file(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            };
        };
        // Never let a poll see a partial setting and silently use the default.
        let staged = self.dir.join(format!(
            "{RESERVE_FILE}.{}-{}.new",
            std::process::id(),
            TICKETS.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&staged, bytes.to_string())?;
        std::fs::rename(staged, path)
    }

    /// Sets the machine's own slot count for `class`, or with `None` goes
    /// back to the default. Runs waiting now see it on their next look; a
    /// lowered count stops no run already going.
    pub fn set_slots(&self, class: RunClass, slots: Option<u32>) -> io::Result<()> {
        if slots == Some(0) {
            return Err(io::Error::other("a class needs at least one slot"));
        }
        std::fs::create_dir_all(&self.dir)?;
        let mut overrides = self.slot_overrides();
        overrides.retain(|(overridden, _)| *overridden != class);
        overrides.extend(slots.map(|slots| (class, slots)));
        let text: String = overrides
            .iter()
            .map(|(class, slots)| {
                format!(
                    "{}={slots}
",
                    class.name()
                )
            })
            .collect();
        // Written aside and renamed over, so a waiter never reads half a file.
        let staged = self
            .dir
            .join(format!("{SLOTS_FILE}.{}.new", std::process::id()));
        std::fs::write(&staged, text)?;
        std::fs::rename(&staged, self.dir.join(SLOTS_FILE))
    }

    /// The machine's own slot counts, one line `class=count` each. A line
    /// that doesn't parse is ignored, so a bad edit leaves the defaults.
    fn slot_overrides(&self) -> Vec<(RunClass, u32)> {
        let text = std::fs::read_to_string(self.dir.join(SLOTS_FILE)).unwrap_or_default();
        text.lines()
            .filter_map(|line| {
                let (class, slots) = line.split_once('=')?;
                let class = RunClass::parse(class.trim())?;
                let slots = slots
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|slots| *slots > 0)?;
                Some((class, slots))
            })
            .collect()
    }

    /// Slot 0 keeps the name the queue used when each class had one slot, so
    /// an older `rosen` on the machine still shares it.
    fn lock_path(&self, class: RunClass, slot: u32) -> PathBuf {
        self.dir.join(format!("{}.lock", slot_stem(class, slot)))
    }

    fn admission(&self) -> io::Result<Option<File>> {
        try_lock(&self.dir.join("admission.lock"))
    }

    fn budget_path(&self, class: RunClass, slot: u32) -> PathBuf {
        self.dir.join(format!("{}.budget", slot_stem(class, slot)))
    }

    pub fn running_budget(&self) -> io::Result<RunBudget> {
        Ok(self.budget_for(&self.running()?))
    }

    fn budget_for(&self, slots: &[(RunClass, u32)]) -> RunBudget {
        let mut total = RunBudget::default();
        for &(class, slot) in slots {
            total.charge(self.slot_budget(class, slot));
        }
        total
    }

    /// The budget a live turn recorded, or its class's cap.
    fn slot_budget(&self, class: RunClass, slot: u32) -> RunBudget {
        std::fs::read_to_string(self.budget_path(class, slot))
            .ok()
            .and_then(|text| {
                let (jobs, memory) = text.split_once(' ')?;
                Some(RunBudget {
                    jobs: jobs.parse().ok()?,
                    memory: memory.parse().ok()?,
                })
            })
            .unwrap_or_else(|| class.budget())
    }

    /// Whether the waiter `name` of `class` has a slot of its class left
    /// after the earlier waiters of its class take theirs.
    fn has_free_slot(
        &self,
        name: &str,
        class: RunClass,
        waiting: &[(String, RunClass)],
        running: &[(RunClass, u32)],
    ) -> bool {
        let earlier_of_class = waiting
            .iter()
            .take_while(|(other, _)| other.as_str() < name)
            .filter(|(_, other)| *other == class)
            .count();
        let in_class = running.iter().filter(|(other, _)| *other == class).count();
        let free = usize::try_from(self.slots(class))
            .unwrap_or(usize::MAX)
            .saturating_sub(in_class);
        earlier_of_class < free
    }

    /// Beside the lock, not in it: a locked file's bytes can't be read by
    /// another process on Windows.
    fn note_path(&self, class: RunClass, slot: u32) -> PathBuf {
        self.dir.join(format!("{}.note", slot_stem(class, slot)))
    }
}

/// The file in the queue folder holding the machine's own slot counts.
const SLOTS_FILE: &str = "slots";
/// The machine-wide override, in bytes; kept separate from class slot counts.
const RESERVE_FILE: &str = "memory-reserve";
/// Beside a starving waiter's ticket: what it waits for. The ticket itself is
/// locked, and a locked file can't be read on Windows.
const REASON_SUFFIX: &str = ".why";

fn reason_name(ticket: &str) -> String {
    let stem = ticket.strip_suffix(".wait").unwrap_or(ticket);
    format!("{stem}{REASON_SUFFIX}")
}

fn slot_stem(class: RunClass, slot: u32) -> String {
    match slot {
        0 => format!("run-{}", class.name()),
        slot => format!("run-{}-{slot}", class.name()),
    }
}

/// The slot of `class` a lock file's name gives; `None` for any other file.
fn lock_slot(name: &str, class: RunClass) -> Option<u32> {
    let rest = name
        .strip_suffix(".lock")?
        .strip_prefix("run-")?
        .strip_prefix(class.name())?;
    match rest {
        "" => Some(0),
        rest => rest.strip_prefix('-')?.parse().ok(),
    }
}

fn try_lock(path: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// A place in line. Dropping it leaves the line.
#[derive(Debug)]
pub struct Waiter {
    queue: RunQueue,
    class: RunClass,
    /// The budget the caller asked for, if less than the class's cap.
    requested: Option<RunBudget>,
    /// Shown to those waiting behind this run once it has its turn.
    note: String,
    path: PathBuf,
    /// Held locked for as long as this waiter lives.
    _ticket: File,
    since: Instant,
    /// What held this waiter at its last look, and what it last wrote
    /// beside its ticket.
    report: Mutex<WaitReport>,
}

#[derive(Debug, Default)]
struct WaitReport {
    reason: Option<String>,
    written: Option<(String, Instant)>,
}

impl Waiter {
    /// The turn, when it is this waiter's; `None` while it has to wait.
    /// Never blocks.
    pub fn try_start(&self) -> io::Result<Option<Turn>> {
        self.try_start_on(machine_cores(), || self.queue.memory_headroom())
    }

    fn try_start_on(
        &self,
        cores: usize,
        read_memory: impl FnOnce() -> MemoryHeadroom,
    ) -> io::Result<Option<Turn>> {
        // A class cannot spend free memory another class is about to reserve.
        let Some(_admission) = self.queue.admission()? else {
            return Ok(None);
        };
        let yielding = self.since.elapsed() < self.class.patience();
        let slots = self.queue.slots(self.class);
        let running = self.queue.running()?;
        let waiting = self.queue.waiting()?;
        if let Some(reason) = self.not_next(&waiting, yielding, slots, &running) {
            self.report(reason);
            return Ok(None);
        }
        let budget = self.budget_on(cores);
        if let Err(reason) = self.room(budget, cores, &waiting, &running, read_memory()) {
            self.report(reason);
            return Ok(None);
        }
        for slot in 0..slots {
            let Some(lock) = try_lock(&self.queue.lock_path(self.class, slot))? else {
                continue;
            };
            let path = self.queue.budget_path(self.class, slot);
            std::fs::write(&path, format!("{} {}", budget.jobs, budget.memory))?;
            let _ = std::fs::write(self.queue.note_path(self.class, slot), &self.note);
            return Ok(Some(Turn {
                _lock: lock,
                budget,
                path,
            }));
        }
        self.report(format!("waiting for a free {} slot", self.class.name()));
        Ok(None)
    }

    /// `Ok` when this waiter's budget fits the cores and memory left after
    /// every live turn and the waiter it keeps room for; otherwise what it
    /// waits for.
    fn room(
        &self,
        budget: RunBudget,
        cores: usize,
        waiting: &[(String, RunClass)],
        running: &[(RunClass, u32)],
        memory: MemoryHeadroom,
    ) -> Result<(), String> {
        let holders: Vec<_> = running
            .iter()
            .map(|&(class, slot)| (class, self.queue.slot_budget(class, slot)))
            .collect();
        let mut reserved = RunBudget::default();
        for (_, held) in &holders {
            reserved.charge(*held);
        }
        // Room is kept only for a waiter that fits once live turns drain:
        // memory other programs hold is not the queue's to wait out.
        let ahead = self
            .kept_for(waiting, running, cores)
            .filter(|(_, wanted)| memory.fits_once_drained(reserved.memory, wanted.memory));
        if let Some((_, wanted)) = ahead {
            reserved.charge(wanted);
        }
        reserved.charge(budget);
        // When no turn is running and nothing startable precedes this waiter,
        // admit against the floor so a larger reserve never deadlocks idle.
        let idle_head = running.is_empty() && ahead.is_none();
        let fits_memory = if idle_head {
            memory.fits_idle(reserved.memory)
        } else {
            memory.fits(reserved.memory)
        };
        // Live turns can still grow to their caps. Charging the full caps
        // also covers a just-admitted command that has not started yet.
        let short = if reserved.jobs > cores {
            format!(
                "waiting for {} cores, {cores} on this machine",
                reserved.jobs
            )
        } else if !fits_memory {
            let needed = memory
                .reserve_for(idle_head)
                .saturating_add(reserved.memory);
            let available = memory.available.map_or_else(
                || "no reading available".to_owned(),
                |bytes| format!("{} available", gib(bytes)),
            );
            format!("waiting for {}, {available}", gib(needed))
        } else {
            return Ok(());
        };
        Err(format!(
            "{short}; {}{}",
            holding(&holders),
            keeping_room(ahead)
        ))
    }

    /// The waiter this one keeps room for: the first that precedes it and
    /// has a free slot of its class, with the budget it would be charged.
    fn kept_for(
        &self,
        waiting: &[(String, RunClass)],
        running: &[(RunClass, u32)],
        cores: usize,
    ) -> Option<(RunClass, RunBudget)> {
        let now = now_nanos();
        let mine = Ticket::parse(&self.file_name())?.precedence(now);
        let mut ahead: Vec<_> = waiting
            .iter()
            .filter_map(|(name, _)| Some((Ticket::parse(name)?, name)))
            .filter(|(ticket, _)| ticket.precedence(now) < mine)
            .collect();
        ahead.sort_by_key(|(ticket, _)| ticket.precedence(now));
        ahead
            .into_iter()
            .find(|(ticket, name)| {
                self.queue
                    .has_free_slot(name, ticket.class, waiting, running)
            })
            .map(|(ticket, _)| {
                let budget = admitted_budget(ticket.class, ticket.requested, cores);
                (ticket.class, budget)
            })
    }

    /// Records what holds this waiter. Past [`STARVING`] it also writes it
    /// beside its ticket for `rosen run --slots`, at most every [`REPOST`].
    fn report(&self, reason: String) {
        let Ok(mut report) = self.report.lock() else {
            return;
        };
        let due = report
            .written
            .as_ref()
            .is_none_or(|(written, at)| *written != reason && at.elapsed() >= REPOST);
        if self.since.elapsed() >= STARVING && due {
            let path = self.queue.dir.join(reason_name(&self.file_name()));
            if std::fs::write(path, &reason).is_ok() {
                report.written = Some((reason.clone(), Instant::now()));
            }
        }
        report.reason = Some(reason);
    }

    /// What held this waiter at its last look, if it had to wait.
    pub fn waiting_for(&self) -> Option<String> {
        self.report.lock().ok()?.reason.clone()
    }

    /// The budget this waiter is admitted and charged on: what its caller
    /// asked for within the class's cap, or the cap.
    pub fn budget(&self) -> RunBudget {
        self.budget_on(machine_cores())
    }

    fn budget_on(&self, cores: usize) -> RunBudget {
        admitted_budget(self.class, self.requested, cores)
    }

    /// All current runs, then the waiters ahead in line.
    pub fn ahead(&self) -> io::Result<Vec<Entry>> {
        let mine = self.file_name();
        let waiting = self.queue.waiting()?;
        let ahead_in_line = waiting.iter().filter(|(name, _)| *name < mine).count();
        let line = self.queue.line()?;
        let running = line.iter().filter(|entry| entry.running);
        let waiters = line.iter().filter(|entry| !entry.running);
        Ok(running
            .chain(waiters.take(ahead_in_line))
            .cloned()
            .collect())
    }

    /// `None` when this waiter's class has a free slot left after the
    /// earlier waiters of its class take theirs and, while this waiter still
    /// yields, no more important run is waiting or going; otherwise why not.
    fn not_next(
        &self,
        waiting: &[(String, RunClass)],
        yielding: bool,
        slots: u32,
        running: &[(RunClass, u32)],
    ) -> Option<String> {
        let mine = self.file_name();
        let ahead = waiting.iter().take_while(|(name, _)| *name < mine);
        let earlier_of_class = ahead
            .clone()
            .filter(|(_, class)| *class == self.class)
            .count();
        let yields_to_waiter = yielding && ahead.clone().any(|(_, class)| *class != self.class);
        let yields_to_run = yielding && running.iter().any(|(class, _)| *class < self.class);
        let in_class = running
            .iter()
            .filter(|(class, _)| *class == self.class)
            .count();
        let free = usize::try_from(slots)
            .unwrap_or(usize::MAX)
            .saturating_sub(in_class);
        if yields_to_waiter || yields_to_run {
            Some("yielding to more important runs".to_owned())
        } else if earlier_of_class >= free {
            Some(format!("waiting for a free {} slot", self.class.name()))
        } else {
            None
        }
    }

    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        // The ticket goes first: a reason file without its ticket is taken
        // for a leftover and removed.
        let _ = std::fs::remove_file(&self.path);
        let reason = self.queue.dir.join(reason_name(&self.file_name()));
        let _ = std::fs::remove_file(reason);
    }
}

/// What a waiter's ticket name says about it, for the waiters behind it.
struct Ticket {
    class: RunClass,
    /// Nanoseconds since the Unix epoch.
    arrived: u128,
    requested: Option<RunBudget>,
}

impl Ticket {
    /// Reads `<rank>-<arrival>-<process>[~<jobs>x<memory>].wait`. A ticket
    /// an older `rosen` made has no budget and is charged its class's cap.
    fn parse(name: &str) -> Option<Self> {
        let class = ticket_class(name)?;
        let stem = name.strip_suffix(".wait")?;
        let (stem, requested) = match stem.split_once('~') {
            Some((stem, budget)) => {
                let (jobs, memory) = budget.split_once('x')?;
                let requested = RunBudget {
                    jobs: jobs.parse().ok()?,
                    memory: memory.parse().ok()?,
                };
                (stem, Some(requested))
            }
            None => (stem, None),
        };
        let arrived = stem.split('-').nth(1)?.parse().ok()?;
        Some(Self {
            class,
            arrived,
            requested,
        })
    }

    /// Admission order across the line: class, then arrival. An aged waiter
    /// no longer yields to more important classes, so it ranks with the most
    /// important and only earlier arrivals precede it. One total order keeps
    /// two waiters from each keeping room for the other.
    fn precedence(&self, now: u128) -> (u8, u128) {
        let waited = now.saturating_sub(self.arrived);
        let rank = if waited >= self.class.patience().as_nanos() {
            RunClass::Landing.rank()
        } else {
            self.class.rank()
        };
        (rank, self.arrived)
    }
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
}

fn nanos_to_duration(nanos: u128) -> Duration {
    let secs = u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX);
    let subsec = (nanos % 1_000_000_000) as u32;
    Duration::new(secs, subsec)
}

/// The budget a waiter is admitted and charged on: what its caller asked
/// for within the class's cap, or the cap.
fn admitted_budget(class: RunClass, requested: Option<RunBudget>, cores: usize) -> RunBudget {
    let cap = class.budget_on(cores);
    requested.map_or(cap, |requested| requested.within(cap))
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

/// The live turns, as a wait reason names them.
fn holding(holders: &[(RunClass, RunBudget)]) -> String {
    let turns: Vec<_> = holders
        .iter()
        .map(|(class, budget)| format!("{} ({})", class.name(), gib(budget.memory)))
        .collect();
    match turns.as_slice() {
        [] => "no turn is running".to_owned(),
        [turn] => format!("{turn} holds a turn"),
        turns => format!("{} hold turns", turns.join(", ")),
    }
}

fn keeping_room(ahead: Option<(RunClass, RunBudget)>) -> String {
    ahead.map_or_else(String::new, |(class, budget)| {
        format!(
            "; keeping room for {} ({}) ahead in line",
            class.name(),
            gib(budget.memory)
        )
    })
}

/// A run's turn, holding one slot of its class. The next run of its class
/// may take that slot when this is dropped, or when its process dies.
#[derive(Debug)]
pub struct Turn {
    _lock: File,
    budget: RunBudget,
    path: PathBuf,
}

impl Turn {
    pub fn budget(&self) -> RunBudget {
        self.budget
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The class a ticket's file name gives it; `None` for any other file.
fn ticket_class(name: &str) -> Option<RunClass> {
    let rank = name.strip_suffix(".wait")?.split('-').next()?;
    RunClass::ALL
        .into_iter()
        .find(|class| class.rank().to_string() == rank)
}

/// Whether the ticket at `path` is no longer held by a live process.
fn owner_is_gone(path: &Path) -> bool {
    match File::open(path) {
        Ok(ticket) => unlocked(&ticket),
        // Removed between the listing and now.
        Err(_) => true,
    }
}

/// Whether no process holds `file` locked. Takes and at once gives back a
/// shared lock to find out.
fn unlocked(file: &File) -> bool {
    file.try_lock_shared().is_ok()
}

#[cfg(windows)]
fn machine_dir() -> io::Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))?;
    Ok(PathBuf::from(local).join("rosen").join("run-queue"))
}

#[cfg(not(windows))]
fn machine_dir() -> io::Result<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(runtime).join("rosen").join("run-queue"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(PathBuf::from(home)
        .join(".cache")
        .join("rosen")
        .join("run-queue"))
}

#[cfg(test)]
mod tests;
