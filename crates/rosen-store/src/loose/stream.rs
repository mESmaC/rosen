//! Streaming reads and writes of loose objects (issue #250). A blob can be
//! larger than memory allows to hold at once, so these move it in pieces and
//! hash it on the way, keeping the same checks as `get_raw` and `put_raw`.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rosen_core::{ObjectHasher, ObjectId, ObjectKind};

use super::{create_temp, read_header, Existing, LooseObjectStore};
use crate::backend::{RawObject, StoreError};

/// The most body bytes held at once while streaming.
const PIECE: usize = 64 * 1024;

/// What [`LooseObjectStore::get_raw_within`] found.
#[derive(Debug)]
pub enum ReadWithin {
    Missing,
    /// The body's length, from the header alone; the body was not read.
    TooLarge(u64),
    Read(RawObject),
}

impl LooseObjectStore {
    /// Reads `id` only if its body is at most `max` bytes. The header gives
    /// the length within the same open, so sorting small objects from large
    /// ones never inflates a large one.
    pub fn get_raw_within(&self, id: &ObjectId, max: u64) -> Result<ReadWithin, StoreError> {
        let Some(file) = self.open_object(&self.object_path(id))? else {
            return Ok(ReadWithin::Missing);
        };
        let mut inflated = ZlibDecoder::new(file);
        let (kind, len) = read_header(id, &mut inflated)?;
        if len > max {
            return Ok(ReadWithin::TooLarge(len));
        }
        let mut data = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        copy_checked(id, kind, len, &mut inflated, &mut data)?;
        Ok(ReadWithin::Read(RawObject { kind, data }))
    }

    /// Writes the body of `id` to `out` piece by piece and returns its kind
    /// and length, or `None` when the store does not hold it. The body is
    /// checked against `id` as it passes, so on an error `out` may hold a
    /// partial or damaged body and must be discarded.
    pub fn copy_raw_to(
        &self,
        id: &ObjectId,
        out: &mut dyn Write,
    ) -> Result<Option<(ObjectKind, u64)>, StoreError> {
        let Some(file) = self.open_object(&self.object_path(id))? else {
            return Ok(None);
        };
        copy_body(id, file, out).map(Some)
    }

    /// Starts storing an object of `kind`, `len` bytes long, that the
    /// caller expects to hash to `id`. Write the body to the returned
    /// [`PendingObject`], then [`finish`](PendingObject::finish) it.
    pub fn begin_put(
        &self,
        kind: ObjectKind,
        id: ObjectId,
        len: u64,
    ) -> Result<PendingObject, StoreError> {
        let path = self.object_path(&id);
        let fan_out = path.parent().expect("object path has a fan-out directory");
        let (temp, file) = create_temp(fan_out)?;
        let mut pending = PendingObject {
            store: self.clone(),
            id,
            temp: Some(temp),
            // Git's level, as for every other put.
            encoder: Some(ZlibEncoder::new(file, Compression::fast())),
            hasher: ObjectHasher::new(kind.as_str(), len),
        };
        // The header goes to the file but not the hasher, which has its own.
        let header = format!("{} {len}\0", kind.as_str());
        pending.encoder_mut()?.write_all(header.as_bytes())?;
        Ok(pending)
    }

    /// Stores the `len`-byte body read from `body` as a `kind` object if it
    /// hashes to `id`, without holding it in memory. Answers whether the
    /// store now holds `id`: false means the bytes were some other object,
    /// and nothing was stored.
    pub fn put_raw_from(
        &self,
        kind: ObjectKind,
        id: ObjectId,
        len: u64,
        body: &mut dyn Read,
    ) -> Result<bool, StoreError> {
        let mut pending = self.begin_put(kind, id, len)?;
        io::copy(body, &mut pending)?;
        pending.finish()
    }
}

