//! Machine-wide materialisation admission (SPEC R3).
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

#[derive(Serialize, Deserialize)]
struct Reservation {
    destination: PathBuf,
    staging: PathBuf,
    bytes: u64,
    slot: usize,
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
            let lock = self.lock()?;
            let mut registry = self.read()?;
            let used = self.usage(&registry)?;
            if bytes <= registry.settings.disk_bytes.saturating_sub(used) {
                if let Some((slot, file)) = self.free_slot(registry.settings.materialisations)? {
                    registry.workspaces.push(Reservation {
                        destination: destination.clone(),
                        staging: staging.to_path_buf(),
                        bytes,
                        slot,
                    });
                    self.save(&registry)?;
                    return Ok(Permit { _slot: file });
                }
            }
            drop(lock);
            std::thread::sleep(Duration::from_millis(100));
        }
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
        registry.workspaces.push(Reservation {
            destination: std::path::absolute(to)?,
            staging: std::path::absolute(from)?,
            bytes: 0,
            slot: usize::MAX,
        });
        self.save(&registry)?;
        fs::rename(from, to)
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

    fn usage(&self, registry: &Registry) -> io::Result<u64> {
        let mut used = 0u64;
        let mut slots = HashSet::new();
        let mut paths = HashSet::new();
        for entry in registry.workspaces.iter().rev() {
            let mut actual = 0u64;
            for path in [&entry.destination, &entry.staging] {
                if paths.insert(path) {
                    actual = actual.saturating_add(disk_bytes(path)?);
                }
            }
            // Only the newest reservation owns a reused slot. Older workspaces
            // still count their files, but never inherit that slot's new lease.
            let active = slots.insert(entry.slot) && !try_lock(&self.slot(entry.slot)?)?;
            used = used.saturating_add(if active {
                actual.max(entry.bytes)
            } else {
                actual
            });
        }
        Ok(used)
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
