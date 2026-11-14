//! The machine's run queue (SPEC R3 "Run queue"): one line for every heavy
//! command on a machine, so builds take turns instead of all running at once.
//!
//! Many agents compiling side by side each assume they have every core, and
//! all finish late together. The queue gives each kind of run a class:
//! `landing` (the checks a land waits on), `build` (an agent's own builds and
//! tests), `verify` (the full tests after a land, and bisection) and
//! `prepare` (the warm pool's prepare commands), most important first.
//!
//! - One run of each class at a time.
//! - A run starts when no run of its class or a more important one is
//!   running, and nothing sorts ahead of it in line: every more important
//!   waiter, and earlier waiters of its own class.
//! - A less important run that is already going is never stopped and never
//!   holds a more important one back. So a landing check starts at once
//!   beside a prepare; the caller runs `verify` and `prepare` work at low
//!   process priority, and the landing check gets the cores.
//! - Nothing waits for ever: after its class's patience a waiter stops
//!   yielding to more important classes and needs only its own class's turn.
//!   A stuck landing check then cannot hold every agent's build, and steady
//!   agent builds cannot keep the tests after a land from running.
//!
//! The queue is files in one folder per machine and user, with operating
//! system file locks, so it needs no daemon and survives any crash: a
//! waiter holds its ticket file locked and a runner its class's lock file,
//! and the system releases both when their process dies.
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
}

/// A run in the queue, as [`RunQueue::line`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub class: RunClass,
    /// What the run is, as its owner described it.
    pub note: String,
    pub running: bool,
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
            if self.is_running(class)? {
                let note = std::fs::read_to_string(self.note_path(class)).unwrap_or_default();
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

    fn is_running(&self, class: RunClass) -> io::Result<bool> {
        match File::open(self.lock_path(class)) {
            Ok(lock) => Ok(!unlocked(&lock)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn lock_path(&self, class: RunClass) -> PathBuf {
        self.dir.join(format!("run-{}.lock", class.name()))
    }

    /// Beside the lock, not in it: a locked file's bytes can't be read by
    /// another process on Windows.
    fn note_path(&self, class: RunClass) -> PathBuf {
        self.dir.join(format!("run-{}.note", class.name()))
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
        if !self.is_next(yielding)? {
            return Ok(None);
        }
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.queue.lock_path(self.class))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
        let _ = std::fs::write(self.queue.note_path(self.class), &self.note);
        Ok(Some(Turn { _lock: lock }))
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

    /// Whether no earlier waiter of this class is in line and, while this
    /// waiter still yields, no more important run is waiting or going.
    fn is_next(&self, yielding: bool) -> io::Result<bool> {
        let mine = self.file_name();
        for (name, class) in self.queue.waiting()? {
            if name >= mine {
                break;
            }
            if class == self.class || yielding {
                return Ok(false);
            }
        }
        if yielding {
            for class in RunClass::ALL {
                if class < self.class && self.queue.is_running(class)? {
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

/// A run's turn. The next run of its class may start when this is dropped,
/// or when its process dies.
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