/// An object being written in pieces. Readers see nothing of it until
/// [`finish`](Self::finish) finds the pieces hash to the promised ID, and
/// one dropped unfinished leaves no temporary file behind.
pub struct PendingObject {
    store: LooseObjectStore,
    id: ObjectId,
    /// The temporary file, until it is published.
    temp: Option<PathBuf>,
    /// Taken when the object is finished.
    encoder: Option<ZlibEncoder<File>>,
    hasher: ObjectHasher,
}

impl PendingObject {
    fn encoder_mut(&mut self) -> io::Result<&mut ZlibEncoder<File>> {
        self.encoder
            .as_mut()
            .ok_or_else(|| io::Error::other("the object was already finished"))
    }

    /// Body bytes written so far.
    pub fn written(&self) -> u64 {
        self.hasher.fed()
    }

    /// Publishes the object if its pieces hash to the promised ID, durably
    /// as `put_raw` does, and answers whether the store now holds it.
    pub fn finish(mut self) -> Result<bool, StoreError> {
        let encoder = self.encoder.take().expect("finish runs once");
        if self.hasher.clone().finish() != Some(self.id) {
            return Ok(false);
        }
        encoder.finish()?.sync_all()?;
        let path = self.store.object_path(&self.id);
        let existing = self.store.existing_state(&self.id, &path)?;
        if existing == Existing::Valid {
            return Ok(true);
        }
        let temp = self
            .temp
            .take()
            .expect("an unpublished object has a temp file");
        self.store.publish(&self.id, existing, &temp, &path)?;
        Ok(true)
    }
}

impl Write for PendingObject {
    fn write(&mut self, piece: &[u8]) -> io::Result<usize> {
        let written = self.encoder_mut()?.write(piece)?;
        self.hasher.update(&piece[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.encoder_mut()?.flush()
    }
}

impl Drop for PendingObject {
    fn drop(&mut self) {
        // Closes the file first, so its removal is not refused on Windows.
        drop(self.encoder.take());
        if let Some(temp) = self.temp.take() {
            let _ = fs::remove_file(temp);
        }
    }
}

/// Inflates a loose object file's body into `out`, checking it against
/// `id` as `get_raw` does, and returns its kind and length.
pub(super) fn copy_body(
    id: &ObjectId,
    file: File,
    out: &mut dyn Write,
) -> Result<(ObjectKind, u64), StoreError> {
    let mut inflated = ZlibDecoder::new(file);
    let (kind, len) = read_header(id, &mut inflated)?;
    copy_checked(id, kind, len, &mut inflated, out)?;
    Ok((kind, len))
}

/// Copies an inflated body to `out`, failing as corrupt when it is longer
/// or shorter than its header says or does not hash to `id`. A body longer
/// than its header stops at the first excess piece, so `out` never receives
/// much more than `len` bytes.
fn copy_checked(
    id: &ObjectId,
    kind: ObjectKind,
    len: u64,
    inflated: &mut impl Read,
    out: &mut dyn Write,
) -> Result<(), StoreError> {
    let corrupt = |reason: String| StoreError::Corrupt { id: *id, reason };
    let mut hasher = ObjectHasher::new(kind.as_str(), len);
    // One byte more than the body, so a small object's end is read at once.
    let size = usize::try_from(len.saturating_add(1)).map_or(PIECE, |len| len.min(PIECE));
    let mut piece = vec![0u8; size];
    loop {
        let read = match inflated.read(&mut piece) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(corrupt(format!("zlib: {error}"))),
        };
        hasher.update(&piece[..read]);
        if hasher.fed() > len {
            return Err(corrupt(format!(
                "holds more than the {len} bytes its header says"
            )));
        }
        out.write_all(&piece[..read])?;
    }
    let fed = hasher.fed();
    match hasher.finish() {
        None => Err(corrupt(format!("holds {fed} bytes, header says {len}"))),
        Some(found) if found != *id => Err(corrupt("content does not hash to its id".into())),
        Some(_) => Ok(()),
    }
}
