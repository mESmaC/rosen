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
//!   `landing` and `build` get at least two, sized from the machine's cores
//!   and memory; `verify` and `prepare` one. A machine can set its own.
//! - A run starts when a slot of its class is free for it, no more important
//!   run is going, and nothing sorts ahead of it in line: every more
//!   important waiter, and earlier waiters of its own class, which take the
//!   free slots first.
//! - Every new turn also needs free memory above the machine's reserve.
//!   Priority aging never bypasses this gate; running turns are not stopped.
//! - A less important run that is already going is never stopped and never
//!   holds a more important one back. With memory to spare a landing check starts
//!   beside a prepare; the caller runs `verify` and `prepare` work at low
//!   process priority, and the landing check gets the cores.
//! - Priority alone cannot hold a run forever: after its patience a waiter stops
//!   yielding to more important classes and needs only a free slot of its
//!   own class and sufficient memory.
//!   A stuck landing check then cannot hold every agent's build, and steady
//!   agent builds cannot keep the tests after a land from running when memory
//!   permits.
//!
//! The queue is files in one folder per machine and user, with operating
//! system file locks, so it needs no daemon and survives any crash: a
//! waiter holds its ticket file locked and a runner one of its class's slot
//! lock files, and the system releases both when their process dies.
//!
//! It only schedules. A caller that cannot use it (an unwritable folder)
//! runs its command anyway.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often a waiter looks at the line again.
pub const POLL: Duration = Duration::from_millis(50);
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
                let cores = std::thread::available_parallelism().map_or(1, |cores| cores.get());
                sized_slots(cores, rosen_platform::memory::physical_memory())
            }),
            Self::Verify | Self::Prepare => 1,
        }
    }
}

static MACHINE_SLOTS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// Logical cores each concurrent heavy run is given. In the SPEC R3 ramp on
/// 20 cores, clean builds of Rosen and Velocity still did more work per
/// second at four at once than at three (3.0 and 1.65 times one alone), so
/// four on that machine; the ramp went no further, so neither does this.
const CORES_PER_SLOT: usize = 5;
/// Memory each concurrent heavy run is given. In the ramp, free memory fell
/// by under 2 GiB per added build; the rest is left for links, tests and
/// the agents and tools beside them, so the runs stay out of the page file.
const MEMORY_PER_SLOT: u64 = 8 << 30;
/// Runs per class that every machine gets, however small: two runs side by
/// side still beat one on any machine with more than one core.
const MIN_SLOTS: u32 = 2;

/// The slot count for `cores` logical cores and `memory` bytes of memory:
/// whichever runs out first, and never under [`MIN_SLOTS`].
pub fn sized_slots(cores: usize, memory: Option<u64>) -> u32 {
    let by_cores = u32::try_from(cores / CORES_PER_SLOT).unwrap_or(u32::MAX);
    let by_memory = memory.map_or(u32::MAX, |bytes| {
        u32::try_from(bytes / MEMORY_PER_SLOT).unwrap_or(u32::MAX)
    });
    by_cores.min(by_memory).max(MIN_SLOTS)
}

/// A run in the queue, as [`RunQueue::line`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub class: RunClass,
    /// What the run is, as its owner described it.
    pub note: String,
    pub running: bool,
}

/// A current sample of the memory gate, in bytes. Unknown readings leave the
/// queue usable on systems that cannot report memory, as with slot sizing.
#[derive(Clone, Copy, Debug)]
pub struct MemoryHeadroom {
    pub available: Option<u64>,
    pub reserve: Option<u64>,
}

impl MemoryHeadroom {
    pub fn is_low(self) -> bool {
        matches!((self.available, self.reserve), (Some(free), Some(reserve)) if free <= reserve)
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
        // Sorts into queue order: class, then arrival, then process.
        let path = self
            .dir
            .join(format!("{}-{arrived:030}-{process}.wait", class.rank()));
        if let Err(error) = std::fs::rename(&unlisted, &path) {
            let _ = std::fs::remove_file(&unlisted);
            return Err(error);
        }
        Ok(Waiter {
            queue: self.clone(),
            class,
            note: note.to_owned(),
            path,
            _ticket: ticket,
            since: Instant::now(),
        })
    }

    /// Waits in line on this thread and returns the turn. Blocking: call it
    /// off an async runtime's threads.
    pub fn turn(&self, class: RunClass, note: &str) -> io::Result<Turn> {
        let waiter = self.wait(class, note)?;
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
        for class in RunClass::ALL {
            for slot in self.running(class)? {
                let note = std::fs::read_to_string(self.note_path(class, slot)).unwrap_or_default();
                line.push(Entry {
                    class,
                    note,
                    running: true,
                });
            }
        }
        for (_, class) in self.waiting()? {
            // A waiter's note is known only once it runs: its ticket is
            // locked, and a locked file can't be read on Windows.
            line.push(Entry {
                class,
                note: String::new(),
                running: false,
            });
        }
        Ok(line)
    }

