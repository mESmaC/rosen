//! A final content guard, independent of whether conflict rows remain open.
use crate::history::{trees, HistoryError, HistoryResult};
use rosen_core::{FileMode, ObjectId, ObjectKind};
use rosen_store::{LooseObjectStore, ObjectStore, StoreError};
use std::collections::HashMap;
use std::io::{self, Write};

pub(crate) fn paths(store: &LooseObjectStore, tree: ObjectId) -> HistoryResult<Vec<String>> {
    let mut pending = vec![(String::new(), tree)];
    let mut checked = HashMap::new();
    let mut paths = Vec::new();
    while let Some((prefix, tree)) = pending.pop() {
        for entry in store.read_tree(&tree)?.entries() {
            let path = format!("{prefix}{}", String::from_utf8_lossy(&entry.name));
            if entry.mode.is_tree() {
                pending.push((format!("{path}/"), entry.id));
            } else if entry.mode != FileMode::Submodule {
                let marked = match checked.get(&entry.id) {
                    Some(&marked) => marked,
                    None => {
                        let marked = blob_has_markers(store, entry.id)?;
                        checked.insert(entry.id, marked);
                        marked
                    }
                };
                if marked {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    Ok(paths)
}

pub(crate) fn path_has_markers(
    store: &LooseObjectStore,
    tree: ObjectId,
    path: &str,
) -> HistoryResult<bool> {
    match trees::file_at(store, tree, path)? {
        Some(entry) if entry.is_blob() => blob_has_markers(store, entry.id),
        _ => Ok(false),
    }
}

fn blob_has_markers(store: &LooseObjectStore, id: ObjectId) -> HistoryResult<bool> {
    // Stream even large files and long lines; a land must never need a
    // second in-memory copy of an uploaded binary or generated artifact.
    let mut scan = MarkerScan::default();
    let (kind, _) = store
        .copy_raw_to(&id, &mut scan)?
        .ok_or(StoreError::NotFound(id))?;
    if kind != ObjectKind::Blob {
        return Err(HistoryError::Store(StoreError::WrongKind {
            id,
            expected: ObjectKind::Blob,
            found: kind,
        }));
    }
    scan.end_line();
    Ok(scan.found)
}

#[derive(Default)]
struct MarkerScan {
    marker: Option<u8>,
    run: usize,
    tail: bool,
    carriage_return: bool,
    found: bool,
}

impl MarkerScan {
    fn end_line(&mut self) {
        self.found |= self.run >= 7 && (!self.tail || self.carriage_return);
        self.marker = None;
        self.run = 0;
        self.tail = false;
        self.carriage_return = false;
    }

    fn byte(&mut self, byte: u8) {
        if byte == b'\n' {
            self.end_line();
        } else if self.tail {
            self.carriage_return = false;
        } else if self.marker == Some(byte) {
            self.run = (self.run + 1).min(7);
        } else if self.marker.is_none() && matches!(byte, b'<' | b'|' | b'=' | b'>') {
            self.marker = Some(byte);
            self.run = 1;
        } else {
            self.found |=
                self.run >= 7 && self.marker != Some(b'=') && matches!(byte, b' ' | b'\t');
            self.carriage_return = self.run >= 7 && byte == b'\r';
            self.tail = true;
        }
    }
}

impl Write for MarkerScan {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for &byte in bytes {
            self.byte(byte);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
