//! Copies that `GetObjectChunk` reads slices from (SPEC D8, issue #250).
//!
//! An object too large for one frame is read in slices. Inflating and
//! hashing the whole object again for every slice would cost a 1 GiB blob
//! 64 whole reads, and hold it in memory each time. So the first slice
//! inflates the object once, checking its hash as it streams, into a plain
//! file named for its ID and kind. Every later slice seeks into that file.
//! A copy unused for an hour is swept when the next one is made; a reader
//! that loses its copy to the sweep only pays for one more inflate.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use rosen_core::{ObjectId, ObjectKind};
use rosen_store::{LooseObjectStore, StoreError};

/// A copy untouched this long serves no download in progress: each slice
/// read marks its copy used.
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);
const KINDS: [ObjectKind; 3] = [ObjectKind::Blob, ObjectKind::Tree, ObjectKind::Commit];
/// Separates temporary copies made by concurrent readers of one object.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Reads up to `len` bytes at `offset` of object `id`, whose body the store
/// says is `total` bytes long, from its checked copy. Makes the copy first
/// if there is none. `None` when the store does not hold `id`.
pub fn read(
    store: &LooseObjectStore,
    dir: &Path,
    id: &ObjectId,
    total: u64,
    offset: u64,
    len: u64,
) -> Result<Option<(ObjectKind, Vec<u8>)>, StoreError> {
    let found = match open_copy(dir, id, total)? {
        Some(found) => Some(found),
        None if make_copy(store, dir, id)? => open_copy(dir, id, total)?,
        None => return Ok(None),
    };
    let Some((mut file, kind)) = found else {
        let why = format!("the checked copy of {id} is not {total} bytes long");
        return Err(io::Error::other(why).into());
    };
    let start = offset.min(total);
    let end = start.saturating_add(len).min(total);
    file.seek(SeekFrom::Start(start))?;
    let mut data = Vec::with_capacity(usize::try_from(end - start).unwrap_or(0));
    file.take(end - start).read_to_end(&mut data)?;
    Ok(Some((kind, data)))
}

/// Opens the copy of `id`, if one is intact, and marks it used.
fn open_copy(
    dir: &Path,
    id: &ObjectId,
    total: u64,
) -> Result<Option<(File, ObjectKind)>, StoreError> {
    for kind in KINDS {
        let path = copy_path(dir, id, kind);
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        // A copy is renamed into place unsynced, so a crash can leave one
        // short. Its length is checked against the object's own header on
        // every read, and a wrong one is made again.
        if file.metadata()?.len() != total {
            drop(file);
            let _ = fs::remove_file(&path);
            continue;
        }
        let _ = file.set_modified(SystemTime::now());
        return Ok(Some((file, kind)));
    }
    Ok(None)
}

/// Inflates `id` into a new copy, checking its hash, and answers false
/// when the store does not hold it. A concurrent reader making the same
/// copy is harmless: both copies are the same checked bytes.
fn make_copy(store: &LooseObjectStore, dir: &Path, id: &ObjectId) -> Result<bool, StoreError> {
    fs::create_dir_all(dir)?;
    sweep_stale(dir);
    let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!("{id}.{}-{sequence}.tmp", std::process::id()));
    let copied = (|| {
        let mut out = BufWriter::new(File::create_new(&temp)?);
        let copied = store.copy_raw_to(id, &mut out)?;
        out.into_inner().map_err(|error| error.into_error())?;
        Ok::<_, StoreError>(copied)
    })();
    let kind = match copied {
        Ok(Some((kind, _))) => kind,
        other => {
            let _ = fs::remove_file(&temp);
            return other.map(|_| false);
        }
    };
    let path = copy_path(dir, id, kind);
    if let Err(error) = fs::rename(&temp, &path) {
        let _ = fs::remove_file(&temp);
        // On Windows a copy another reader holds open cannot be replaced;
        // that copy serves as well.
        if !path.is_file() {
            return Err(error.into());
        }
    }
    Ok(true)
}

fn copy_path(dir: &Path, id: &ObjectId, kind: ObjectKind) -> PathBuf {
    dir.join(format!("{id}.{}", kind.as_str()))
}

/// Best effort, as for upload staging: a copy that cannot be removed now
/// is tried again when the next copy is made.
fn sweep_stale(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() > STALE_AFTER);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}
