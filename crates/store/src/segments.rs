// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Segments: the files objects live in, and how processes share them.
//!
//! - **Leases.** A writer appends only to a segment it holds an exclusive,
//!   non-blocking lock on. It reuses an unlocked segment that is not sealed,
//!   or creates a new one with an exclusive create. The kernel releases the
//!   lock when the writer exits or dies, and the next writer resumes after
//!   the last record the journal describes. Nobody ever truncates a segment:
//!   other processes may be reading it.
//! - **Journals.** After each record the lease holder appends a journal
//!   entry, so readers learn about the record without reading payloads.
//! - **Sealing.** Once a segment passes the size threshold, or holds the
//!   most records a segment may, its holder writes its sorted index through
//!   a temporary file, flushed with the segment, and a rename, flushes the
//!   directory, and removes the journal. A sealed segment is never written
//!   again.
//! - **Merging.** Once enough segment indexes pile up, a writer merges them
//!   into a merged index (see the `merge` submodule), so that lookups go
//!   through few indexes.
//! - **Lookups.** Every handle keeps a map from id to location for the open
//!   segments only, built from their journals, which the record limit keeps
//!   short. Sealed segments are looked up in their indexes, which a handle
//!   reads as lookups need them (see the `sources` submodule): opening a
//!   store costs the same whatever it holds. On a miss a handle reads the
//!   directory again once, since another process may have written, sealed
//!   or merged something since. When it knows several copies of one object,
//!   a lookup takes the first that serves: a delta's base must be stored
//!   whole, and a chunk must not be chunked itself.
//! - **Durability.** [`Segments::put`] appends without flushing;
//!   [`Segments::sync`] flushes everything this handle wrote, segments and
//!   journals, in one batch. A copy written by someone else stands for a new
//!   write only together with what it needs to be read: a delta's base, a
//!   chunked blob's chunks. Those that sit in another writer's open segment
//!   are flushed by this handle's next sync too.
//!
//! How objects are stored whole, as deltas or in chunks, and read back, is
//! in the `objects` submodule.

mod merge;
mod objects;
mod sources;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap, hash_map};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};

use braid_format::{Kind, ObjectId};

use crate::error::{Error, io_error};
use crate::fsutil::{
    flush, is_random_name, open_dir, publish_file, random_name, read_range, remove_stale_temps,
    sync_dir, write_all_at,
};
use crate::index::{self, Covered};
use crate::journal::{self, ENTRY_LEN, Entry};
use crate::prefix::HexPrefix;
use crate::record::{self, CODEC_CHUNKED, CODEC_DELTA, CODEC_DICTIONARY, Encoded, Header};
use crate::scan::{self, Window};
use crate::{Durability, Options};
use crate::{chunks, format_file};

pub(crate) use objects::BaseCache;
use sources::{Lookup, Source, SourceKind};

/// Segments a handle keeps open for reading. Past this, a read opens the
/// segment for itself.
const MAX_CACHED_READERS: usize = 256;

/// How many times a rescan lists the directory again while segments are
/// sealed and merged under it.
const RESCAN_PASSES: usize = 3;

/// The storage version a store needs once it holds an index of version 2.
pub(crate) const INDEX_STORAGE: u32 = 3;

/// The worst copy a lookup takes, by [`record::rank`]: any this version
/// can read.
pub(crate) const ANY: u8 = 2;
/// A copy that is not chunked: stored whole or as a delta.
pub(crate) const NOT_CHUNKED: u8 = 1;
/// A copy stored whole, as a delta's base must be.
pub(crate) const WHOLE: u8 = 0;

/// The object half of a store: the `seg/` directory.
#[derive(Debug)]
pub(crate) struct Segments {
    /// The store's directory, where the `format` file is.
    root: PathBuf,
    dir: PathBuf,
    segment_size: u64,
    segment_records: u64,
    merge_threshold: usize,
    durability: Durability,
    catalog: RwLock<Catalog>,
    /// Held while rescanning the directory, so that one thread does it at a time.
    rescanning: Mutex<()>,
    writer: Mutex<Writer>,
    /// Segments with a cached reader.
    cached_readers: AtomicUsize,
    /// Payloads of delta bases read recently.
    bases: BaseCache,
    /// The storage version the `format` file is known to have at least.
    storage: AtomicU32,
    /// Set when enough segment indexes have piled up to merge them.
    merge_due: AtomicBool,
}

/// What this handle knows about the segments and the objects in them.
#[derive(Debug)]
struct Catalog {
    segments: Vec<Segment>,
    by_name: HashMap<String, u32>,
    /// The best ranked copy of each object in the open segments, from their
    /// journals and this handle's own appends.
    open: HashMap<ObjectId, Location>,
    /// Every index this handle has registered, by number.
    sources: Vec<Arc<Source>>,
    /// Their numbers, by file name.
    by_file: HashMap<String, u32>,
    /// The indexes lookups go through, in the order they try them.
    active: Arc<[Active]>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            segments: Vec::new(),
            by_name: HashMap::new(),
            open: HashMap::new(),
            sources: Vec::new(),
            by_file: HashMap::new(),
            active: Arc::from(Vec::new()),
        }
    }
}

/// An index lookups go through, and which of its segments it serves: each
/// sealed segment is served by one index, the best that covers it.
#[derive(Debug)]
struct Active {
    source: Arc<Source>,
    serves: Box<[bool]>,
}

#[derive(Debug)]
struct Segment {
    name: String,
    state: SegmentState,
    /// Opened on the first read, while fewer than [`MAX_CACHED_READERS`] are.
    reader: OnceLock<Arc<File>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SegmentState {
    /// Still open: its journal has been read up to this offset.
    Open { journal_end: u64 },
    /// Sealed, and looked up in this index. It will not change again.
    Sealed { source: u32 },
}

/// Where a copy of an object is: its segment, and its journal or index
/// entry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Location {
    /// The segment's number in this handle's catalog.
    pub segment: u32,
    /// What the record holds and where it starts.
    pub entry: Entry,
}

