//! Machine-wide materialisation admission (SPEC R3).
//!
//! Admission runs under one machine-wide registry lock, so it must stay
//! cheap: it adds up sizes stored in the registry and never walks a
//! directory. One process at a time rescans the workspaces' files on disk,
//! walking them with no registry lock held, and stores what it measured.
//! Reservations made since that scan are charged the bytes they reserved.
//! Each scan walks for a bounded time, least recently measured first, so
//! the request that runs it waits about that long however large the
//! machine, and a scan cut short by its process exiting loses little.
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Initial settings, persisted on first use so every client shares one cap.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub materialisations: usize,
    pub disk_bytes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            materialisations: 4,
            disk_bytes: 100 * 1024 * 1024 * 1024,
        }
    }
}

/// The slot of a pool take's record, which never holds a slot lease.
const NO_SLOT: usize = usize::MAX;

/// How often a queued request retries admission.
const POLL: Duration = Duration::from_millis(100);

/// The youngest scan that is rescanned. A scan slower than this waits as
/// long as it took, so scanning never keeps the disk busy more than half
/// the time however many workspaces there are.
const RESCAN_AFTER_MS: u64 = 1000;

/// A scan starts no new workspace walk after this long. Walks already
/// started finish, and the first is always started, so every scan makes
/// progress even when one workspace takes longer than this to walk.
const SCAN_SLICE: Duration = Duration::from_millis(200);

