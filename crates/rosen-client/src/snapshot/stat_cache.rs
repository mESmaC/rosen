//! Each tracked file's size and timestamp as last seen holding a known blob,
//! kept in the workspace's state directory. The index starts every
//! snapshotter with no disk stats, so without this a full scan by a new one
//! (each `rosen commit`, a watcher start, the first snapshot of a
//! materialised workspace) read and hashed every tracked file: minutes on a
//! large tree (#174).
//!
//! Only trusted stats are kept (see [`Stat::trusted_at`]). A cached stat that
//! no longer matches the disk, or whose blob is not the index's, costs a
//! reread, never a wrong snapshot. Deletion (R4) never uses it: its guard
//! rereads every file.

use super::index::{Index, Stat};
use super::STATE_DIR;
use rosen_core::ObjectId;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use std::{fs, io};

const FILE: &str = "stat-cache.json";

/// Size, then the modification time as seconds and nanoseconds since the
/// Unix epoch, then the blob the file held.
type Cached = (u64, u64, u32, ObjectId);

fn path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join(FILE)
}

/// Gives each index entry without a stat the cached one, when the cache saw
/// the file holding the entry's blob. A missing or unreadable cache gives
/// nothing.
pub fn apply(root: &Path, index: &mut Index) {
    let Ok(bytes) = fs::read(path(root)) else {
        return;
    };
    let Ok(cached) = serde_json::from_slice::<BTreeMap<String, Cached>>(&bytes) else {
        return;
    };
    for (rel, (len, secs, nanos, blob)) in cached {
        let Some(entry) = index.files.get_mut(&rel) else {
            continue;
        };
        if entry.blob == blob && entry.stat.is_none() {
            let modified = UNIX_EPOCH + Duration::new(secs, nanos);
            entry.stat = Some(Stat { len, modified });
        }
    }
}

/// Saves the stat of every index entry that has one. Entries get a stat only
/// when it was trusted, from a read or from the cache.
pub fn save(root: &Path, index: &Index) -> io::Result<()> {
    let stats = index
        .files
        .iter()
        .filter_map(|(rel, entry)| Some((rel.as_str(), entry.stat?, entry.blob)));
    write(root, stats)
}

/// Saves the stats of files a materialisation just wrote, each with the blob
/// it was written from and its metadata, keeping only those trusted now.
pub fn seed(root: &Path, written: &[(String, ObjectId, fs::Metadata)]) -> io::Result<()> {
    let now = std::time::SystemTime::now();
    let stats = written.iter().filter_map(|(rel, blob, meta)| {
        let stat = super::scan::stat_of(meta).filter(|stat| stat.trusted_at(now))?;
        Some((rel.as_str(), stat, *blob))
    });
    write(root, stats)
}

/// Replaces the cache in one rename, so a reader sees all of it or the last.
fn write<'a>(
    root: &Path,
    stats: impl Iterator<Item = (&'a str, Stat, ObjectId)>,
) -> io::Result<()> {
    let cached: BTreeMap<&str, Cached> = stats
        .filter_map(|(rel, stat, blob)| {
            let since = stat.modified.duration_since(UNIX_EPOCH).ok()?;
            Some((rel, (stat.len, since.as_secs(), since.subsec_nanos(), blob)))
        })
        .collect();
    let bytes = serde_json::to_vec(&cached).map_err(io::Error::other)?;
    let target = path(root);
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
    }
    let temp = target.with_extension("json.tmp");
    fs::write(&temp, bytes)?;
    fs::rename(temp, target)
}