/// Which existing copies of an object can stand for a new write of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wanted {
    /// Any copy this version can read: whole, a delta, or chunks.
    Any,
    /// A copy that is not chunked, as a chunk list needs.
    NotChunked,
}

impl Wanted {
    /// The worst rank of a copy it takes.
    fn rank(self) -> u8 {
        match self {
            Wanted::Any => ANY,
            Wanted::NotChunked => NOT_CHUNKED,
        }
    }
}

/// A record read from its segment: its checksum is valid, and it is the one
/// its entry describes.
#[derive(Debug)]
pub(crate) struct Record {
    bytes: Vec<u8>,
    path: PathBuf,
    offset: u64,
    /// Its fixed fields.
    pub header: Header,
    /// The base it names, for a delta.
    pub reference: Option<ObjectId>,
}

impl Record {
    /// The payload as stored.
    pub fn stored(&self) -> &[u8] {
        let start = record::header_len(self.header.codec);
        &self.bytes[start..start + self.header.stored_len as usize]
    }

    /// The error for a problem with this record.
    pub fn corrupt(&self, detail: impl fmt::Display) -> Error {
        Error::corrupt(
            &self.path,
            self.offset,
            format!("object {}: {detail}", self.header.id),
        )
    }
}

/// The write side of this handle.
#[derive(Debug, Default)]
struct Writer {
    /// The segment this handle appends to.
    lease: Option<Lease>,
    /// Segments this handle wrote to and gave up before flushing them.
    unsynced: Vec<(String, Arc<File>, Arc<File>)>,
    /// Open segments of other writers that hold objects this handle relied on
    /// instead of writing them again. Their writers may not have flushed them
    /// yet, so the next sync does.
    foreign: BTreeSet<u32>,
}

impl Writer {
    /// Gives the lease up: unlocks the segment for other writers, and keeps
    /// its files open only to flush them at the next sync.
    fn retire(&mut self) {
        if let Some(lease) = self.lease.take() {
            self.keep_unsynced(lease);
        }
    }

    fn keep_unsynced(&mut self, lease: Lease) {
        let _ = lease.seg.unlock();
        self.unsynced.push((lease.name, lease.seg, lease.journal));
    }
}

/// An open segment this handle holds the lock on.
#[derive(Debug)]
struct Lease {
    segment: u32,
    name: String,
    /// Locked for as long as the lease lasts.
    seg: Arc<File>,
    journal: Arc<File>,
    /// Where the next record goes.
    seg_end: u64,
    /// Where the next journal entry goes.
    journal_end: u64,
    /// Records its journal lists.
    records: u64,
    /// Whether something was written since the last sync.
    dirty: bool,
}

/// Which of a segment's files exist.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Files {
    pub seg: bool,
    pub jrn: bool,
    pub idx: bool,
}

/// What `seg/` holds.
#[derive(Debug, Default)]
pub(crate) struct Listing {
    /// Segments by name, and which of their files exist.
    pub segments: BTreeMap<String, Files>,
    /// The names of merged indexes.
    pub merged: BTreeSet<String>,
}

impl Segments {
    /// Opens the segments of the store at `root`, whose `format` file says
    /// it has storage version `storage`, and reads the journals of the open
    /// segments; indexes are read as lookups need them. If the journals
    /// already hold records that need a later storage version, as a writer
    /// from before storage versions could leave, the version is raised.
    pub fn open(root: &Path, options: &Options, storage: u32) -> Result<Self, Error> {
        let segments = Self {
            root: root.to_path_buf(),
            dir: root.join("seg"),
            segment_size: options.segment_size,
            segment_records: options.segment_records,
            merge_threshold: options.merge_threshold,
            durability: options.durability,
            catalog: RwLock::new(Catalog::default()),
            rescanning: Mutex::new(()),
            writer: Mutex::new(Writer::default()),
            cached_readers: AtomicUsize::new(0),
            bases: BaseCache::default(),
            storage: AtomicU32::new(storage),
            merge_due: AtomicBool::new(false),
        };
        segments.rescan()?;
        let needed = segments
            .read_catalog()
            .open
            .values()
            .map(|location| record::storage_version(location.entry.codec))
            .max()
            .unwrap_or(1);
        segments.need_storage(needed)?;
        Ok(segments)
    }

    /// Makes sure the store's `format` file says it has at least storage
    /// version `version`, raising it if needed, before anything that needs
    /// that version is written.
    pub(crate) fn need_storage(&self, version: u32) -> Result<(), Error> {
        if self.storage.load(Ordering::Acquire) >= version {
            return Ok(());
        }
        format_file::raise(&self.root, version, self.durability)?;
        self.storage.fetch_max(version, Ordering::AcqRel);
        Ok(())
    }

    /// The `seg/` directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The path of one of a segment's files.
    pub fn path(&self, name: &str, extension: &str) -> PathBuf {
        segment_path(&self.dir, name, extension)
    }

    /// Appends the record `make` encodes for the object `id`, unless a copy
    /// that `wanted` accepts can stand for it. `make` runs at most once,
    /// outside the writer lock unless a copy appeared and turned out
    /// unusable. Nothing is flushed: call [`Segments::sync`] before anything
    /// durable refers to the object.
    pub fn store_record(
        &self,
        id: &ObjectId,
        wanted: Wanted,
        make: impl FnOnce() -> Result<Encoded, Error>,
    ) -> Result<(), Error> {
        let mut make = Some(make);
        // Encode outside the writer lock, unless a copy is already known.
        let mut encoded = None;
        if self.locate(id, wanted.rank())?.is_none()
            && let Some(make) = make.take()
        {
            encoded = Some(make()?);
        }
        let mut writer = self.lock_writer();
        if let Some(location) = self.locate(id, wanted.rank())?
            && self.reuse(&mut writer, &location, wanted)?
        {
            return Ok(());
        }
        let encoded = match (encoded, make) {
            (Some(encoded), _) => encoded,
            (None, Some(make)) => make()?,
            (None, None) => unreachable!("make runs only when nothing was encoded"),
        };
        self.append(&mut writer, encoded)
    }

