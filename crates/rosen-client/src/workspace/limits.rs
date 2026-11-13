//! Machine-wide materialisation admission (SPEC R3).
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// How long a workspace's measured size is trusted before an admission
/// attempt measures it again. Agents' builds grow workspaces, so sizes go
/// stale, but walking every workspace on every attempt made queued
/// admissions slower the more workspaces the machine held.
const REFRESH_AFTER_SECS: u64 = 30;
/// Bounds the walking one attempt does, so no single caller pays for
/// re-measuring every workspace on the machine at once.
const REFRESHES_PER_ATTEMPT: usize = 4;
/// The slot of a reservation that never held a materialisation lease.
const NO_SLOT: usize = usize::MAX;

#[derive(Serialize, Deserialize)]
struct Reservation {
    destination: PathBuf,
    staging: PathBuf,
    /// The admitted estimate. It is charged until the paths are measured.
    bytes: u64,
    slot: usize,
    /// Last measured logical sizes of `destination` and `staging`.
    #[serde(default)]
    sizes: Option<[u64; 2]>,
    /// Unix seconds when a measurement of this entry last started.
    #[serde(default)]
    checked_at: u64,
}

impl Reservation {
    fn paths(&self) -> [&PathBuf; 2] {
        [&self.destination, &self.staging]
    }
}

