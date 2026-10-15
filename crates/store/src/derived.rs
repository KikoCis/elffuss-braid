// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The derived index: where the records of derived values are, by key
//! (format spec, section 7).
//!
//! A derived value is stored as a derived object whose payload starts with
//! its key, and this index maps keys to the ids of those objects. It is a
//! cache: losing it, or part of it, only means computing values again, and
//! whoever reads an entry checks the record it names. So it needs no sync of
//! its own; but its writer makes the records durable first, so an entry never
//! outlives the record it names.
//!
//! ```text
//! derived/index   append-only, 72-byte entries at multiples of 72:
//!                 [32] key | [32] record id | u32 0 | u32 crc32c of the 68 bytes before it
//! ```
//!
//! A writer takes an exclusive lock on the file, appends its entries at the
//! first multiple of 72 at or after the end, and lets go: entries of
//! different writers never interleave, and the remains of an append cut short
//! by a crash fail their CRC and are skipped. Readers take no lock. A handle
//! reads the whole file when first asked, and on a miss reads what was
//! appended since. When a key has several entries, the last one counts.
//! Entries are never removed; a compacted form can come with the store's own
//! indexes.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use braid_format::ObjectId;

use crate::error::{Error, io_error};
use crate::fsutil::{read_range, write_all_at};

/// The directory of the derived index, inside the store.
pub const DERIVED_DIR: &str = "derived";

/// The file of entries, inside [`DERIVED_DIR`].
const INDEX_FILE: &str = "index";

/// Bytes per entry.
const ENTRY_LEN: u64 = 72;

/// The derived index of a store, opened by this process.
///
/// Creating a handle does no I/O: the file is read when it is first needed.
/// A handle is `Send` and `Sync`.
#[derive(Debug)]
pub struct DerivedIndexFile {
    path: PathBuf,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// What the entries read so far say, the last entry of a key winning.
    map: HashMap<[u8; 32], ObjectId>,
    /// The file, once it exists.
    file: Option<File>,
    /// Where reading resumes: the end of the last whole slot read.
    read_to: u64,
}

impl DerivedIndexFile {
    /// The derived index of the store at `store_root`.
    pub fn new(store_root: impl AsRef<Path>) -> Self {
        DerivedIndexFile {
            path: store_root.as_ref().join(DERIVED_DIR).join(INDEX_FILE),
            state: Mutex::new(State::default()),
        }
    }

    /// The id of the record of `key`, if an entry names one. A miss reads
    /// what other processes appended since this handle last looked.
    pub fn find(&self, key: &[u8; 32]) -> Result<Option<ObjectId>, Error> {
        let mut state = self.lock();
        if let Some(id) = state.map.get(key) {
            return Ok(Some(*id));
        }
        self.refresh(&mut state)?;
        Ok(state.map.get(key).copied())
    }

    /// Appends entries, in order, after making sure nobody else is
    /// appending. The records they name must already be durable.
    pub fn append(&self, entries: &[([u8; 32], ObjectId)]) -> Result<(), Error> {
        if entries.is_empty() {
            return Ok(());
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir).map_err(io_error("create", dir))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .map_err(io_error("open", &self.path))?;
        file.lock().map_err(io_error("lock", &self.path))?;
        let mut bytes = Vec::with_capacity(entries.len() * ENTRY_LEN as usize);
        for (key, id) in entries {
            bytes.extend_from_slice(&encode(key, id));
        }
        let end = file
            .metadata()
            .map_err(io_error("read the size of", &self.path))?
            .len();
        let at = end.next_multiple_of(ENTRY_LEN);
        write_all_at(&file, &bytes, at).map_err(io_error("append to", &self.path))?;
        // Closing the file lets the next writer in.
        drop(file);
        let mut state = self.lock();
        for (key, id) in entries {
            state.map.insert(*key, *id);
        }
        Ok(())
    }