    /// Whether a copy of `id` that `wanted` accepts can stand for a new
    /// write of it. Like a write that finds it, this makes the next sync
    /// flush the copy and what it needs, wherever another writer has not
    /// flushed them yet.
    pub fn stored(&self, id: &ObjectId, wanted: Wanted) -> Result<bool, Error> {
        let Some(location) = self.locate(id, wanted.rank())? else {
            return Ok(false);
        };
        let mut writer = self.lock_writer();
        self.reuse(&mut writer, &location, wanted)
    }

    /// Reads and checks the record at `location`.
    pub fn read_record(&self, location: &Location) -> Result<Record, Error> {
        let (file, path) = self.reader(location.segment)?;
        let entry = &location.entry;
        let corrupt = |detail: &str| {
            Error::corrupt(
                &path,
                entry.offset,
                format!("object {}: {detail}", entry.id),
            )
        };
        let header = entry.header();
        let bytes = match read_range(&file, entry.offset, header.unpadded_len()) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(corrupt("record runs past the end of the segment"));
            }
            Err(err) => return Err(io_error("read", &path)(err)),
        };
        let checked = record::check(&bytes).map_err(corrupt)?;
        if checked.header != header || checked.crc != entry.crc {
            return Err(corrupt("record does not match its entry"));
        }
        let reference = checked.reference;
        Ok(Record {
            bytes,
            path,
            offset: entry.offset,
            header,
            reference,
        })
    }

    /// The kind and size of an object, from its record's header alone: the
    /// payload is neither read nor decompressed. The header is checked
    /// against the object's index or journal entry, which has a checksum of
    /// its own, so a damaged header is an error; damage to the payload only
    /// shows on [`Segments::get`].
    pub fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, Error> {
        let Some(location) = self.find(id, ANY)? else {
            return Ok(None);
        };
        let (file, path) = self.reader(location.segment)?;
        let entry = &location.entry;
        let corrupt = |detail: &str| {
            Error::corrupt(
                &path,
                entry.offset,
                format!("object {}: {detail}", entry.id),
            )
        };
        let bytes = match read_range(&file, entry.offset, record::HEADER_LEN as u64) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(corrupt("record runs past the end of the segment"));
            }
            Err(err) => return Err(io_error("read", &path)(err)),
        };
        let header = record::parse_header(&bytes).map_err(corrupt)?;
        if header != entry.header() {
            return Err(corrupt("record does not match its entry"));
        }
        Ok(Some((header.kind, header.raw_len)))
    }

    /// Whether an index or a journal lists the object. The record is not read.
    pub fn contains(&self, id: &ObjectId) -> Result<bool, Error> {
        Ok(self.find(id, ANY)?.is_some())
    }

    /// The ids this handle knows of whose hex form starts with `prefix`,
    /// sorted. Only the buckets of the indexes the prefix falls in are
    /// read: see [`Segments::rescan`] for what is new.
    pub fn ids_with_prefix(&self, prefix: &HexPrefix) -> Result<Vec<ObjectId>, Error> {
        self.retry_gone(|| {
            let (mut found, active) = {
                let catalog = self.read_catalog();
                let found: BTreeSet<ObjectId> = catalog
                    .open
                    .keys()
                    .filter(|id| prefix.matches(id))
                    .copied()
                    .collect();
                (found, Arc::clone(&catalog.active))
            };
            for active in active.iter() {
                for (entry, segment) in active.source.with_prefix(prefix, self)? {
                    if active.serves[usize::from(segment)] {
                        found.insert(entry.id);
                    }
                }
            }
            Ok(found.into_iter().collect())
        })
    }

    /// Flushes everything this handle wrote: segments and journals, in one
    /// batch.
    pub fn sync(&self) -> Result<(), Error> {
        let mut writer = self.lock_writer();
        self.sync_locked(&mut writer)
    }

    /// Makes sure the objects in `ids` are durable before something durable
    /// refers to them: flushes this handle's writes, and the open segments of
    /// other writers that hold any of them or what they need to be read.
    /// Fails if a record, or a base or chunk it needs, is damaged or
    /// missing. Objects this handle does not know about are skipped.
    pub fn make_durable(&self, ids: &[ObjectId]) -> Result<(), Error> {
        let mut writer = self.lock_writer();
        for id in ids {
            let Some(location) = self.locate(id, ANY)? else {
                continue;
            };
            if !self.reuse(&mut writer, &location, Wanted::Any)? {
                let (_, path) = self.reader(location.segment)?;
                return Err(Error::corrupt(
                    &path,
                    location.entry.offset,
                    format!("object {id}: its record, or what it needs, is damaged or missing"),
                ));
            }
        }
        self.sync_locked(&mut writer)
    }

    /// Reads what changed in the directory since the last look: new
    /// segments, new journal entries, segments sealed since, and indexes
    /// merged since. Index entries are not read here: lookups read them.
    pub fn rescan(&self) -> Result<(), Error> {
        let _one_at_a_time = self
            .rescanning
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A listing may miss files that others create or remove while it is
        // read: a segment sealed and merged meanwhile is found by listing
        // again.
        for pass in 1..=RESCAN_PASSES {
            if self.rescan_once(pass == RESCAN_PASSES)? {
                break;
            }
        }
        self.note_own_indexes();
        Ok(())
    }

    /// One look at the directory. Returns `false` when a segment that holds
    /// records turned out to have neither a journal nor an index of its own,
    /// and no merged index listed covers it: sealed and merged since the
    /// listing, so that another listing finds it. On the `last` pass, such a
    /// segment is read by scanning it if a merged index cannot be read.
    fn rescan_once(&self, last: bool) -> Result<bool, Error> {
        let listing = list(&self.dir)?;

        // The heads of merged indexes this handle has not seen yet.
        let mut fresh = Vec::new();
        let mut unreadable = false;
        for name in &listing.merged {
            let file = format!("{name}.midx");
            if self.read_catalog().by_file.contains_key(&file) {
                continue;
            }
            match Source::open_merged(&self.dir, &file)? {
                Some(Ok(opened)) => fresh.push(opened),
                Some(Err(_damaged)) => unreadable = true,
                // Merged into another one since the listing.
                None => {}
            }
        }

        let mut follow = Vec::new();
        let mut settled = true;
        {
            let mut catalog = self.write_catalog();
            for opened in fresh {
                catalog.register(opened);
            }
            // Which merged index of those listed serves each segment.
            let mut best: HashMap<&str, u32> = HashMap::new();
            for name in &listing.merged {
                let Some(&number) = catalog.by_file.get(&format!("{name}.midx")) else {
                    continue;
                };
                let source = &catalog.sources[number as usize];
                for (segment, _) in &source.covered {
                    let better = best.get(segment.as_str()).is_none_or(|&other| {
                        catalog.sources[other as usize].rank() < source.rank()
                    });
                    if better {
                        best.insert(segment.as_str(), number);
                    }
                }
            }
            let best: HashMap<String, u32> = best
                .into_iter()
                .map(|(name, number)| (name.to_owned(), number))
                .collect();
            let mut changed = false;
            for (name, files) in &listing.segments {
                if !files.seg {
                    continue;
                }
                // An index of its own that a lookup found gone is not
                // there, whatever the listing says.
                let own_gone = catalog
                    .by_file
                    .get(&format!("{name}.idx"))
                    .is_some_and(|&own| catalog.sources[own as usize].is_gone());
                if let Some(&source) = best.get(name) {
                    changed |= catalog.seal(name, source);
                } else if files.idx && !own_gone {
                    let source = catalog.own_source(&self.dir, name, None);
                    changed |= catalog.seal(name, source);
                } else {
                    // Open, or new to this handle. Its journal is opened by
                    // name, whether the listing showed it or not.
                    match catalog.state(name) {
                        // Its own index is gone, merged into one that this
                        // listing missed.
                        Some(SegmentState::Sealed { source })
                            if catalog.sources[source as usize].kind == SourceKind::Own =>
                        {
                            settled &= last;
                        }
                        Some(SegmentState::Sealed { .. }) => {}
                        Some(SegmentState::Open { journal_end }) => {
                            follow.push((name.clone(), journal_end));
                        }
                        None => follow.push((name.clone(), 0)),
                    }
                }
            }
            if changed {
                catalog.refresh();
            }
        }
        for (name, from) in follow {
            if self.follow(&name, from)? {
                continue;
            }
            // Neither a journal nor an index of its own: never written to,
            // or sealed and merged since the listing.
            let path = self.path(&name, "seg");
            let len = match fs::metadata(&path) {
                Ok(meta) => meta.len(),
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(io_error("read", &path)(err)),
            };
            if len == 0 {
                continue;
            }
            if !last {
                settled = false;
            } else if unreadable {
                // Merged into an index this handle cannot read: its records
                // say what it holds.
                let mut catalog = self.write_catalog();
                let source = catalog.scan_source(&name);
                if catalog.seal(&name, source) {
                    catalog.refresh();
                }
            }
        }
        Ok(settled)
    }

    /// Reads what is new in the journal of an open segment, from `from`.
    /// Returns `false` if the segment has neither a journal nor an index of
    /// its own any more.
    fn follow(&self, name: &str, from: u64) -> Result<bool, Error> {
        let path = self.path(name, "jrn");
        match File::open(&path) {
            Ok(file) => {
                let read = journal::read(&file, from, &path)?;
                self.apply(name, read.end, &read.entries);
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Sealed since the listing: the index replaced the journal.
                let idx = self.path(name, "idx");
                if !idx.try_exists().map_err(io_error("check", &idx))? {
                    return Ok(false);
                }
                let mut catalog = self.write_catalog();
                let source = catalog.own_source(&self.dir, name, None);
                if catalog.seal(name, source) {
                    catalog.refresh();
                }
                Ok(true)
            }
            Err(err) => Err(io_error("open", &path)(err)),
        }
    }

    /// Records the entries read from an open segment's journal, up to `end`,
    /// and returns the segment's number.
    fn apply(&self, name: &str, end: u64, entries: &[Entry]) -> u32 {
        let mut catalog = self.write_catalog();
        let segment = catalog.segment(name);
        let SegmentState::Open { journal_end } = &mut catalog.segments[segment as usize].state
        else {
            // Sealed since: its index serves its objects.
            return segment;
        };
        *journal_end = (*journal_end).max(end);
        for entry in entries {
            let location = Location {
                segment,
                entry: *entry,
            };
            match catalog.open.entry(entry.id) {
                hash_map::Entry::Vacant(slot) => {
                    slot.insert(location);
                }
                // A copy stored whole beats a delta, which beats chunks.
                hash_map::Entry::Occupied(mut slot) => {
                    if record::rank(entry.codec) < record::rank(slot.get().entry.codec) {
                        slot.insert(location);
                    }
                }
            }
        }
        segment
    }

    /// Where a copy of an object no worse than `rank` is, rescanning the
    /// directory once on a miss.
    pub fn find(&self, id: &ObjectId, rank: u8) -> Result<Option<Location>, Error> {
        if let Some(location) = self.locate(id, rank)? {
            return Ok(Some(location));
        }
        self.rescan()?;
        self.locate(id, rank)
    }

    /// Where this handle knows a copy of an object no worse than `rank` to
    /// be, by [`record::rank`], without listing the directory again: in an
    /// open segment's journal, or in the first index that serves one. The
    /// indexes it goes through are read as far as the lookup needs.
    pub fn locate(&self, id: &ObjectId, rank: u8) -> Result<Option<Location>, Error> {
        self.retry_gone(|| {
            let active = {
                let catalog = self.read_catalog();
                if let Some(location) = catalog.open.get(id)
                    && record::rank(location.entry.codec) <= rank
                {
                    return Ok(Some(*location));
                }
                Arc::clone(&catalog.active)
            };
            for active in active.iter() {
                let Some((entry, segment)) = active.source.find(id, self)? else {
                    continue;
                };
                if active.serves[usize::from(segment)] && record::rank(entry.codec) <= rank {
                    return Ok(Some(Location {
                        segment: active.source.covered[usize::from(segment)].1,
                        entry,
                    }));
                }
            }
            Ok(None)
        })
    }

    /// Runs a lookup through the indexes, and again after a rescan if one of
    /// them was merged away before this handle read it.
    fn retry_gone<T>(&self, mut lookup: impl FnMut() -> Result<T, Lookup>) -> Result<T, Error> {
        let mut rescans = 0;
        loop {
            match lookup() {
                Ok(found) => return Ok(found),
                Err(Lookup::Gone(_)) if rescans < RESCAN_PASSES => {
                    rescans += 1;
                    self.rescan()?;
                }
                Err(err) => return Err(err.into_error()),
            }
        }
    }

    /// The entries of every object this handle knows of, one copy each, the
    /// best ranked, in no particular order. Every index is read whole.
    pub fn known_entries(&self) -> Result<Vec<Entry>, Error> {
        self.retry_gone(|| {
            let (mut best, active) = {
                let catalog = self.read_catalog();
                let best: HashMap<ObjectId, Entry> = catalog
                    .open
                    .iter()
                    .map(|(id, location)| (*id, location.entry))
                    .collect();
                (best, Arc::clone(&catalog.active))
            };
            for active in active.iter() {
                for (entry, segment) in active.source.all(self)? {
                    if !active.serves[usize::from(segment)] {
                        continue;
                    }
                    match best.entry(entry.id) {
                        hash_map::Entry::Vacant(slot) => {
                            slot.insert(entry);
                        }
                        hash_map::Entry::Occupied(mut slot) => {
                            if record::rank(entry.codec) < record::rank(slot.get().codec) {
                                slot.insert(entry);
                            }
                        }
                    }
                }
            }
            Ok(best.into_values().collect())
        })
    }

    /// A handle to read a segment, and its path.
    fn reader(&self, segment: u32) -> Result<(Arc<File>, PathBuf), Error> {
        let catalog = self.read_catalog();
        let segment = &catalog.segments[segment as usize];
        let path = self.path(&segment.name, "seg");
        if let Some(file) = segment.reader.get() {
            return Ok((file.clone(), path));
        }
        let file = Arc::new(File::open(&path).map_err(io_error("open", &path))?);
        if self.cached_readers.load(Ordering::Relaxed) < MAX_CACHED_READERS
            && segment.reader.set(file.clone()).is_ok()
        {
            self.cached_readers.fetch_add(1, Ordering::Relaxed);
        }
        Ok((file, path))
    }

    /// Whether the copy at `location` can stand for a new write of the same
    /// object: `wanted` accepts it, its record is there to stay, and so is
    /// what it needs to be read. A delta needs its base stored whole; a
    /// chunked blob needs every chunk, stored whole or as a delta, with the
    /// size its list gives. Depth is bounded: a base is whole, and a chunk is
    /// never chunked itself.
    fn reuse(
        &self,
        writer: &mut Writer,
        location: &Location,
        wanted: Wanted,
    ) -> Result<bool, Error> {
        let codec = location.entry.codec;
        if codec == CODEC_DICTIONARY || (wanted == Wanted::NotChunked && codec == CODEC_CHUNKED) {
            return Ok(false);
        }
        if !self.record_stays(writer, location)? {
            return Ok(false);
        }
        match codec {
            CODEC_DELTA => {
                let Some(base) = self.reference(location)? else {
                    return Ok(false);
                };
                match self.find(&base, WHOLE)? {
                    Some(base) => self.record_stays(writer, &base),
                    None => Ok(false),
                }
            }
            CODEC_CHUNKED => {
                let list = match self.read_record(location) {
                    Ok(list) => list,
                    Err(Error::Corrupt { .. }) => return Ok(false),
                    Err(err) => return Err(err),
                };
                let Ok(chunks) = chunks::decode_list(list.stored(), list.header.raw_len) else {
                    return Ok(false);
                };
                for chunk in chunks {
                    let usable = match self.find(&chunk.id, NOT_CHUNKED)? {
                        Some(found) => {
                            found.entry.kind == Kind::Blob
                                && found.entry.raw_len == chunk.size
                                && self.reuse(writer, &found, Wanted::NotChunked)?
                        }
                        None => false,
                    };
                    if !usable {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    /// The base that `location`, a delta, names in its header. `None` if
    /// the header cannot be read or is not the one the entry describes.
    fn reference(&self, location: &Location) -> Result<Option<ObjectId>, Error> {
        let (file, path) = self.reader(location.segment)?;
        let len = record::header_len(location.entry.codec);
        let bytes = match read_range(&file, location.entry.offset, len as u64) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(io_error("read", &path)(err)),
        };
        match record::parse_header(&bytes) {
            Ok(header) if header == location.entry.header() && len > record::HEADER_LEN => {
                Ok(Some(ObjectId::from_bytes(record::array(
                    &bytes[record::HEADER_LEN..len],
                ))))
            }
            _ => Ok(None),
        }
    }

    /// Whether the base at `location` can be relied on by a new delta: it
    /// is stored whole and there to stay. If it sits in another writer's
    /// open segment, this handle's next sync flushes that segment.
    pub fn rely_on_base(&self, location: &Location) -> Result<bool, Error> {
        if !record::is_whole(location.entry.codec) {
            return Ok(false);
        }
        let mut writer = self.lock_writer();
        self.record_stays(&mut writer, location)
    }

    /// Whether the record at `location` is there to stay. A record in a
    /// sealed segment, or in this handle's own segment, is. A record in
    /// another writer's open segment is if it is whole; that segment is then
    /// flushed by this handle's next sync, since its writer may not have
    /// flushed it yet.
    fn record_stays(&self, writer: &mut Writer, location: &Location) -> Result<bool, Error> {
        let state = {
            let catalog = self.read_catalog();
            catalog.segments[location.segment as usize].state
        };
        let own = writer
            .lease
            .as_ref()
            .is_some_and(|lease| lease.segment == location.segment);
        if matches!(state, SegmentState::Sealed { .. }) || own {
            return Ok(true);
        }
        let (file, path) = self.reader(location.segment)?;
        let header = location.entry.header();
        let whole = match read_range(&file, location.entry.offset, header.unpadded_len()) {
            Ok(bytes) => matches!(
                record::check(&bytes),
                Ok(found) if found.header == header && found.crc == location.entry.crc
            ),
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => false,
            Err(err) => return Err(io_error("read", &path)(err)),
        };
        if whole {
            writer.foreign.insert(location.segment);
        }
        Ok(whole)
    }

    /// Appends a record to the leased segment, taking a lease first if
    /// needed, and seals the segment if it is now full. A record that needs
    /// a later storage version than the store has raises it first, so that
    /// no version of braid that does not know the record ever reads the
    /// store.
    fn append(&self, writer: &mut Writer, encoded: Encoded) -> Result<(), Error> {
        self.need_storage(record::storage_version(encoded.header.codec))?;
        if writer.lease.is_none() {
            let lease = self.acquire(writer)?;
            writer.lease = Some(lease);
        }
        let Some(lease) = writer.lease.as_mut() else {
            unreachable!("a lease was just taken");
        };
        let entry = Entry::new(lease.seg_end, &encoded.header, encoded.crc);
        let written = write_all_at(&lease.seg, &encoded.bytes, lease.seg_end)
            .map_err(io_error("write", &self.path(&lease.name, "seg")))
            .and_then(|()| {
                write_all_at(&lease.journal, &entry.encode(), lease.journal_end)
                    .map_err(io_error("write", &self.path(&lease.name, "jrn")))
            });
        if let Err(err) = written {
            // The write position is unknown now. Give the segment up: whoever
            // takes it next resumes from what its files hold.
            writer.retire();
            return Err(err);
        }
        lease.seg_end += encoded.bytes.len() as u64;
        lease.journal_end += ENTRY_LEN;
        lease.records += 1;
        lease.dirty = true;
        let segment = lease.segment;
        let journal_end = lease.journal_end;
        let full = self.full(lease.seg_end, lease.records);
        {
            let mut catalog = self.write_catalog();
            // Replaces a damaged copy elsewhere, if that is why this was written.
            catalog.open.insert(entry.id, Location { segment, entry });
            if let SegmentState::Open { journal_end: known } =
                &mut catalog.segments[segment as usize].state
            {
                *known = (*known).max(journal_end);
            }
        }
        if full && let Some(lease) = writer.lease.take() {
            // The object is stored either way. If sealing fails, the segment
            // stays open and full, and the next writer to take it seals it.
            let _ = self.seal(writer, lease);
        }
        Ok(())
    }

    /// Whether a segment of `len` bytes and `records` records is full.
    fn full(&self, len: u64, records: u64) -> bool {
        len >= self.segment_size || (self.segment_records > 0 && records >= self.segment_records)
    }

    /// Takes a lease: on an unlocked segment that is not sealed if there is
    /// one, else on a new segment.
    fn acquire(&self, writer: &mut Writer) -> Result<Lease, Error> {
        for (name, files) in list(&self.dir)?.segments {
            // An open segment has a journal and no index; a segment with
            // neither is sealed and merged, or was never written to.
            if !files.seg || !files.jrn || files.idx {
                continue;
            }
            let Some(lease) = self.try_lease(&name)? else {
                continue;
            };
            if self.full(lease.seg_end, lease.records) {
                // Full, but whoever filled it died before sealing it. If
                // sealing fails now, it is left for later.
                let _ = self.seal(writer, lease);
                continue;
            }
            return Ok(lease);
        }
        self.create()
    }

    /// Takes a lease on an existing segment if nobody holds it and it is not
    /// sealed.
    fn try_lease(&self, name: &str) -> Result<Option<Lease>, Error> {
        let seg_path = self.path(name, "seg");
        let seg = match OpenOptions::new().read(true).write(true).open(&seg_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_error("open", &seg_path)(err)),
        };
        match seg.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(err)) => return Err(io_error("lock", &seg_path)(err)),
        }
        let idx_path = self.path(name, "idx");
        if idx_path
            .try_exists()
            .map_err(io_error("check", &idx_path))?
        {
            // Sealed since the listing. A journal still next to the index was
            // left by a sealer that died before removing it.
            remove_if_exists(&self.path(name, "jrn"))?;
            return Ok(None);
        }
        let jrn_path = self.path(name, "jrn");
        // Never created here: a segment without a journal is sealed and
        // merged since the listing, and merging removes a journal before it
        // removes the index.
        let journal = match OpenOptions::new().read(true).write(true).open(&jrn_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_error("open", &jrn_path)(err)),
        };
        // A journal a newer version wrote to stops here: this version never
        // writes after entries it does not understand.
        let read = journal::read(&journal, 0, &jrn_path)?;
        if read.entries.is_empty() {
            // Whoever created these files may have died before flushing the
            // directory, and objects acknowledged from now on depend on
            // them. Once a journal lists a record, someone has.
            sync_dir(&self.dir, self.durability)?;
        }
        let seg_len = seg.metadata().map_err(io_error("read", &seg_path))?.len();
        // Never write over bytes an entry points at, even if they are gone.
        let claimed = read.entries.iter().map(Entry::end).max().unwrap_or(0);
        let seg_end = record::align(seg_len).max(claimed);
        let records = read.entries.len() as u64;
        let segment = self.apply(name, read.end, &read.entries);
        Ok(Some(Lease {
            segment,
            name: name.to_owned(),
            seg: Arc::new(seg),
            journal: Arc::new(journal),
            seg_end,
            journal_end: read.end,
            records,
            dirty: false,
        }))
    }

    /// Creates a new segment and takes a lease on it.
    fn create(&self) -> Result<Lease, Error> {
        loop {
            let name = random_name(&self.dir)?;
            let seg_path = self.path(&name, "seg");
            let seg = match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&seg_path)
            {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(io_error("create", &seg_path)(err)),
            };
            match seg.try_lock() {
                Ok(()) => {}
                // Another writer took the new segment before this one locked
                // it. Let it keep it.
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(err)) => return Err(io_error("lock", &seg_path)(err)),
            }
            let jrn_path = self.path(&name, "jrn");
            let journal = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&jrn_path)
                .map_err(io_error("create", &jrn_path))?;
            // Before any record: others may rely on what this segment holds
            // and flush it, but not its directory.
            sync_dir(&self.dir, self.durability)?;
            let segment = self.apply(&name, 0, &[]);
            return Ok(Lease {
                segment,
                name,
                seg: Arc::new(seg),
                journal: Arc::new(journal),
                seg_end: 0,
                journal_end: 0,
                records: 0,
                dirty: false,
            });
        }
    }

    /// Seals a segment and gives its lease up.
    fn seal(&self, writer: &mut Writer, lease: Lease) -> Result<(), Error> {
        match self.write_index(&lease) {
            Ok(index) => {
                let mut catalog = self.write_catalog();
                let source = catalog.own_source(&self.dir, &lease.name, Some(index));
                if catalog.seal(&lease.name, source) {
                    catalog.refresh();
                }
                drop(catalog);
                self.note_own_indexes();
                Ok(())
            }
            Err(err) => {
                writer.keep_unsynced(lease);
                Err(err)
            }
        }
    }

    /// Writes a segment's index, flushing the segment with it, and removes
    /// its journal. Returns what it covers and its entries.
    fn write_index(&self, lease: &Lease) -> Result<(Covered, Vec<Entry>), Error> {
        // A version 2 index needs a store that says so first.
        self.need_storage(INDEX_STORAGE)?;
        let seg_path = self.path(&lease.name, "seg");
        let jrn_path = self.path(&lease.name, "jrn");
        let read = journal::read(&lease.journal, 0, &jrn_path)?;
        let seg_len = lease
            .seg
            .metadata()
            .map_err(io_error("read", &seg_path))?
            .len();
        // Only entries whose record is whole go in. Of several copies of an
        // object, the best ranked wins, and the first of those.
        let mut window = Window::new(&lease.seg, seg_len);
        let mut chosen: HashMap<ObjectId, usize> = HashMap::new();
        let mut entries: Vec<Entry> = Vec::with_capacity(read.entries.len());
        for entry in read.entries {
            if !scan::matches(&mut window, &entry).map_err(io_error("read", &seg_path))? {
                continue;
            }
            match chosen.get(&entry.id) {
                None => {
                    chosen.insert(entry.id, entries.len());
                    entries.push(entry);
                }
                Some(&at) if record::rank(entry.codec) < record::rank(entries[at].codec) => {
                    entries[at] = entry;
                }
                Some(_) => {}
            }
        }
        entries.sort_by_key(|entry| entry.id);
        let covered = Covered {
            name: lease.name.clone(),
            len: entries.iter().map(Entry::end).fold(seg_len, u64::max),
        };
        let numbered: Vec<(Entry, u16)> = entries.iter().map(|entry| (*entry, 0)).collect();
        remove_stale_temps(&self.dir);
        // The index must never reach the disk before the records it points
        // at: they are flushed in one batch with it, before the rename.
        publish_file(
            &self.dir,
            &format!("{}.idx", lease.name),
            &index::encode(std::slice::from_ref(&covered), &numbered),
            self.durability,
            &[(&seg_path, &lease.seg)],
        )?;
        // The index supersedes the journal. A journal that cannot be removed
        // now is harmless, and the next writer to look at the segment
        // removes it.
        let _ = fs::remove_file(&jrn_path);
        Ok((covered, entries))
    }

    fn sync_locked(&self, writer: &mut Writer) -> Result<(), Error> {
        let mut segments: Vec<(PathBuf, Arc<File>)> = Vec::new();
        let mut journals: Vec<(PathBuf, Arc<File>)> = Vec::new();
        if let Some(lease) = &writer.lease
            && lease.dirty
        {
            segments.push((self.path(&lease.name, "seg"), lease.seg.clone()));
            journals.push((self.path(&lease.name, "jrn"), lease.journal.clone()));
        }
        for (name, seg, journal) in &writer.unsynced {
            segments.push((self.path(name, "seg"), seg.clone()));
            journals.push((self.path(name, "jrn"), journal.clone()));
        }
        for &segment in &writer.foreign {
            let name = {
                let catalog = self.read_catalog();
                catalog.segments[segment as usize].name.clone()
            };
            for (extension, list) in [("seg", &mut segments), ("jrn", &mut journals)] {
                let path = self.path(&name, extension);
                match File::open(&path) {
                    Ok(file) => list.push((path, Arc::new(file))),
                    // Sealed since, and its journal removed: the index is durable.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(io_error("open", &path)(err)),
                }
            }
        }
        if segments.is_empty() && journals.is_empty() {
            return Ok(());
        }
        let dir = match self.durability {
            Durability::Full => open_dir(&self.dir)?,
            Durability::Ordered => None,
        };
        let mut batch: Vec<(&Path, &File)> = segments
            .iter()
            .chain(&journals)
            .map(|(path, file)| (path.as_path(), file.as_ref()))
            .collect();
        if let Some(dir) = &dir {
            batch.push((&self.dir, dir));
        }
        flush(&batch, self.durability)?;
        if let Some(lease) = &mut writer.lease {
            lease.dirty = false;
        }
        writer.unsynced.clear();
        writer.foreign.clear();
        Ok(())
    }

    /// Notes whether enough segment indexes have piled up to merge them.
    fn note_own_indexes(&self) {
        if self.merge_threshold == 0 {
            return;
        }
        let own = self
            .read_catalog()
            .active
            .iter()
            .filter(|active| active.source.kind != SourceKind::Merged)
            .count();
        if own >= self.merge_threshold {
            self.merge_due.store(true, Ordering::Release);
        }
    }

    fn lock_writer(&self) -> MutexGuard<'_, Writer> {
        match self.writer.lock() {
            Ok(writer) => writer,
            Err(poisoned) => {
                let mut writer = poisoned.into_inner();
                // A panic in the middle of an append leaves the write position
                // unknown: start over from the files.
                writer.retire();
                self.writer.clear_poison();
                writer
            }
        }
    }

    fn read_catalog(&self) -> std::sync::RwLockReadGuard<'_, Catalog> {
        self.catalog.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_catalog(&self) -> std::sync::RwLockWriteGuard<'_, Catalog> {
        self.catalog.write().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Catalog {
    /// The number of a segment, registering it as open if it is new.
    fn segment(&mut self, name: &str) -> u32 {
        if let Some(&index) = self.by_name.get(name) {
            return index;
        }
        let index = u32::try_from(self.segments.len()).expect("fewer than 2^32 segments");
        self.segments.push(Segment {
            name: name.to_owned(),
            state: SegmentState::Open { journal_end: 0 },
            reader: OnceLock::new(),
        });
        self.by_name.insert(name.to_owned(), index);
        index
    }

    /// What this handle knows of a segment's state.
    fn state(&self, name: &str) -> Option<SegmentState> {
        let &segment = self.by_name.get(name)?;
        Some(self.segments[segment as usize].state)
    }

    /// Registers an index, numbering the segments it covers, and returns its
    /// number. An index already registered keeps its number.
    fn register(&mut self, source: Source) -> u32 {
        if let Some(&number) = self.by_file.get(&source.file) {
            return number;
        }
        let mut source = source;
        for (name, segment) in &mut source.covered {
            *segment = self.segment(name);
        }
        let number = u32::try_from(self.sources.len()).expect("fewer than 2^32 indexes");
        self.by_file.insert(source.file.clone(), number);
        self.sources.push(Arc::new(source));
        number
    }

    /// The number of a segment's own index, registered if new. `known` is
    /// what it covers and its entries, when this handle just wrote it.
    fn own_source(&mut self, dir: &Path, name: &str, known: Option<(Covered, Vec<Entry>)>) -> u32 {
        let file = format!("{name}.idx");
        if let Some(&number) = self.by_file.get(&file) {
            return number;
        }
        self.register(Source::own(dir, name, known))
    }

    /// The number of a source that scans a sealed segment no readable index
    /// covers.
    fn scan_source(&mut self, name: &str) -> u32 {
        let file = format!("{name}.seg");
        if let Some(&number) = self.by_file.get(&file) {
            return number;
        }
        self.register(Source::scan(name))
    }

    /// Makes `source` serve the segment `name` if it is better than what
    /// serves it now, and returns whether anything changed. A segment that
    /// was open forgets its journal's entries: the index serves them now.
    fn seal(&mut self, name: &str, source: u32) -> bool {
        let segment = self.segment(name);
        let state = &mut self.segments[segment as usize].state;
        match *state {
            SegmentState::Sealed { source: current }
                if current == source
                    || self.sources[current as usize].rank()
                        >= self.sources[source as usize].rank() =>
            {
                false
            }
            SegmentState::Sealed { .. } => {
                *state = SegmentState::Sealed { source };
                true
            }
            SegmentState::Open { .. } => {
                *state = SegmentState::Sealed { source };
                self.open.retain(|_, location| location.segment != segment);
                true
            }
        }
    }

    /// Rebuilds the list of indexes lookups go through: those that serve a
    /// segment, the segments' own first, which are the youngest, then the
    /// merged ones, smallest first.
    fn refresh(&mut self) {
        let mut active: Vec<Active> = Vec::new();
        for (number, source) in self.sources.iter().enumerate() {
            let serves: Box<[bool]> = source
                .covered
                .iter()
                .map(|(_, segment)| {
                    self.segments[*segment as usize].state
                        == SegmentState::Sealed {
                            source: number as u32,
                        }
                })
                .collect();
            if serves.iter().any(|serves| *serves) {
                active.push(Active {
                    source: Arc::clone(source),
                    serves,
                });
            }
        }
        active.sort_by(|a, b| {
            let key = |active: &Active| {
                (
                    active.source.kind,
                    active.source.covered.len(),
                    active.source.file.clone(),
                )
            };
            key(a).cmp(&key(b))
        });
        self.active = Arc::from(active);
    }
}