#[derive(Serialize, Deserialize)]
struct Reservation {
    destination: PathBuf,
    staging: PathBuf,
    bytes: u64,
    slot: usize,
    /// Unique and increasing, so a scan's results attach to the records it
    /// saw. Records written before sequences existed read as 0.
    #[serde(default)]
    sequence: u64,
    /// The bytes its paths held at the last scan; `None` until a scan sees it.
    #[serde(default)]
    measured: Option<u64>,
    /// Its copy was running during that scan, so the measure may be partial.
    #[serde(default)]
    measured_in_flight: bool,
    /// When that scan measured it, in Unix milliseconds; 0 if never.
    #[serde(default)]
    measured_ms: u64,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct ScanTime {
    finished_ms: u64,
    took_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct Registry {
    settings: Settings,
    workspaces: Vec<Reservation>,
    #[serde(default)]
    next_sequence: u64,
    #[serde(default)]
    last_scan: Option<ScanTime>,
}

/// One record's share of a scan: its paths no newer record also names.
struct ScanTarget {
    sequence: u64,
    paths: Vec<PathBuf>,
    in_flight: bool,
    measured_ms: u64,
}

/// Coordinates independent clients and daemon pool workers through OS locks.
/// Locks are released by the OS on a crash; persistent paths retain disk usage.
pub struct Budget {
    root: PathBuf,
}

/// Holds a machine slot until the blocking copy actually completes.
pub struct Permit {
    _slot: File,
}

impl Budget {
    pub fn open(root: &Path, settings: Settings) -> io::Result<Self> {
        if settings.materialisations == 0 || settings.disk_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "load caps must be positive",
            ));
        }
        fs::create_dir_all(root)?;
        let budget = Self {
            root: root.to_path_buf(),
        };
        let _lock = budget.lock()?;
        if !budget.root.join("registry.json").exists() {
            budget.save(&Registry {
                settings,
                workspaces: Vec::new(),
                next_sequence: 0,
                last_scan: None,
            })?;
        }
        Ok(budget)
    }

    /// Waits without changing any existing workspace when disk or slots are full.
    pub fn acquire(&self, destination: &Path, staging: &Path, bytes: u64) -> io::Result<Permit> {
        let destination = std::path::absolute(destination)?;
        loop {
            let lock = self.lock()?;
            let registry = self.read()?;
            if scan_due(&registry) {
                if let Some(lease) = self.scan_lease()? {
                    drop(lock);
                    self.scan(lease)?;
                    continue;
                }
            }
            if let Some(slot) = self.admit(registry, &destination, staging, bytes)? {
                return Ok(Permit { _slot: slot });
            }
            drop(lock);
            std::thread::sleep(POLL);
        }
    }

    /// Records the reservation and returns its slot lease when the stored
    /// sizes leave room and a slot is free. Caller holds the registry lock.
    fn admit(
        &self,
        mut registry: Registry,
        destination: &Path,
        staging: &Path,
        bytes: u64,
    ) -> io::Result<Option<File>> {
        // Before the first scan finishes, existing workspaces are unknown.
        if registry.last_scan.is_none() {
            return Ok(None);
        }
        let free = registry
            .settings
            .disk_bytes
            .saturating_sub(self.usage(&registry)?);
        if bytes > free {
            return Ok(None);
        }
        let Some((slot, file)) = self.free_slot(registry.settings.materialisations)? else {
            return Ok(None);
        };
        let (destination, staging) = (destination.to_path_buf(), staging.to_path_buf());
        push_reservation(&mut registry, destination, staging, bytes, slot);
        self.save(&registry)?;
        Ok(Some(file))
    }

    /// Raises or lowers disk admission without disturbing existing workspaces.
    /// A queued request observes the new cap on its next admission attempt.
    pub fn set_disk_cap(&self, bytes: u64) -> io::Result<()> {
        let _lock = self.lock()?;
        let mut registry = self.read()?;
        registry.settings.disk_bytes = bytes;
        self.save(&registry)
    }

    /// Keeps pool workspaces charged when atomically taken into a new path.
    pub fn rename_workspace(&self, from: &Path, to: &Path) -> io::Result<()> {
        let _lock = self.lock()?;
        let mut registry = self.read()?;
        // Record both names before renaming. A crash on either side of the
        // rename therefore charges whichever name survives.
        let (to_absolute, from_absolute) = (std::path::absolute(to)?, std::path::absolute(from)?);
        // A scan running now may already have snapshotted `from` and will
        // measure it empty after the rename. So the workspace's charge moves
        // from the record that made it to this one, which keeps it until a
        // scan measures the workspace as `to`.
        let carried = match registry
            .workspaces
            .iter_mut()
            .rev()
            .find(|entry| entry.destination == from_absolute)
        {
            Some(maker) => {
                let carried = charge(maker, false);
                maker.measured = Some(0);
                maker.measured_in_flight = false;
                carried
            }
            None => 0,
        };
        push_reservation(&mut registry, to_absolute, from_absolute, carried, NO_SLOT);
        self.save(&registry)?;
        rename_while_walked(from, to)
    }

    fn lock(&self) -> io::Result<File> {
        let file = open_lock(&self.root.join("registry.lock"))?;
        file.lock()?;
        Ok(file)
    }

    /// Held by the one process scanning; the OS frees it if that process dies.
    fn scan_lease(&self) -> io::Result<Option<File>> {
        let file = open_lock(&self.root.join("scan.lock"))?;
        Ok(try_lock(&file)?.then_some(file))
    }

    fn read(&self) -> io::Result<Registry> {
        let mut registry: Registry =
            serde_json::from_slice(&fs::read(self.root.join("registry.json"))?)
                .map_err(io::Error::other)?;
        // A client from before sequences existed rewrites the registry
        // without them. Number its records after every surviving one, so a
        // scan can still tell all records apart.
        let highest = registry.workspaces.iter().map(|entry| entry.sequence).max();
        registry.next_sequence = registry.next_sequence.max(highest.unwrap_or(0));
        for entry in &mut registry.workspaces {
            if entry.sequence == 0 {
                registry.next_sequence += 1;
                entry.sequence = registry.next_sequence;
            }
        }
        Ok(registry)
    }

    fn save(&self, registry: &Registry) -> io::Result<()> {
        let data = serde_json::to_vec(registry).map_err(io::Error::other)?;
        atomicwrites::AtomicFile::new(
            self.root.join("registry.json"),
            atomicwrites::AllowOverwrite,
        )
        .write(|file| {
            file.write_all(&data)?;
            file.sync_all()
        })
        .map_err(io::Error::other)
    }

    fn slot(&self, slot: usize) -> io::Result<File> {
        open_lock(&self.root.join(format!("slot-{slot}.lock")))
    }

    fn free_slot(&self, count: usize) -> io::Result<Option<(usize, File)>> {
        for slot in 0..count {
            let file = self.slot(slot)?;
            if try_lock(&file)? {
                return Ok(Some((slot, file)));
            }
        }
        Ok(None)
    }

    /// The records whose materialisation still holds its slot lease. Only
    /// the newest record owns a reused slot; older workspaces still count
    /// their files, but never inherit that slot's new lease.
    fn active(&self, registry: &Registry) -> io::Result<HashSet<u64>> {
        let mut slots = HashSet::new();
        let mut active = HashSet::new();
        for entry in registry.workspaces.iter().rev() {
            if entry.slot != NO_SLOT
                && slots.insert(entry.slot)
                && !try_lock(&self.slot(entry.slot)?)?
            {
                active.insert(entry.sequence);
            }
        }
        Ok(active)
    }

    fn usage(&self, registry: &Registry) -> io::Result<u64> {
        let active = self.active(registry)?;
        Ok(registry
            .workspaces
            .iter()
            .map(|entry| charge(entry, active.contains(&entry.sequence)))
            .fold(0, u64::saturating_add))
    }

    /// Measures workspaces' files without holding the registry lock, so
    /// admissions and pool takes never wait behind a directory walk.
    fn scan(&self, _lease: File) -> io::Result<()> {
        let started = Instant::now();
        let targets = self.prune_and_snapshot()?;
        let measured = measure(&targets, started)?;
        let _lock = self.lock()?;
        let mut registry = self.read()?;
        let measured_ms = now_ms();
        for entry in &mut registry.workspaces {
            if let Some(&(bytes, in_flight)) = measured.get(&entry.sequence) {
                entry.measured = Some(bytes);
                entry.measured_in_flight = in_flight;
                entry.measured_ms = measured_ms;
            }
        }
        registry.last_scan = Some(ScanTime {
            finished_ms: measured_ms,
            took_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
        self.save(&registry)
    }

    /// Drops records that no longer charge anything, and lists the paths
    /// each remaining record owns. A record charges nothing when its copy
    /// is not running and each of its paths is gone or named by a newer
    /// record. Without this the registry grows by a record per
    /// materialisation and per take, forever. Records measured longest ago
    /// come first; running copies come last, since each is charged at least
    /// its reservation whatever a walk finds.
    fn prune_and_snapshot(&self) -> io::Result<Vec<ScanTarget>> {
        let _lock = self.lock()?;
        let mut registry = self.read()?;
        let active = self.active(&registry)?;
        let mut claimed = HashSet::new();
        let mut targets = Vec::new();
        for entry in registry.workspaces.iter().rev() {
            let mut paths = Vec::new();
            for path in [&entry.destination, &entry.staging] {
                if claimed.insert(path.clone()) && exists(path)? {
                    paths.push(path.clone());
                }
            }
            let in_flight = active.contains(&entry.sequence);
            if in_flight || !paths.is_empty() {
                targets.push(ScanTarget {
                    sequence: entry.sequence,
                    paths,
                    in_flight,
                    measured_ms: entry.measured_ms,
                });
            }
        }
        targets.sort_by_key(|target| (target.in_flight, target.measured_ms));
        let kept: HashSet<u64> = targets.iter().map(|target| target.sequence).collect();
        registry
            .workspaces
            .retain(|entry| kept.contains(&entry.sequence));
        self.save(&registry)?;
        Ok(targets)
    }
}

/// The longest a take retries a rename that a scan's walk is blocking.
const RENAME_RETRY: Duration = Duration::from_secs(2);

/// On Windows, renaming a directory fails with PermissionDenied while any
/// process holds a handle inside it. A scan walks workspaces with no
/// registry lock, so it can hold one briefly during a take; each handle
/// lives for one metadata or directory read, so a short retry gets past it.
/// The walk never waits on the registry lock this caller holds, so the
/// retry cannot deadlock.
fn rename_while_walked(from: &Path, to: &Path) -> io::Result<()> {
    let started = Instant::now();
    let mut delay = Duration::from_millis(2);
    loop {
        match fs::rename(from, to) {
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && started.elapsed() < RENAME_RETRY =>
            {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(100));
            }
            result => return result,
        }
    }
}

