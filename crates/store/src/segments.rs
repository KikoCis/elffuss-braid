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
//! - **Sealing.** Once a segment passes the size threshold, its holder
//!   flushes it, writes its sorted index through a temporary file and a
//!   rename, flushes the directory, and removes the journal. A sealed segment
//!   is never written again.
//! - **Lookups.** Every handle keeps a map from id to location, built from the
//!   indexes and journals. On a miss it rescans the directory once, since
//!   another process may have written or sealed something since.
//! - **Durability.** [`Segments::put`] appends without flushing;
//!   [`Segments::sync`] flushes everything this handle wrote, segments first,
//!   then journals, so that a durable journal entry always points at a
//!   durable record.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};

use braid_format::{Kind, ObjectId};

use crate::error::{Error, io_error};
use crate::fsutil::{
    is_random_name, publish_file, random_name, read_range, remove_stale_temps, sync_dir, sync_file,
    write_all_at,
};
use crate::journal::{self, ENTRY_LEN, Entry};
use crate::record::{self, Encoded};
use crate::scan::{self, Window};
use crate::{Durability, Options, index};

/// Segments a handle keeps open for reading. Past this, a read opens the
/// segment for itself.
const MAX_CACHED_READERS: usize = 256;

/// The object half of a store: the `seg/` directory.
#[derive(Debug)]
pub(crate) struct Segments {
    dir: PathBuf,
    segment_size: u64,
    durability: Durability,
    catalog: RwLock<Catalog>,
    /// Held while rescanning the directory, so that one thread does it at a time.
    rescanning: Mutex<()>,
    writer: Mutex<Writer>,
    /// Segments with a cached reader.
    cached_readers: AtomicUsize,
}

/// What this handle knows about the segments and the objects in them.
#[derive(Debug, Default)]
struct Catalog {
    segments: Vec<Segment>,
    by_name: HashMap<String, u32>,
    objects: HashMap<ObjectId, Location>,
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
    /// Sealed: its index has been read, and it will not change again.
    Sealed,
}

#[derive(Clone, Copy, Debug)]
struct Location {
    segment: u32,
    entry: Entry,
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

/// How a read changes what the catalog knows about a segment.
#[derive(Clone, Copy, Debug)]
enum Update {
    /// The journal was read up to this offset.
    Journal(u64),
    /// The index was read.
    Sealed,
}

impl Segments {
    /// Opens the segments in `dir` and reads every index and journal.
    pub fn open(dir: PathBuf, options: &Options) -> Result<Self, Error> {
        let segments = Self {
            dir,
            segment_size: options.segment_size,
            durability: options.durability,
            catalog: RwLock::new(Catalog::default()),
            rescanning: Mutex::new(()),
            writer: Mutex::new(Writer::default()),
            cached_readers: AtomicUsize::new(0),
        };
        segments.rescan()?;
        Ok(segments)
    }

    /// The path of one of a segment's files.
    pub fn path(&self, name: &str, extension: &str) -> PathBuf {
        segment_path(&self.dir, name, extension)
    }

    /// Stores an object and returns its id. Nothing is flushed: call
    /// [`Segments::sync`] before anything durable refers to the object.
    pub fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Error> {
        let id = ObjectId::hash(kind, payload);
        // Compress outside the writer lock, unless a copy is already known.
        let mut encoded = if self.location(&id).is_some() {
            None
        } else {
            Some(encode(kind, id, payload)?)
        };
        let mut writer = self.lock_writer();
        if let Some(location) = self.location(&id)
            && self.reuse(&mut writer, &location)?
        {
            return Ok(id);
        }
        let encoded = match encoded.take() {
            Some(encoded) => encoded,
            None => encode(kind, id, payload)?,
        };
        self.append(&mut writer, encoded)?;
        Ok(id)
    }

    /// Reads an object, checking its record and its id.
    pub fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Error> {
        let Some(location) = self.find(id)? else {
            return Ok(None);
        };
        let (file, path) = self.reader(location.segment)?;
        let payload = read_object(&file, &path, &location.entry)?;
        Ok(Some((location.entry.kind, payload)))
    }

