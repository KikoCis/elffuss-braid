// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Merging indexes, so that a store with many sealed segments needs few
//! index reads.
//!
//! Sealing gives each segment an index of its own. Once a writer has sealed
//! enough that `merge_threshold` segment indexes are there, its next sync,
//! or the end of its next transaction, merges them into a merged index,
//! `seg/<name>.midx`, under an exclusive, non-blocking lock on
//! `seg/merge.lock`, so that one process merges at a time while the others
//! go on. A merge:
//!
//! 1. lists the directory, and reads the heads of the merged indexes;
//! 2. takes every segment index that no merged index covers, then the
//!    merged indexes that hold at most twice as many entries as it took so
//!    far, smallest first, so that every merged index left holds more than
//!    twice what the new one does: an entry is written again a number of
//!    times that grows with the logarithm of the store's size, not with it;
//! 3. reads them whole, checking every entry; an index found damaged is
//!    made again from its segments' records;
//! 4. writes the merged index, one copy of each object, the best ranked and
//!    the first of equals, through a temporary file, a flush and a rename,
//!    then flushes the directory;
//! 5. removes the journals that sealers which died left next to the
//!    indexes it took, flushes the directory, and only then removes the
//!    indexes it took, with any that an earlier merge cut short left behind.
//!
//! A segment with a journal and no index of its own is open, and writers
//! take it over; merging removes a segment's journal before its index, and
//! makes the first durable before the second, so that a merged segment
//! never looks open, not even after a power loss.
//!
//! A merge cut short at any point leaves a store that reads the same: until
//! the new index is in place the old ones serve, and afterwards an index
//! left behind covers segments that the new one covers too. Readers prefer
//! the index that covers more, and the next merge removes the rest.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::sync::atomic::Ordering;

use super::sources::Source;
use super::{INDEX_STORAGE, Listing, Segments, list, segment_path};
use crate::error::{Error, io_error};
use crate::fsutil::{publish_file, random_name, remove_stale_temps, sync_dir};
use crate::index::{self, Covered, IndexError, MAX_COVERED, Numbered};
use crate::journal::{ENTRY_LEN, Entry};
use crate::{record, scan};

/// The lock a merge holds, in `seg/`.
const LOCK: &str = "merge.lock";

/// An index a merge can take.
#[derive(Debug)]
struct Input {
    /// Its file name in `seg/`.
    file: String,
    /// The segment it is the index of, or `None` for a merged index.
    own: Option<String>,
    /// The segments it covers, as far as known before reading it.
    covered: Vec<String>,
    /// How many entries it holds, about.
    entries: u64,
}

impl Segments {
    /// Merges indexes if sealing left enough of them since this handle last
    /// did. Returns whether it merged.
    pub fn merge_if_due(&self) -> Result<bool, Error> {
        if !self.merge_due.swap(false, Ordering::AcqRel) {
            return Ok(false);
        }
        self.merge(false)
    }