fn push_reservation(
    registry: &mut Registry,
    destination: PathBuf,
    staging: PathBuf,
    bytes: u64,
    slot: usize,
) {
    registry.next_sequence += 1;
    registry.workspaces.push(Reservation {
        destination,
        staging,
        bytes,
        slot,
        sequence: registry.next_sequence,
        measured: None,
        measured_in_flight: false,
        measured_ms: 0,
    });
}

/// A running copy is charged at least what it reserved; a record the last
/// scan has not seen is charged only that.
fn charge(entry: &Reservation, active: bool) -> u64 {
    match entry.measured {
        None => entry.bytes,
        Some(measured) if active || entry.measured_in_flight => measured.max(entry.bytes),
        Some(measured) => measured,
    }
}

fn scan_due(registry: &Registry) -> bool {
    let Some(scan) = registry.last_scan else {
        return true;
    };
    // A clock that went backwards makes the age unknown, so rescan.
    now_ms()
        .checked_sub(scan.finished_ms)
        .is_none_or(|age| age >= RESCAN_AFTER_MS.max(scan.took_ms))
}

/// Walks targets in order, in parallel since each walk waits mostly on the
/// file system, starting none after the scan's slice; returns the walked.
fn measure(targets: &[ScanTarget], started: Instant) -> io::Result<HashMap<u64, (u64, bool)>> {
    let walked: Vec<OnceLock<u64>> = targets.iter().map(|_| OnceLock::new()).collect();
    let indexed: Vec<usize> = (0..targets.len()).collect();
    super::parallel::for_each(&indexed, |&index| {
        if index > 0 && started.elapsed() >= SCAN_SLICE {
            return Ok(());
        }
        let mut bytes = 0u64;
        for path in &targets[index].paths {
            bytes = bytes.saturating_add(disk_bytes(path)?);
        }
        let _ = walked[index].set(bytes);
        Ok::<(), io::Error>(())
    })?;
    Ok(targets
        .iter()
        .zip(walked)
        .filter_map(|(target, bytes)| {
            let bytes = bytes.into_inner()?;
            Some((target.sequence, (bytes, target.in_flight)))
        })
        .collect())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn try_lock(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Does not follow symlinks into another workspace or outside its root.
/// Runs with no lock held, so a directory removed or renamed mid-walk,
/// such as a staging directory being published, counts as empty.
pub(super) fn disk_bytes(root: &Path) -> io::Result<u64> {
    let mut bytes = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_dir() && !metadata.is_symlink() {
            let entries = match fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for entry in entries {
                pending.push(entry?.path());
            }
        } else {
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok(bytes)
}