/// The error for an object whose stored form does not fit in a record.
pub(crate) fn too_large(size: u64) -> Error {
    Error::ObjectTooLarge { size }
}

/// What `seg/` holds: segments and which of their files exist, and merged
/// indexes.
pub(crate) fn list(dir: &Path) -> Result<Listing, Error> {
    let mut out = Listing::default();
    for entry in fs::read_dir(dir).map_err(io_error("list", dir))? {
        let entry = entry.map_err(io_error("list", dir))?;
        let file_name = entry.file_name();
        let Some((stem, extension)) = file_name.to_str().and_then(|name| name.split_once('.'))
        else {
            continue;
        };
        if !is_random_name(stem) {
            continue;
        }
        if extension == "midx" {
            out.merged.insert(stem.to_owned());
            continue;
        }
        let files: &mut Files = out.segments.entry(stem.to_owned()).or_default();
        match extension {
            "seg" => files.seg = true,
            "jrn" => files.jrn = true,
            "idx" => files.idx = true,
            _ => {}
        }
    }
    Ok(out)
}

/// The path of one of a segment's files.
pub(crate) fn segment_path(dir: &Path, name: &str, extension: &str) -> PathBuf {
    dir.join(format!("{name}.{extension}"))
}

fn remove_if_exists(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(io_error("remove", path)(err)),
    }
}