    /// Merges segment indexes into a merged index once at least
    /// `merge_threshold` of them are there, or, with `all`, every index
    /// into one. Returns whether it merged; `false` too when another process
    /// is merging. See the module documentation.
    pub fn merge(&self, all: bool) -> Result<bool, Error> {
        let lock_path = self.dir.join(LOCK);
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(io_error("create", &lock_path))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(err)) => return Err(io_error("lock", &lock_path)(err)),
        }
        let listing = list(&self.dir)?;
        let (inputs, leftovers) = self.inputs(&listing)?;
        let own = inputs.iter().filter(|input| input.own.is_some()).count();
        let threshold = self.merge_threshold.max(2);
        let taken = if all {
            take_all(inputs)
        } else if own >= threshold {
            take(inputs)
        } else {
            Vec::new()
        };
        let merged = taken.len() >= 2;
        let mut covered = Vec::new();
        if merged {
            self.need_storage(INDEX_STORAGE)?;
            let entries;
            (covered, entries) = self.read_inputs(&taken)?;
            remove_stale_temps(&self.dir);
            publish_file(
                &self.dir,
                &format!("{}.midx", random_name(&self.dir)?),
                &index::encode(&covered, &entries),
                self.durability,
                &[],
            )?;
            #[cfg(test)]
            failpoints::check("published")?;
        }
        // Journals first, durably: a segment with a journal and no index of
        // its own is open.
        let gone: Vec<&Input> = taken.iter().filter(|_| merged).chain(&leftovers).collect();
        let mut removed = false;
        let segments = covered
            .iter()
            .map(|segment| segment.name.as_str())
            .chain(gone.iter().filter_map(|input| input.own.as_deref()));
        for name in segments {
            removed |= remove(&self.path(name, "jrn"))?;
        }
        if removed {
            sync_dir(&self.dir, self.durability)?;
        }
        #[cfg(test)]
        failpoints::check("journals removed")?;
        for input in &gone {
            remove(&self.dir.join(&input.file))?;
        }
        drop(lock);
        if !gone.is_empty() {
            self.rescan()?;
        }
        Ok(merged)
    }

    /// The indexes a merge can take, and those an earlier merge left
    /// behind: segment indexes that a merged index covers, and merged
    /// indexes whose segments a larger one covers.
    fn inputs(&self, listing: &Listing) -> Result<(Vec<Input>, Vec<Input>), Error> {
        let mut merged = Vec::new();
        for name in &listing.merged {
            let file = format!("{name}.midx");
            // One that cannot be read is left alone: lookups scan its
            // segments, and fsck reports it.
            if let Some(Ok(source)) = Source::open_merged(&self.dir, &file)? {
                let covered: Vec<String> = source.covered.iter().map(|(n, _)| n.clone()).collect();
                let entries = file_len(&self.dir.join(&file))? / ENTRY_LEN;
                let key = (covered.len(), file.clone());
                merged.push((
                    key,
                    Input {
                        file,
                        own: None,
                        covered,
                        entries,
                    },
                ));
            }
        }
        // Each segment goes to the best merged index that covers it.
        let mut best: HashMap<&str, usize> = HashMap::new();
        for (at, (key, input)) in merged.iter().enumerate() {
            for segment in &input.covered {
                let better = best
                    .get(segment.as_str())
                    .is_none_or(|&other| merged[other].0 < *key);
                if better {
                    best.insert(segment.as_str(), at);
                }
            }
        }
        let mut leftovers = Vec::new();
        let mut keep = vec![false; merged.len()];
        for &at in best.values() {
            keep[at] = true;
        }
        let mut inputs = Vec::new();
        for (name, files) in &listing.segments {
            if !files.idx || !files.seg {
                continue;
            }
            let file = format!("{name}.idx");
            let entries = file_len(&self.dir.join(&file))? / ENTRY_LEN;
            let input = Input {
                file,
                own: Some(name.clone()),
                covered: vec![name.clone()],
                entries,
            };
            if best.contains_key(name.as_str()) {
                leftovers.push(input);
            } else {
                inputs.push(input);
            }
        }
        for ((_, input), keep) in merged.into_iter().zip(keep) {
            if keep {
                inputs.push(input);
            } else {
                leftovers.push(input);
            }
        }
        Ok((inputs, leftovers))
    }

    /// Reads the indexes a merge takes, and returns the segments they cover
    /// and their entries, one copy of each object.
    fn read_inputs(&self, taken: &[Input]) -> Result<(Vec<Covered>, Vec<Numbered>), Error> {
        let mut covered: BTreeMap<String, u64> = BTreeMap::new();
        let mut entries: Vec<(Entry, String, usize)> = Vec::new();
        for input in taken {
            let path = self.dir.join(&input.file);
            let bytes = fs::read(&path).map_err(io_error("read", &path))?;
            match index::decode(&bytes, input.own.as_deref()) {
                Ok(decoded) => {
                    for (entry, segment) in decoded.entries {
                        let name = decoded.covered[usize::from(segment)].name.clone();
                        entries.push((entry, name, entries.len()));
                    }
                    for segment in decoded.covered {
                        covered.insert(segment.name, segment.len);
                    }
                }
                Err(IndexError::Newer(detail)) => {
                    return Err(Error::NewerStore {
                        path,
                        detail: format!("it holds {detail}"),
                    });
                }
                // Damaged: the records describe themselves.
                Err(IndexError::Damaged(_)) => {
                    for name in &input.covered {
                        let len = self.scan_into(name, &mut entries)?;
                        covered.insert(name.clone(), len);
                    }
                }
            }
        }
        let numbers: HashMap<&str, u16> = covered
            .keys()
            .enumerate()
            .map(|(number, name)| (name.as_str(), number as u16))
            .collect();
        entries.sort_by(|(a, _, a_order), (b, _, b_order)| {
            (a.id, record::rank(a.codec), a_order).cmp(&(b.id, record::rank(b.codec), b_order))
        });
        entries.dedup_by_key(|(entry, _, _)| entry.id);
        let entries = entries
            .iter()
            .map(|(entry, name, _)| (*entry, numbers[name.as_str()]))
            .collect();
        let covered = covered
            .into_iter()
            .map(|(name, len)| Covered { name, len })
            .collect();
        Ok((covered, entries))
    }

    /// Adds the entries of a segment's records, found by scanning it, and
    /// returns its length.
    fn scan_into(
        &self,
        name: &str,
        entries: &mut Vec<(Entry, String, usize)>,
    ) -> Result<u64, Error> {
        let path = segment_path(&self.dir, name, "seg");
        let file = File::open(&path).map_err(io_error("open", &path))?;
        let len = file.metadata().map_err(io_error("read", &path))?.len();
        scan::scan(&file, len, |offset, found| {
            let entry = Entry::new(offset, &found.header, found.crc);
            entries.push((entry, name.to_owned(), entries.len()));
        })
        .map_err(io_error("read", &path))?;
        Ok(len)
    }
}