#[derive(Serialize, Deserialize)]
struct Registry {
    settings: Settings,
    workspaces: Vec<Reservation>,
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

/// Entries, by destination and staging, that one attempt re-measures after
/// releasing the registry lock.
type Stale = Vec<(PathBuf, PathBuf)>;

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
            })?;
        }
        Ok(budget)
    }

    /// Waits without changing any existing workspace when disk or slots are full.
    pub fn acquire(&self, destination: &Path, staging: &Path, bytes: u64) -> io::Result<Permit> {
        let destination = std::path::absolute(destination)?;
        loop {
            self.refresh()?;
            if let Some(permit) = self.attempt(&destination, staging, bytes)? {
                return Ok(permit);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Re-measures a few stale workspaces before a decision. The walks run
    /// with the registry unlocked, so a pool take or another admission never
    /// waits behind them.
    fn refresh(&self) -> io::Result<()> {
        let stale = {
            let _lock = self.lock()?;
            let mut registry = self.read()?;
            let (_, pruned) = self.settle(&mut registry)?;
            let stale = self.claim_stale(&mut registry)?;
            if pruned || !stale.is_empty() {
                self.save(&registry)?;
            }
            stale
        };
        self.remeasure(stale)
    }

    /// One admission decision under the registry lock, from cached sizes.
    fn attempt(&self, destination: &Path, staging: &Path, bytes: u64) -> io::Result<Option<Permit>> {
        let _lock = self.lock()?;
        let mut registry = self.read()?;
        let (used, pruned) = self.settle(&mut registry)?;
        let mut permit = None;
        if bytes <= registry.settings.disk_bytes.saturating_sub(used) {
            if let Some((slot, file)) = self.free_slot(registry.settings.materialisations)? {
                registry.workspaces.push(Reservation {
                    destination: destination.to_path_buf(),
                    staging: staging.to_path_buf(),
                    bytes,
                    slot,
                    sizes: None,
                    // The estimate is the copy's exact size; builds that grow
                    // the workspace later are caught by the next refresh.
                    checked_at: now_secs(),
                });
                permit = Some(Permit { _slot: file });
            }
        }
        if pruned || permit.is_some() {
            self.save(&registry)?;
        }
        Ok(permit)
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
        self.settle(&mut registry)?;
        let from = std::path::absolute(from)?;
        // The taken workspace keeps the charge its source last had, so a take
        // never walks its files.
        let (bytes, checked_at) = registry
            .workspaces
            .iter()
            .rev()
            .find(|entry| entry.destination == from)
            .map_or((0, 0), |entry| {
                let bytes = entry.sizes.map_or(entry.bytes, |sizes| sizes[0]);
                (bytes, entry.checked_at)
            });
        // Record both names before renaming. A crash on either side of the
        // rename therefore charges whichever name survives.
        registry.workspaces.push(Reservation {
            destination: std::path::absolute(to)?,
            staging: from.clone(),
            bytes,
            slot: NO_SLOT,
            sizes: None,
            checked_at,
        });
        self.save(&registry)?;
        fs::rename(&from, to)
    }

    fn lock(&self) -> io::Result<File> {
        let file = open_lock(&self.root.join("registry.lock"))?;
        file.lock()?;
        Ok(file)
    }

    fn read(&self) -> io::Result<Registry> {
        serde_json::from_slice(&fs::read(self.root.join("registry.json"))?)
            .map_err(io::Error::other)
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

    /// Whether a copy still holds `slot`. Probing takes and at once drops
    /// the lease, which is safe because slots are only granted under the
    /// registry lock the caller holds.
    fn slot_held(&self, slot: usize) -> io::Result<bool> {
        Ok(slot != NO_SLOT && !try_lock(&self.slot(slot)?)?)
    }

    /// Totals the machine's usage from cached sizes and drops entries that
    /// charge nothing: no live lease, and every path gone or owned by a newer
    /// entry. Costs one stat per path, never a walk. Returns the usage and
    /// whether any entry was dropped.
    fn settle(&self, registry: &mut Registry) -> io::Result<(u64, bool)> {
        let mut used = 0u64;
        let mut slots = HashSet::new();
        let mut owned = HashSet::new();
        let mut keep = vec![false; registry.workspaces.len()];
        for (index, entry) in registry.workspaces.iter().enumerate().rev() {
            let mut charges = false;
            let mut actual = 0u64;
            for (side, path) in entry.paths().into_iter().enumerate() {
                if owned.insert(path.clone()) && exists(path)? {
                    charges = true;
                    let size = entry.sizes.map_or(0, |sizes| sizes[side]);
                    actual = actual.saturating_add(size);
                }
            }
            // Only the newest reservation owns a reused slot. Older workspaces
            // still count their files, but never inherit that slot's new lease.
            let active = slots.insert(entry.slot) && self.slot_held(entry.slot)?;
            if active || (charges && entry.sizes.is_none()) {
                actual = actual.max(entry.bytes);
            }
            keep[index] = active || charges;
            if keep[index] {
                used = used.saturating_add(actual);
            }
        }
        let before = registry.workspaces.len();
        let mut flags = keep.into_iter();
        registry.workspaces.retain(|_| flags.next().unwrap_or(true));
        Ok((used, registry.workspaces.len() != before))
    }

    /// Picks the longest-unmeasured entries and stamps them, so concurrent
    /// waiters spread the walking instead of repeating it.
    fn claim_stale(&self, registry: &mut Registry) -> io::Result<Stale> {
        let now = now_secs();
        let mut candidates = Vec::new();
        for (index, entry) in registry.workspaces.iter().enumerate() {
            let due = now.saturating_sub(entry.checked_at) >= REFRESH_AFTER_SECS;
            // A live copy is charged its estimate; measuring it mid-write is wasted.
            if due && !self.slot_held(entry.slot)? {
                candidates.push((entry.checked_at, index));
            }
        }
        candidates.sort_unstable();
        let mut stale = Vec::new();
        for (_, index) in candidates.into_iter().take(REFRESHES_PER_ATTEMPT) {
            let entry = &mut registry.workspaces[index];
            entry.checked_at = now;
            stale.push((entry.destination.clone(), entry.staging.clone()));
        }
        Ok(stale)
    }

    /// Walks the claimed entries without the lock, then stores their sizes.
    fn remeasure(&self, stale: Stale) -> io::Result<()> {
        let measured: Vec<_> = stale
            .into_iter()
            .filter_map(|(destination, staging)| {
                // A walk that fails, such as on a workspace being deleted, is
                // retried at the next refresh; a deleted one is pruned instead.
                let sizes = [disk_bytes(&destination).ok()?, disk_bytes(&staging).ok()?];
                Some((destination, staging, sizes))
            })
            .collect();
        if measured.is_empty() {
            return Ok(());
        }
        let _lock = self.lock()?;
        let mut registry = self.read()?;
        for (destination, staging, sizes) in measured {
            let entry = registry
                .workspaces
                .iter_mut()
                .rev()
                .find(|entry| entry.destination == destination && entry.staging == staging);
            if let Some(entry) = entry {
                entry.sizes = Some(sizes);
            }
        }
        self.save(&registry)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
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
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        } else {
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok(bytes)
}
