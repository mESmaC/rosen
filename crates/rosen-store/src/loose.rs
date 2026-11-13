//! The git-format loose-object backend (SPEC D5, §3 "Object store").
//!
//! Each object is one zlib-compressed file at `objects/<2 hex>/<38 hex>`
//! holding `<kind> <len>\0<body>`: byte for byte what git writes, so a
//! directory used as `.git/objects` is readable by git directly. There are no
//! packfiles, so the store never needs repacking or other maintenance.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rosen_core::{ObjectId, ObjectKind};

use crate::backend::{ObjectStore, RawObject, StoreError};

/// Distinguishes temporary files written concurrently by threads of one
/// process; the process ID in the name separates processes.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A loose-object store rooted at an `objects` directory.
#[derive(Debug, Clone)]
pub struct LooseObjectStore {
    objects_dir: PathBuf,
}

impl LooseObjectStore {
    /// Opens the store at `objects_dir`, creating the directory if needed.
    pub fn open(objects_dir: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let objects_dir = objects_dir.into();
        fs::create_dir_all(&objects_dir)?;
        Ok(Self { objects_dir })
    }

    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    /// The file holding `id`; the first byte's hex names the fan-out directory.
    pub fn object_path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_hex();
        self.objects_dir.join(&hex[..2]).join(&hex[2..])
    }

    /// Compresses an object into a new temporary file in its fan-out
    /// directory and flushes it to disk. Publishing it is then a rename on
    /// the same volume, so readers never see a partial object.
    fn write_temp(&self, fan_out: &Path, kind: ObjectKind, data: &[u8]) -> io::Result<PathBuf> {
        let (temp, file) = create_temp(fan_out)?;
        let written = (|| {
            // Git also writes loose objects at its fastest level: landing
            // latency matters more than disk space for write-once objects.
            let mut encoder = ZlibEncoder::new(file, Compression::fast());
            encoder.write_all(format!("{} {}\0", kind.as_str(), data.len()).as_bytes())?;
            encoder.write_all(data)?;
            encoder.finish()?.sync_all()
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        Ok(temp)
    }
}

/// Creates a fresh temporary file in `fan_out`. A crashed process can leave
/// `tmp_obj_<pid>_<n>` behind, and Windows reuses PIDs, so a name that already
/// exists is skipped rather than failing the put.
fn create_temp(fan_out: &Path) -> io::Result<(PathBuf, File)> {
    const MAX_ATTEMPTS: u32 = 1024;
    let mut last_error = None;
    for _ in 0..MAX_ATTEMPTS {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = fan_out.join(format!("tmp_obj_{}_{sequence}", std::process::id()));
        let created = match File::create_new(&temp) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(fan_out)?;
                File::create_new(&temp)
            }
            other => other,
        };
        match created {
            Ok(file) => return Ok((temp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_error.expect("at least one attempt was made"))
}

/// Inflates a loose object file and checks it against the ID it is stored under.
fn decode_loose(id: &ObjectId, compressed: &[u8]) -> Result<RawObject, StoreError> {
    let corrupt = |reason: String| StoreError::Corrupt { id: *id, reason };
    let mut inflated = Vec::new();
    ZlibDecoder::new(compressed)
        .read_to_end(&mut inflated)
        .map_err(|error| corrupt(format!("zlib: {error}")))?;
    let nul = inflated
        .iter()
        .position(|&byte| byte == 0)
        .ok_or_else(|| corrupt("missing header".into()))?;
    let header = std::str::from_utf8(&inflated[..nul]).map_err(|_| corrupt("bad header".into()))?;
    let (kind, len) = header
        .split_once(' ')
        .ok_or_else(|| corrupt("bad header".into()))?;
    let kind = ObjectKind::parse(kind.as_bytes()).map_err(|error| corrupt(error.to_string()))?;
    let len: usize = len.parse().map_err(|_| corrupt("bad length".into()))?;
    let data = inflated.split_off(nul + 1);
    if data.len() != len {
        return Err(corrupt(format!(
            "holds {} bytes, header says {len}",
            data.len()
        )));
    }
    // Verifying the hash on every read turns silent disk corruption into an
    // error instead of a wrong file landing in someone's tree.
    if ObjectId::hash_object(kind.as_str(), &data) != *id {
        return Err(corrupt("content does not hash to its id".into()));
    }
    Ok(RawObject { kind, data })
}

impl ObjectStore for LooseObjectStore {
    fn put_raw(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId, StoreError> {
        let id = ObjectId::hash_object(kind.as_str(), data);
        let path = self.object_path(&id);
        // Objects are immutable and content-addressed: an existing file
        // already holds exactly these bytes.
        if path.is_file() {
            return Ok(id);
        }
        let fan_out = path.parent().expect("object path has a fan-out directory");
        let temp = self.write_temp(fan_out, kind, data)?;
        // move_atomic refuses to replace an existing file and makes the new
        // directory entry durable (write-through on Windows, a directory fsync
        // elsewhere). Without that, a crash just after a land could leave the
        // base ref naming a commit whose file never reached the disk.
        if let Err(error) = atomicwrites::move_atomic(&temp, &path) {
            let _ = fs::remove_file(&temp);
            // A concurrent writer of the same object got there first; its
            // file holds identical bytes.
            if !path.is_file() {
                return Err(error.into());
            }
        }
        Ok(id)
    }

    fn get_raw(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError> {
        match fs::read(self.object_path(id)) {
            Ok(compressed) => decode_loose(id, &compressed).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, StoreError> {
        match fs::metadata(self.object_path(id)) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Inflates only the `<kind> <len>\0` header, so sizing a large blob
    /// costs one small read instead of reading and hashing all of it.
    fn object_size(&self, id: &ObjectId) -> Result<u64, StoreError> {
        let file = match File::open(self.object_path(id)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(*id))
            }
            Err(error) => return Err(error.into()),
        };
        let compressed = file.metadata()?.len();
        let len = read_header_len(id, ZlibDecoder::new(file))?;
        // Callers reserve disk for this length before any validating read,
        // so a corrupt header claiming more than the file could inflate to
        // must fail here rather than wait forever for room.
        if len > compressed.saturating_mul(MAX_DEFLATE_RATIO) {
            return Err(StoreError::Corrupt {
                id: *id,
                reason: format!("header claims {len} bytes from {compressed} compressed"),
            });
        }
        Ok(len)
    }
}

/// Deflate expands at most about 1032 to 1, so no honest object inflates
/// to more than this many times its compressed size.
const MAX_DEFLATE_RATIO: u64 = 1032;

/// The longest header is a kind name, a space and a 20-digit u64 length.
const MAX_HEADER: usize = 32;

fn read_header_len(id: &ObjectId, mut inflated: impl Read) -> Result<u64, StoreError> {
    let corrupt = |reason: String| StoreError::Corrupt { id: *id, reason };
    let mut header = Vec::with_capacity(MAX_HEADER);
    let mut byte = [0u8; 1];
    loop {
        let read = inflated
            .read(&mut byte)
            .map_err(|error| corrupt(format!("zlib: {error}")))?;
        if read == 0 || header.len() == MAX_HEADER {
            return Err(corrupt("missing header".into()));
        }
        if byte[0] == 0 {
            break;
        }
        header.push(byte[0]);
    }
    let header = std::str::from_utf8(&header).map_err(|_| corrupt("bad header".into()))?;
    let (kind, len) = header
        .split_once(' ')
        .ok_or_else(|| corrupt("bad header".into()))?;
    ObjectKind::parse(kind.as_bytes()).map_err(|error| corrupt(error.to_string()))?;
    len.parse().map_err(|_| corrupt("bad length".into()))
}