/// What a merge takes when enough segment indexes are there: all of them,
/// then the merged indexes no larger than twice what it took so far,
/// smallest first, as long as the segments they cover fit in one index.
fn take(inputs: Vec<Input>) -> Vec<Input> {
    let (own, mut merged): (Vec<Input>, Vec<Input>) =
        inputs.into_iter().partition(|input| input.own.is_some());
    let mut taken: Vec<Input> = own.into_iter().take(MAX_COVERED).collect();
    let mut entries: u64 = taken.iter().map(|input| input.entries).sum();
    let mut segments = taken.len();
    merged.sort_by_key(|input| input.entries);
    for input in merged {
        if input.entries > entries.saturating_mul(2) || segments + input.covered.len() > MAX_COVERED
        {
            break;
        }
        entries += input.entries;
        segments += input.covered.len();
        taken.push(input);
    }
    taken
}

/// Every index, as far as the segments fit in one.
fn take_all(inputs: Vec<Input>) -> Vec<Input> {
    let mut segments = 0;
    let mut taken = Vec::new();
    for input in inputs {
        if segments + input.covered.len() > MAX_COVERED {
            break;
        }
        segments += input.covered.len();
        taken.push(input);
    }
    taken
}

fn file_len(path: &std::path::Path) -> Result<u64, Error> {
    Ok(fs::metadata(path).map_err(io_error("read", path))?.len())
}

/// Removes a file, and says whether it was there.
fn remove(path: &std::path::Path) -> Result<bool, Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(io_error("remove", path)(err)),
    }
}

/// Points where a test can cut a merge short, as a crash would.
#[cfg(test)]
pub(crate) mod failpoints {
    use std::cell::RefCell;
    use std::io;

    use crate::error::Error;

    thread_local! {
        static ARMED: RefCell<Option<&'static str>> = const { RefCell::new(None) };
    }

    /// Makes merges on this thread stop at `point`, until disarmed.
    pub(crate) fn arm(point: Option<&'static str>) {
        ARMED.with(|armed| *armed.borrow_mut() = point);
    }

    pub(super) fn check(point: &'static str) -> Result<(), Error> {
        if ARMED.with(|armed| *armed.borrow() == Some(point)) {
            return Err(Error::Io {
                action: "continue past",
                path: point.into(),
                source: io::Error::other("a test cut the merge short here"),
            });
        }
        Ok(())
    }
}