    /// Reads the entries appended since the last read.
    fn refresh(&self, state: &mut State) -> Result<(), Error> {
        if state.file.is_none() {
            match File::open(&self.path) {
                Ok(file) => state.file = Some(file),
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(io_error("open", &self.path)(err)),
            }
        }
        let Some(file) = &state.file else {
            return Ok(());
        };
        let len = file
            .metadata()
            .map_err(io_error("read the size of", &self.path))?
            .len();
        // Only whole slots: the last one may still be being written.
        let end = len - len % ENTRY_LEN;
        if end <= state.read_to {
            return Ok(());
        }
        let bytes = read_range(file, state.read_to, end - state.read_to)
            .map_err(io_error("read", &self.path))?;
        for slot in bytes.chunks_exact(ENTRY_LEN as usize) {
            if let Some((key, id)) = decode(slot) {
                state.map.insert(key, id);
            }
        }
        state.read_to = end;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every change to the state is complete before it is visible.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The bytes of one entry.
fn encode(key: &[u8; 32], id: &ObjectId) -> [u8; ENTRY_LEN as usize] {
    let mut out = [0u8; ENTRY_LEN as usize];
    out[0..32].copy_from_slice(key);
    out[32..64].copy_from_slice(id.as_bytes());
    // out[64..68] stays zero.
    let crc = crc32c::crc32c(&out[..68]);
    out[68..72].copy_from_slice(&crc.to_le_bytes());
    out
}

/// The key and record id of an entry, or `None` if it fails its checks.
fn decode(slot: &[u8]) -> Option<([u8; 32], ObjectId)> {
    if slot.len() != ENTRY_LEN as usize || slot[64..68] != [0; 4] {
        return None;
    }
    let mut crc = [0u8; 4];
    crc.copy_from_slice(&slot[68..72]);
    if crc32c::crc32c(&slot[..68]) != u32::from_le_bytes(crc) {
        return None;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&slot[..32]);
    let mut id = [0u8; 32];
    id.copy_from_slice(&slot[32..64]);
    Some((key, ObjectId::from_bytes(id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::Kind;

    fn key(n: u32) -> [u8; 32] {
        *ObjectId::hash(Kind::Derived, &n.to_le_bytes()).as_bytes()
    }

    fn id(n: u32) -> ObjectId {
        ObjectId::hash(Kind::Blob, &n.to_le_bytes())
    }

    #[test]
    fn entries_round_trip_and_damage_is_caught() {
        let bytes = encode(&key(1), &id(2));
        assert_eq!(decode(&bytes), Some((key(1), id(2))));
        for i in 0..bytes.len() {
            let mut damaged = bytes;
            damaged[i] ^= 0x10;
            assert_eq!(decode(&damaged), None, "byte {i}");
        }
        assert_eq!(decode(&[0; 72]), None);
        assert_eq!(decode(&bytes[..71]), None);
    }

    #[test]
    fn appends_are_seen_by_other_handles() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DerivedIndexFile::new(dir.path());
        let reader = DerivedIndexFile::new(dir.path());
        // No file yet: a miss, and nothing created.
        assert_eq!(reader.find(&key(1)).unwrap(), None);
        assert!(!dir.path().join(DERIVED_DIR).exists());

        writer.append(&[(key(1), id(1)), (key(2), id(2))]).unwrap();
        assert_eq!(writer.find(&key(1)).unwrap(), Some(id(1)));
        assert_eq!(reader.find(&key(2)).unwrap(), Some(id(2)));
        writer.append(&[(key(3), id(3)), (key(1), id(4))]).unwrap();
        assert_eq!(reader.find(&key(3)).unwrap(), Some(id(3)));
        // The last entry of a key counts, once read.
        assert_eq!(reader.find(&key(1)).unwrap(), Some(id(4)));
        writer.append(&[]).unwrap();

        let fresh = DerivedIndexFile::new(dir.path());
        assert_eq!(fresh.find(&key(1)).unwrap(), Some(id(4)));
        assert_eq!(fresh.find(&key(9)).unwrap(), None);
    }

    #[test]
    fn torn_appends_are_skipped_and_written_after() {
        let dir = tempfile::tempdir().unwrap();
        let index = DerivedIndexFile::new(dir.path());
        index.append(&[(key(1), id(1)), (key(2), id(2))]).unwrap();
        let path = dir.path().join(DERIVED_DIR).join(INDEX_FILE);
        // A crash in the middle of the next append left half an entry, and
        // damaged the second one.
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        write_all_at(&file, &encode(&key(3), &id(3))[..40], 2 * ENTRY_LEN).unwrap();
        write_all_at(&file, &[0xff], ENTRY_LEN + 3).unwrap();
        drop(file);

        index.append(&[(key(4), id(4))]).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 4 * ENTRY_LEN);
        let fresh = DerivedIndexFile::new(dir.path());
        assert_eq!(fresh.find(&key(1)).unwrap(), Some(id(1)));
        assert_eq!(fresh.find(&key(2)).unwrap(), None);
        assert_eq!(fresh.find(&key(3)).unwrap(), None);
        assert_eq!(fresh.find(&key(4)).unwrap(), Some(id(4)));
    }

    #[test]
    fn concurrent_writers_never_mix_their_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // Separate handles open the file separately, so their locks exclude
        // each other as those of separate processes do.
        let threads: Vec<_> = (0..8u32)
            .map(|t| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let index = DerivedIndexFile::new(&root);
                    for round in 0..20u32 {
                        let entries: Vec<_> = (0..5u32)
                            .map(|i| {
                                let n = t * 1000 + round * 10 + i;
                                (key(n), id(n))
                            })
                            .collect();
                        index.append(&entries).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let path = root.join(DERIVED_DIR).join(INDEX_FILE);
        assert_eq!(fs::metadata(&path).unwrap().len(), 8 * 20 * 5 * ENTRY_LEN);
        let index = DerivedIndexFile::new(&root);
        for t in 0..8u32 {
            for round in 0..20u32 {
                for i in 0..5u32 {
                    let n = t * 1000 + round * 10 + i;
                    assert_eq!(index.find(&key(n)).unwrap(), Some(id(n)));
                }
            }
        }
    }
}