    /// The kind and size of an object, from its record's header alone: the
    /// payload is neither read nor decompressed. The header is checked
    /// against the object's index or journal entry, which has a checksum of
    /// its own, so a damaged header is an error; damage to the payload only
    /// shows on [`Segments::get`].
    pub fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, Error> {
        let Some(location) = self.find(id)? else {
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
        Ok(self.find(id)?.is_some())
    }

    /// Flushes everything this handle wrote: segments first, then journals.
    pub fn sync(&self) -> Result<(), Error> {
        let mut writer = self.lock_writer();
        self.sync_locked(&mut writer)
    }

    /// Makes sure the objects in `ids` are durable before something durable
    /// refers to them: flushes this handle's writes, and the open segments of
    /// other writers that hold any of them. Fails if a record is damaged.
    /// Objects this handle does not know about are skipped.
    pub fn make_durable(&self, ids: &[ObjectId]) -> Result<(), Error> {
        let mut writer = self.lock_writer();
        for id in ids {
            let Some(location) = self.location(id) else {
                continue;
            };
            if !self.reuse(&mut writer, &location)? {
                let (_, path) = self.reader(location.segment)?;
                return Err(Error::corrupt(
                    &path,
                    location.entry.offset,
                    format!("object {id}: record is damaged"),
                ));
            }
        }
        self.sync_locked(&mut writer)
    }

    /// Reads what changed in the directory since the last look: new
    /// segments, new journal entries, and segments sealed since.
    pub fn rescan(&self) -> Result<(), Error> {
        let _one_at_a_time = self
            .rescanning
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (name, files) in list(&self.dir)? {
            if !files.seg {
                continue;
            }
            let state = {
                let catalog = self.catalog.read().unwrap_or_else(PoisonError::into_inner);
                catalog
                    .by_name
                    .get(&name)
                    .map(|&index| catalog.segments[index as usize].state)
            };
            match state {
                Some(SegmentState::Sealed) => {}
                Some(SegmentState::Open { journal_end }) => {
                    self.follow(&name, journal_end, files.idx)?;
                }
                None if files.idx || files.jrn => self.follow(&name, 0, files.idx)?,
                // Being created, or its writer died before writing anything.
                None => {}
            }
        }
        Ok(())
    }

    /// Reads what is new in a segment: its index if it is sealed, else its
    /// journal from `from`.
    fn follow(&self, name: &str, from: u64, sealed: bool) -> Result<(), Error> {
        if !sealed {
            let path = self.path(name, "jrn");
            match File::open(&path) {
                Ok(file) => {
                    let read = journal::read(&file, from).map_err(io_error("read", &path))?;
                    self.apply(name, Update::Journal(read.end), &read.entries);
                    return Ok(());
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    // Sealed since the listing: the index replaced the journal.
                    let idx = self.path(name, "idx");
                    if !idx.try_exists().map_err(io_error("check", &idx))? {
                        return Ok(());
                    }
                }
                Err(err) => return Err(io_error("open", &path)(err)),
            }
        }
        let entries = self.load_index(name)?;
        self.apply(name, Update::Sealed, &entries);
        Ok(())
    }

    /// The entries of a sealed segment. A damaged index is worked around by
    /// scanning the segment, whose records describe themselves; fsck reports
    /// the damage.
    fn load_index(&self, name: &str) -> Result<Vec<Entry>, Error> {
        let path = self.path(name, "idx");
        let bytes = fs::read(&path).map_err(io_error("read", &path))?;
        match index::decode(&bytes) {
            Ok(index) => Ok(index.entries),
            Err(_) => {
                let path = self.path(name, "seg");
                let file = File::open(&path).map_err(io_error("open", &path))?;
                let len = file.metadata().map_err(io_error("read", &path))?.len();
                let mut entries = Vec::new();
                scan::scan(&file, len, |offset, header, crc, _| {
                    entries.push(Entry::new(offset, header, crc));
                })
                .map_err(io_error("read", &path))?;
                Ok(entries)
            }
        }
    }

    /// Records what was read about a segment, and returns its number.
    fn apply(&self, name: &str, update: Update, entries: &[Entry]) -> u32 {
        let mut catalog = self.catalog.write().unwrap_or_else(PoisonError::into_inner);
        let segment = catalog.segment(name);
        let state = &mut catalog.segments[segment as usize].state;
        match (update, &mut *state) {
            (Update::Sealed, _) => *state = SegmentState::Sealed,
            (Update::Journal(end), SegmentState::Open { journal_end }) => {
                *journal_end = (*journal_end).max(end);
            }
            (Update::Journal(_), SegmentState::Sealed) => {}
        }
        for entry in entries {
            catalog.objects.entry(entry.id).or_insert(Location {
                segment,
                entry: *entry,
            });
        }
        segment
    }

    /// Where an object is, rescanning the directory once on a miss.
    fn find(&self, id: &ObjectId) -> Result<Option<Location>, Error> {
        if let Some(location) = self.location(id) {
            return Ok(Some(location));
        }
        self.rescan()?;
        Ok(self.location(id))
    }

    fn location(&self, id: &ObjectId) -> Option<Location> {
        self.catalog
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .objects
            .get(id)
            .copied()
    }

    /// A handle to read a segment, and its path.
    fn reader(&self, segment: u32) -> Result<(Arc<File>, PathBuf), Error> {
        let catalog = self.catalog.read().unwrap_or_else(PoisonError::into_inner);
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
    /// object. A copy in a sealed segment, or in this handle's own segment,
    /// can. A copy in another writer's open segment can if its record is
    /// whole; that segment is then flushed by this handle's next sync, since
    /// its writer may not have flushed it yet.
    fn reuse(&self, writer: &mut Writer, location: &Location) -> Result<bool, Error> {
        let state = {
            let catalog = self.catalog.read().unwrap_or_else(PoisonError::into_inner);
            catalog.segments[location.segment as usize].state
        };
        let own = writer
            .lease
            .as_ref()
            .is_some_and(|lease| lease.segment == location.segment);
        if state == SegmentState::Sealed || own {
            return Ok(true);
        }
        let (file, path) = self.reader(location.segment)?;
        let header = location.entry.header();
        let whole = match read_range(&file, location.entry.offset, header.unpadded_len()) {
            Ok(bytes) => matches!(
                record::check(&bytes),
                Ok((found, crc, _)) if found == header && crc == location.entry.crc
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
    /// needed, and seals the segment if it is now full.
    fn append(&self, writer: &mut Writer, encoded: Encoded) -> Result<(), Error> {
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
        lease.dirty = true;
        let segment = lease.segment;
        let journal_end = lease.journal_end;
        let full = lease.seg_end >= self.segment_size;
        {
            let mut catalog = self.catalog.write().unwrap_or_else(PoisonError::into_inner);
            // Replaces a damaged copy elsewhere, if that is why this was written.
            catalog
                .objects
                .insert(entry.id, Location { segment, entry });
            if let SegmentState::Open { journal_end: known } =
                &mut catalog.segments[segment as usize].state
            {
                *known = (*known).max(journal_end);
            }
        }
        if full && let Some(lease) = writer.lease.take() {
            // The object is stored either way. If sealing fails, the segment
            // stays open and over the threshold, and the next writer to take
            // it seals it.
            let _ = self.seal(writer, lease);
        }
        Ok(())
    }

    /// Takes a lease: on an unlocked segment that is not sealed if there is
    /// one, else on a new segment.
    fn acquire(&self, writer: &mut Writer) -> Result<Lease, Error> {
        for (name, files) in list(&self.dir)? {
            if !files.seg || files.idx {
                continue;
            }
            let Some(lease) = self.try_lease(&name)? else {
                continue;
            };
            if lease.seg_end >= self.segment_size {
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
        let journal = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&jrn_path)
            .map_err(io_error("open", &jrn_path))?;
        // Whoever created these files may have died before flushing the
        // directory, and objects acknowledged from now on depend on them.
        sync_dir(&self.dir)?;
        let read = journal::read(&journal, 0).map_err(io_error("read", &jrn_path))?;
        let seg_len = seg.metadata().map_err(io_error("read", &seg_path))?.len();
        // Never write over bytes an entry points at, even if they are gone.
        let claimed = read.entries.iter().map(Entry::end).max().unwrap_or(0);
        let seg_end = record::align(seg_len).max(claimed);
        let segment = self.apply(name, Update::Journal(read.end), &read.entries);
        Ok(Some(Lease {
            segment,
            name: name.to_owned(),
            seg: Arc::new(seg),
            journal: Arc::new(journal),
            seg_end,
            journal_end: read.end,
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
            sync_dir(&self.dir)?;
            let segment = self.apply(&name, Update::Journal(0), &[]);
            return Ok(Lease {
                segment,
                name,
                seg: Arc::new(seg),
                journal: Arc::new(journal),
                seg_end: 0,
                journal_end: 0,
                dirty: false,
            });
        }
    }

    /// Seals a segment and gives its lease up.
    fn seal(&self, writer: &mut Writer, lease: Lease) -> Result<(), Error> {
        match self.write_index(&lease) {
            Ok(()) => {
                self.apply(&lease.name, Update::Sealed, &[]);
                Ok(())
            }
            Err(err) => {
                writer.keep_unsynced(lease);
                Err(err)
            }
        }
    }

    /// Flushes a segment, writes its index, and removes its journal.
    fn write_index(&self, lease: &Lease) -> Result<(), Error> {
        let seg_path = self.path(&lease.name, "seg");
        let jrn_path = self.path(&lease.name, "jrn");
        // The index must never point at data that is not durable.
        sync_file(&lease.seg, self.durability).map_err(io_error("sync", &seg_path))?;
        let read = journal::read(&lease.journal, 0).map_err(io_error("read", &jrn_path))?;
        let seg_len = lease
            .seg
            .metadata()
            .map_err(io_error("read", &seg_path))?
            .len();
        // Only entries whose record is whole go in, and the first copy of an
        // object wins.
        let mut window = Window::new(&lease.seg, seg_len);
        let mut seen = HashSet::new();
        let mut entries = Vec::with_capacity(read.entries.len());
        for entry in read.entries {
            if !seen.contains(&entry.id)
                && scan::matches(&mut window, &entry).map_err(io_error("read", &seg_path))?
            {
                seen.insert(entry.id);
                entries.push(entry);
            }
        }
        entries.sort_by_key(|entry| entry.id);
        let covered = entries.iter().map(Entry::end).fold(seg_len, u64::max);
        remove_stale_temps(&self.dir);
        publish_file(
            &self.dir,
            &format!("{}.idx", lease.name),
            &index::encode(&entries, covered),
        )?;
        // The index supersedes the journal. A journal that cannot be removed
        // now is harmless, and the next writer to look at the segment
        // removes it.
        let _ = fs::remove_file(&jrn_path);
        Ok(())
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
                let catalog = self.catalog.read().unwrap_or_else(PoisonError::into_inner);
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
        for (path, file) in segments.iter().chain(&journals) {
            sync_file(file, self.durability).map_err(io_error("sync", path))?;
        }
        if self.durability == Durability::Full {
            sync_dir(&self.dir)?;
        }
        if let Some(lease) = &mut writer.lease {
            lease.dirty = false;
        }
        writer.unsynced.clear();
        writer.foreign.clear();
        Ok(())
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
}

fn encode(kind: Kind, id: ObjectId, payload: &[u8]) -> Result<Encoded, Error> {
    record::encode(kind, id, payload).map_err(|size| Error::ObjectTooLarge { size })
}

/// Reads and checks the record an entry points at, and decodes its payload.
pub(crate) fn read_object(file: &File, path: &Path, entry: &Entry) -> Result<Vec<u8>, Error> {
    let corrupt =
        |detail: &str| Error::corrupt(path, entry.offset, format!("object {}: {detail}", entry.id));
    let header = entry.header();
    let bytes = match read_range(file, entry.offset, header.unpadded_len()) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(corrupt("record runs past the end of the segment"));
        }
        Err(err) => return Err(io_error("read", path)(err)),
    };
    let (found, crc, stored) = record::check(&bytes).map_err(corrupt)?;
    if found != header || crc != entry.crc {
        return Err(corrupt("record does not match its entry"));
    }
    record::decode(&found, stored).map_err(corrupt)
}

/// The segments in `dir` and which of their files exist, by name.
pub(crate) fn list(dir: &Path) -> Result<BTreeMap<String, Files>, Error> {
    let mut out = BTreeMap::new();
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
        let files: &mut Files = out.entry(stem.to_owned()).or_default();
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