    /// The live tickets' file names with their classes, in queue order. A
    /// ticket whose owner died is removed on the way.
    fn waiting(&self) -> io::Result<Vec<(String, RunClass)>> {
        let mut tickets = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let Some(class) = ticket_class(&name) else {
                self.remove_abandoned(&name);
                continue;
            };
            if owner_is_gone(&self.dir.join(&name)) {
                let _ = std::fs::remove_file(self.dir.join(&name));
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

    /// The slots of `class` whose runs are going now. A slot beyond the
    /// class's count, left from before the machine lowered it, still counts
    /// while its run goes.
    fn running(&self, class: RunClass) -> io::Result<Vec<u32>> {
        let mut slots = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let Some(slot) = lock_slot(&name, class) else {
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

    /// Free commit on Windows or available memory elsewhere, and the machine's
    /// reserve. By default one fifth of RAM is kept for growing commands and
    /// other applications, independently of the class's slot count.
    pub fn memory_headroom(&self) -> MemoryHeadroom {
        let reserve = std::fs::read_to_string(self.dir.join(RESERVE_FILE))
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .filter(|bytes| *bytes > 0)
            .or_else(|| rosen_platform::memory::physical_memory().map(|bytes| bytes / 5));
        MemoryHeadroom {
            available: rosen_platform::memory::available_memory(),
            reserve,
        }
    }

    /// Sets the machine's reserve in bytes, or restores the RAM-based default.
    /// All classes and projects see the change on their next admission poll.
    pub fn set_memory_reserve(&self, bytes: Option<u64>) -> io::Result<()> {
        if bytes == Some(0) {
            return Err(io::Error::other("the memory reserve must be positive"));
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

/// A place in line. Dropping it leaves the line.
#[derive(Debug)]
pub struct Waiter {
    queue: RunQueue,
    class: RunClass,
    /// Shown to those waiting behind this run once it has its turn.
    note: String,
    path: PathBuf,
    /// Held locked for as long as this waiter lives.
    _ticket: File,
    since: Instant,
}

impl Waiter {
    /// The turn, when it is this waiter's; `None` while it has to wait.
    /// Never blocks.
    pub fn try_start(&self) -> io::Result<Option<Turn>> {
        let yielding = self.since.elapsed() < self.class.patience();
        let slots = self.queue.slots(self.class);
        if !self.is_next(yielding, slots)? {
            return Ok(None);
        }
        // Any free slot will do; trying each in turn takes the first one no
        // other run holds, so two runs never share a slot.
        for slot in 0..slots {
            let lock = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(self.queue.lock_path(self.class, slot))?;
            match lock.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => continue,
                Err(std::fs::TryLockError::Error(error)) => return Err(error),
            }
            // Sample after taking the slot, as close as possible to starting
            // its command. Low memory is a busy slot, not a queue error that
            // callers would handle by running without a turn.
            if self.queue.memory_headroom().is_low() {
                return Ok(None);
            }
            let _ = std::fs::write(self.queue.note_path(self.class, slot), &self.note);
            return Ok(Some(Turn { _lock: lock }));
        }
        Ok(None)
    }

    /// Those this waiter is behind: runs of its class or a more important
    /// one going now, then the waiters ahead of it in line.
    pub fn ahead(&self) -> io::Result<Vec<Entry>> {
        let mine = self.file_name();
        let waiting = self.queue.waiting()?;
        let ahead_in_line = waiting.iter().filter(|(name, _)| *name < mine).count();
        let line = self.queue.line()?;
        let running = line
            .iter()
            .filter(|entry| entry.running && entry.class <= self.class);
        let waiters = line.iter().filter(|entry| !entry.running);
        Ok(running
            .chain(waiters.take(ahead_in_line))
            .cloned()
            .collect())
    }

    /// Whether this waiter's class has a free slot left after the earlier
    /// waiters of its class take theirs and, while this waiter still yields,
    /// no more important run is waiting or going.
    fn is_next(&self, yielding: bool, slots: u32) -> io::Result<bool> {
        let mine = self.file_name();
        let mut earlier_of_class = 0;
        for (name, class) in self.queue.waiting()? {
            if name >= mine {
                break;
            }
            if class == self.class {
                earlier_of_class += 1;
            } else if yielding {
                return Ok(false);
            }
        }
        let running = self.queue.running(self.class)?.len();
        let free = usize::try_from(slots)
            .unwrap_or(usize::MAX)
            .saturating_sub(running);
        if earlier_of_class >= free {
            return Ok(false);
        }
        if yielding {
            for class in RunClass::ALL {
                if class < self.class && !self.queue.running(class)?.is_empty() {
                    return Ok(false);
                }
            }
        }
        Ok(true)
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
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A run's turn, holding one slot of its class. The next run of its class
/// may take that slot when this is dropped, or when its process dies.
#[derive(Debug)]
pub struct Turn {
    _lock: File,
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
