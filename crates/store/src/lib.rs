// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The local store of elffuss-braid: objects in shared, append-only segments,
//! and refs with the history of every operation in an op log.
//!
//! A store is a directory that any number of processes share at once, with
//! no daemon, and any of them may be killed at any instant. Correctness rests
//! only on what every local file system provides: exclusive create, appends
//! protected by checksums, advisory locks that the kernel releases when a
//! process dies, atomic rename, and ordered syncs.
//!
//! # Example
//!
//! ```
//! use std::convert::Infallible;
//!
//! use braid_store::{Kind, Objects, Options, RefUpdate, Store};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let dir = tempfile::tempdir()?;
//! let store = Store::init(dir.path().join("store"), Options::default())?;
//! let id = store.put(Kind::Blob, b"hello\n")?;
//! store.sync()?;
//! let op = store.transact("agent-1", "point main at hello", |refs| {
//!     Ok::<_, Infallible>(vec![RefUpdate::new("main", refs.get("main"), Some(id))])
//! })?;
//! assert_eq!(op.seq, 1);
//! assert_eq!(store.refs()?.get("main"), Some(id));
//! assert_eq!(store.get(&id)?, Some((Kind::Blob, b"hello\n".to_vec())));
//! assert!(store.fsck()?.is_clean());
//! # Ok(())
//! # }
//! ```
//!
//! # Publish order
//!
//! The op log must never point at an object that is not durable: write the
//! objects, call [`Store::sync`], and only then record the operation that
//! refers to them. [`Store::transact`] enforces this for the objects its
//! updates point at: before it appends the record, it flushes what this
//! handle wrote and any other writer's open segment that holds one of them.
//! Objects that only other objects refer to (the trees under a changeset,
//! say) are the caller's to sync before the transaction.
//!
//! # Sharing
//!
//! - A writer appends only to a segment it holds an exclusive, non-blocking
//!   lock on: an unlocked open segment if there is one, else a new one made
//!   with an exclusive create. The kernel releases the lock when the writer
//!   dies, and the next writer resumes after what the segment's journal
//!   lists. Segments are never truncated, since others may be reading them.
//! - After each record, its writer appends a journal entry, so readers find
//!   new objects without reading payloads.
//! - A segment past the size threshold, or with as many records as a segment
//!   may hold, is sealed: given a sorted index through a temporary file,
//!   flushed with the segment, and a rename, and its journal removed. It is
//!   never written again.
//! - Once enough segment indexes pile up, a writer merges them into a
//!   merged index, so that lookups go through few indexes.
//! - Each handle maps ids to records only for the open segments, from their
//!   journals, which the record limit keeps short. Sealed segments are
//!   looked up in their indexes, which it reads as lookups need them, a
//!   fan-out bucket at a time: opening a store reads no index entry,
//!   whatever it holds. On a miss a handle reads the directory again once.
//!   Short ids, hex prefixes of ids, are looked up the same way
//!   ([`Store::ids_with_prefix`]).
//! - Transactions hold an exclusive lock on `oplog/lock`; readers of the op
//!   log hold it shared, so they never see an operation before it is
//!   durable.
//!
//! # On-disk format
//!
//! This is section 7 of the format spec as implemented. It is not hashed:
//! it can change without changing any id. Fixed-size integers are
//! little-endian; `uv`, `sv` and `bytes` are the codec of section 1.
//!
//! ```text
//! format             object-format=blake3-dk-v1, then storage=<version>
//! format.lock        empty; locked exclusively to raise the storage version
//! seg/<name>.seg     object records, append-only
//! seg/<name>.jrn     journal of a segment that is still open
//! seg/<name>.idx     sorted index of a sealed segment
//! seg/<name>.midx    merged index of several sealed segments
//! seg/merge.lock     empty; locked exclusively while indexes are merged
//! oplog/lock         empty; locked exclusively by transactions, shared by readers
//! oplog/log          operation records, append-only
//! oplog/ckpt.<seq>   every ref as of operation <seq>
//! derived/index      where the records of derived values are, by key
//! ```
//!
//! The derived index is a cache with its own rules: see
//! [`DerivedIndexFile`].
//!
//! Segment names are 32 random lowercase hex digits. Files whose names start
//! with `tmp-` are still being written, or were left by a writer that died;
//! they are never read, and are removed once they are a minute old and
//! unlocked.
//!
//! **Storage version.** The second line of `format` says what a reader must
//! understand: a store without it has version 1 (records stored whole),
//! version 2 adds deltas and chunked blobs, and version 3
//! ([`STORAGE_VERSION`]) indexes of version 2 and merged indexes, which take
//! the place of the indexes they merge. A store of a later version, or with
//! a line this version does not know, is refused with [`Error::NewerStore`]
//! or [`Error::UnsupportedFormat`]. New stores start at the current version;
//! an older one is raised under an exclusive lock on `format.lock`, after
//! reading `format` again, only right before the first record or index that
//! needs it is written, or when a handle finds such records already there. A
//! journal entry or an index that holds its checksum but names a kind or a
//! codec this version does not know, sets a reserved field, or has a later
//! index version, was written by a newer version: the store is refused, so
//! nothing is ever written over it.
//!
//! **Object record**, starting at a multiple of 8 bytes:
//!
//! ```text
//! u32 sync = 0xB7A1D5E9 | u8 kind | u8 codec | u16 flags = 0
//! u32 stored_len | u64 raw_len | [32] id
//! [32] dictionary or base id, codecs 2 and 3 only
//! payload, stored_len bytes
//! u32 crc32c of everything from the sync word to the end of the payload
//! zero padding to a multiple of 8 bytes
//! ```
//!
//! `raw_len` is always the size of the object itself, so [`Objects::stat`]
//! answers from the header whatever the codec. After a crash, the next
//! writer appends at the next multiple of 8 after the end of the file, so
//! the remains of a cut append stay in place and scans step over them 8
//! bytes at a time.
//!
//! | Codec | Payload | Used for |
//! |---|---|---|
//! | 0 raw | the object as it is (`stored_len == raw_len`) | up to 64 bytes, or when zstd does not make it smaller |
//! | 1 zstd | one zstd frame, level 3 | anything else stored whole |
//! | 2 | reserved: zstd with a dictionary | never written |
//! | 3 delta | one zstd frame against the base's raw payload | blobs under 4 MiB, trees and derived objects, given a similar object |
//! | 4 chunked | the chunk list | blobs of 4 MiB or more |
//!
//! **Deltas (codec 3).** [`Objects::put_similar`] names an object the new
//! one is probably close to, such as the same path in the parent, or the
//! previous git map. A blob under 4 MiB, a tree or a derived object is then
//! compressed with zstd at level 3, with the raw payload of the base as a
//! reference prefix, a window that holds the base and the object together
//! (at most 2²⁷ bytes), and long-distance matching. The record names the
//! base in its header. The delta is kept only when it and the base id take
//! at most a quarter of the bytes of the object stored on its own;
//! otherwise the object is stored whole. Changesets are never deltas: they
//! share too little with their parents. Depth is always one: a base is
//! stored whole, and when the similar object is a delta, the new one is made
//! against that delta's base instead. Reading a delta reads its base and
//! applies one delta, never a chain; each handle keeps the payloads of the
//! bases it used last. A handle that knows several copies of one object
//! uses the best: stored whole, then delta, then chunked.
//!
//! **Chunked blobs (codec 4).** A blob of 4 MiB or more is cut with FastCDC
//! (the 2020 algorithm, normalisation level 1, the standard gear table)
//! into chunks of 16 KiB to 256 KiB, 64 KiB on average, or of 256 KiB to
//! 4 MiB, 1 MiB on average, for blobs of 256 MiB or more. Each chunk is a
//! blob of its own, whose id is the hash of its bytes, so equal chunks are
//! stored once across files and versions. A chunk is stored whole: raw when
//! its first 64 KiB do not compress below 0.9 of their size, else
//! compressed when that is smaller. The blob's own record holds its chunk
//! list:
//!
//! ```text
//! uv n | n × ([32] chunk id | uv chunk size)
//! ```
//!
//! The sizes add up to `raw_len`. Reads fetch the chunks in order, each
//! checked against its id, and check the whole against the blob's id, which
//! is the hash of the full content as always. [`Store::read_to`] streams a
//! chunked blob a chunk at a time, and [`Store::put_blob_from`] stores one
//! from a reader, hashing and cutting it as it comes in, with the chunk
//! sizes for the size the caller expects; how a blob is cut never changes
//! its id. A chunk list never names a chunked blob: a chunk is written whole
//! even when a chunked copy of the same bytes exists.
//!
//! **What depends on what.** A delta needs its base, and a chunked blob its
//! chunks. A copy that a writer finds in place of writing an object again
//! counts only together with what it needs, and the open segments of other
//! writers that hold those are flushed by the writer's next sync, as the
//! copy's own segment is. There is no garbage collection yet (`braid gc` is
//! planned): nothing is ever removed. When there is, it must never drop a
//! base while a delta needs it, nor a chunk while a chunk list names it.
//!
//! **Journal entry**, 64 bytes at a multiple of 64:
//!
//! ```text
//! [32] id | u64 offset | u32 stored_len | u8 kind | u8 codec | u16 0
//! u64 raw_len | u32 record crc | u32 crc32c of the 60 bytes before it
//! ```
//!
//! An entry is written after its record. Entries that fail their CRC are
//! skipped, and a new lease holder writes after the last valid one.
//!
//! **Index**, version 2, written once when a segment is sealed
//! (`<name>.idx`, one segment) or when indexes are merged (`<name>.midx`,
//! several):
//!
//! ```text
//! [8] "braididx" | u32 version = 2 | u32 0 | u64 count | u32 segments
//! u32 crc32c of the 28 bytes before it
//! segments × ([16] segment name | u64 segment length)
//! 256 × u32 fan-out: entry b counts the ids whose first byte is at most b
//! u32 crc32c of the segment table and the fan-out
//! count × 64-byte entries, ids strictly ascending:
//!     [32] id | u64 offset | u32 stored_len | u8 kind | u8 codec | u16 segment
//!     u64 raw_len | u32 record crc | u32 crc32c of the 60 bytes before it
//! ```
//!
//! An entry is a journal entry whose reserved field holds the number of its
//! segment in the table. A segment's name is the 32 hex digits of its file
//! name as 16 bytes, its length the size it had when it was sealed. Only
//! entries whose record checks out go in. Of several copies of an object, a
//! copy stored whole wins over a delta, a delta over a chunked blob, and the
//! first of equals wins. Every part has its own checksum, so readers take
//! in and check the head, then one fan-out bucket at a time. Stores of
//! storage versions 1 and 2 hold indexes of version 1, read whole:
//!
//! ```text
//! [8] "braididx" | u32 version = 1 | u32 0 | u64 count | u64 segment length
//! 256 × u32 fan-out: entry b counts the ids whose first byte is at most b
//! count × [32] ids, strictly ascending
//! count × (u64 offset | u32 stored_len | u8 kind | u8 codec | u16 0
//!          u64 raw_len | u32 record crc | u32 0), in id order
//! [32] BLAKE3 hash of everything before it
//! ```
//!
//! A segment is open while it has a journal and no index of its own. Once
//! merged, it has neither: merging removes its journal, if a sealer that
//! died left one, makes that durable, and only then removes its index.
//! There is no filter for fast misses yet.
//!
//! **Operation record**:
//!
//! ```text
//! u32 magic = 0xB7A10910 | u32 body_len | body | u32 body_len + 16
//! u32 crc32c of everything before it
//!
//! body:   u8 version = 1 | uv seq | sv milliseconds since the Unix epoch
//!         bytes agent | bytes description | uv seq undone, 0 for none
//!         uv n | n × (bytes ref name | u8 which | [32] old | [32] new)
//! ```
//!
//! `which` has bit 0 set when `old` is present and bit 1 when `new` is;
//! absent values take no bytes. The length at the end lets readers walk the
//! log backwards. Sequence numbers start at 1 and have no gaps. A record cut
//! short by a crash is skipped by readers and cut off by the next
//! transaction, but only when no whole record follows it.
//!
//! **Checkpoint**, written through a temporary file and a rename after every
//! `checkpoint_interval` operations; the newest two are kept:
//!
//! ```text
//! [8] "braidckp" | u32 version = 1 | u32 0 | u64 seq | u64 where record seq ends
//! uv n | n × (bytes ref name | [32] id), names strictly ascending
//! [32] BLAKE3 hash of everything before it
//! ```

#![forbid(unsafe_code)]

mod chunks;
mod delta;
mod derived;
mod error;
mod format_file;
mod fsck;
mod fsutil;
mod index;
mod journal;
mod oplog;
#[cfg(test)]
mod power_loss;
mod prefix;
mod record;
mod scan;
mod segments;
mod stats;

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub use braid_format::{Kind, ObjectId, Objects};

pub use chunks::CHUNKED_MIN;
pub use derived::{DERIVED_DIR, DerivedIndexFile};
pub use error::{Error, TransactError};
pub use fsck::{FsckReport, Problem};
pub use oplog::{Operation, RefUpdate, Refs};
pub use stats::{CodecStats, KindStats, Stats};

use error::io_error;
use oplog::OpLog;
use segments::Segments;

/// The first line of the `format` file: the object ids a store uses.
pub const OBJECT_FORMAT: &str = "object-format=blake3-dk-v1";

/// The storage version this version of braid writes: 3, where records may
/// be deltas (codec 3) and chunked blobs (codec 4), and indexes are of
/// version 2 and may be merged. It reads stores of every version up to this
/// one, and refuses later ones with [`Error::NewerStore`].
pub const STORAGE_VERSION: u32 = 3;

/// How a record stores its object's payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Codec {
    /// As it is.
    Raw,
    /// One zstd frame.
    Zstd,
    /// Reserved: zstd with a dictionary. Never written.
    Dictionary,
    /// A zstd frame against the raw payload of a base object.
    Delta,
    /// A blob's chunk list; the chunks are blobs of their own.
    Chunked,
}

/// How hard flushes push data to stable storage.
///
/// In both modes [`Objects::put`] only appends, [`Store::sync`] flushes the
/// segments and journals this handle wrote in one batch, and a transaction
/// flushes the objects its updates point at, then appends its record and
/// flushes the log before it returns. Order is what keeps the store
/// consistent after a crash or a power loss: an op log record is never
/// written before the objects it refers to are flushed, an index never
/// reaches the disk before the records it points at, and a file published
/// through a rename never before its contents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// The default for local work: may lose the last operations on power
    /// loss, never consistency. Files are flushed with `fdatasync`, their
    /// contents and the metadata needed to read them back, such as their
    /// length; on Apple systems a batch ends with a write barrier
    /// (`F_BARRIERFSYNC`) instead, which keeps the order of writes without
    /// waiting for the drive to write out its cache. Directories are flushed
    /// when a file is created, renamed or removed in them and something
    /// depends on it.
    #[default]
    Ordered,
    /// The default for servers: what a transaction acknowledges is on stable
    /// storage. Files are flushed with `fsync`, which also writes the rest of
    /// their metadata, or on Apple systems with `F_FULLFSYNC` once per
    /// batch, and every sync of segments also flushes their directory.
    Full,
}

/// How a store is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// How hard flushes push data to stable storage.
    pub durability: Durability,
    /// A segment is sealed once it holds this many bytes or more. A single
    /// object larger than this still goes into one segment.
    pub segment_size: u64,
    /// A segment is also sealed once its journal lists this many records,
    /// so that the journals every handle reads when it opens the store stay
    /// short. 0 for no limit.
    pub segment_records: u64,
    /// Once this many segment indexes are there, a writer merges them into
    /// a merged index at its next sync or transaction, so that lookups go
    /// through few indexes. 0 never merges.
    pub merge_threshold: usize,
    /// After this many operations the refs are saved in a checkpoint, so that
    /// opening the op log does not replay all of it. 0 writes none.
    pub checkpoint_interval: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            durability: Durability::Ordered,
            segment_size: 256 << 20,
            segment_records: 1 << 13,
            merge_threshold: 8,
            checkpoint_interval: 1000,
        }
    }
}

/// A store, opened by this process.
///
/// A `Store` is `Send` and `Sync`: threads can share one handle. Processes
/// each open their own. Dropping a handle releases its locks; anything it
/// wrote and did not sync is still visible to other processes, but may be
/// lost on power loss.
pub struct Store {
    root: PathBuf,
    options: Options,
    segments: Segments,
    oplog: OpLog,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("root", &self.root)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Creates a store at `root` if there is none, then opens it. Safe to run
    /// from several processes at once.
    pub fn init(root: impl AsRef<Path>, options: Options) -> Result<Self, Error> {
        let root = root.as_ref();
        let seg = root.join("seg");
        let oplog = root.join("oplog");
        for dir in [&seg, &oplog] {
            fs::create_dir_all(dir).map_err(io_error("create", dir))?;
        }
        for name in ["lock", "log"] {
            let path = oplog.join(name);
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(io_error("create", &path))?;
        }
        let durability = options.durability;
        fsutil::sync_dir(&seg, durability)?;
        fsutil::sync_dir(&oplog, durability)?;
        fsutil::sync_dir(root, durability)?;
        if let Some(parent) = root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fsutil::sync_dir(parent, durability)?;
        }
        let format = root.join("format");
        match fs::metadata(&format) {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Several processes may get here at once; they all publish
                // the same bytes. Opening reads what won.
                fsutil::publish_file(
                    root,
                    "format",
                    format_file::contents(STORAGE_VERSION).as_bytes(),
                    durability,
                    &[],
                )?;
            }
            Err(err) => return Err(io_error("read", &format)(err)),
        }
        Self::open(root, options)
    }

    /// Opens an existing store and reads the journals of its open segments.
    /// Indexes are read as lookups need them: opening reads no index entry,
    /// whatever the store holds.
    ///
    /// A store that needs a newer version of braid is refused with
    /// [`Error::NewerStore`]: its `format` file names a later storage
    /// version, or one of its journals holds an entry this version does not
    /// know; an index that does is refused when a lookup first reads it. A
    /// store of an earlier storage version is raised when this handle finds
    /// records that need a later one, and otherwise only when it is about to
    /// write the first record or index that does.
    pub fn open(root: impl AsRef<Path>, options: Options) -> Result<Self, Error> {
        let root = root.as_ref().to_path_buf();
        let storage = format_file::read(&root)?;
        let segments = Segments::open(&root, &options, storage)?;
        let oplog = OpLog::new(root.join("oplog"), &options);
        Ok(Self {
            root,
            options,
            segments,
            oplog,
        })
    }

    /// The store's directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The options the store was opened with.
    pub fn options(&self) -> &Options {
        &self.options
    }

    /// Makes everything this handle wrote durable: every segment it appended
    /// to, their journals, and the open segments of other writers whose
    /// copies it relied on instead of writing an object again.
    ///
    /// If this handle sealed enough segments that their indexes should be
    /// merged, it merges them next, outside any transaction. A merge that
    /// fails leaves the store as it was, and the sync succeeds all the same.
    pub fn sync(&self) -> Result<(), Error> {
        self.segments.sync()?;
        self.merge_if_due();
        Ok(())
    }

    /// Merges the indexes of sealed segments into one merged index, so that
    /// lookups go through one index for all of them. Writers do this by
    /// themselves once enough indexes pile up; this does it now, whatever
    /// their number. Returns whether there was anything to merge; `false`
    /// too while another process merges.
    pub fn merge_indexes(&self) -> Result<bool, Error> {
        oplog::not_in_transaction()?;
        self.segments.merge(true)
    }

    /// Merges indexes if this handle sealed enough segments, unless a
    /// transaction's closure is running on this thread. Best effort: a merge
    /// only makes lookups cheaper, and the next one tries again.
    fn merge_if_due(&self) {
        if oplog::not_in_transaction().is_ok() {
            let _ = self.segments.merge_if_due();
        }
    }

    /// Reads what other processes wrote or sealed since this handle last
    /// looked. Lookups that miss do this by themselves.
    pub fn rescan(&self) -> Result<(), Error> {
        self.segments.rescan()
    }

    /// The ids of the stored objects whose hex form starts with `prefix`,
    /// sorted, at most `limit` of them: how a short id is looked up.
    ///
    /// `prefix` is lowercase hex digits, at most 64 of them; anything else
    /// matches nothing, and the empty prefix matches every object. The ids
    /// come from the journals and indexes this handle knows of, and no record
    /// is read; when none matches, the directory is read again once, as a
    /// lookup by id does on a miss. Of each index, only the fan-out buckets
    /// the prefix falls in are read: with two digits or more, one.
    pub fn ids_with_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<ObjectId>, Error> {
        let Some(prefix) = prefix::HexPrefix::parse(prefix) else {
            return Ok(Vec::new());
        };
        let mut found = self.segments.ids_with_prefix(&prefix)?;
        if found.is_empty() {
            self.segments.rescan()?;
            found = self.segments.ids_with_prefix(&prefix)?;
        }
        found.truncate(limit);
        Ok(found)
    }

    /// Stores a blob read from `reader` without holding it in memory whole,
    /// and returns its id and size: the content is hashed and, from 4 MiB
    /// on, cut into chunks as it streams in. `size_hint` is the size the
    /// caller expects, such as a file's length: blobs of 256 MiB or more are
    /// cut into larger chunks. A stream that turns out longer or shorter is
    /// still stored exactly as read. Nothing is flushed: see [`Store::sync`].
    pub fn put_blob_from(
        &self,
        mut reader: impl Read,
        size_hint: Option<u64>,
    ) -> Result<(ObjectId, u64), Error> {
        self.segments.put_blob_from(&mut reader, size_hint)
    }

    /// Writes an object's payload to `out` and returns its kind and size,
    /// or `None` if it is not in the store. A chunked blob is read a chunk at
    /// a time, so a blob of any size never sits in memory whole.
    ///
    /// Every chunk is checked against its id before it is written, and the
    /// whole blob against its id after the last one: if this fails, what
    /// `out` received must be discarded.
    pub fn read_to(
        &self,
        id: &ObjectId,
        mut out: impl Write,
    ) -> Result<Option<(Kind, u64)>, Error> {
        self.segments.read_to(id, &mut out)
    }
}

impl Objects for Store {
    type Error = Error;

    /// Reads an object. The record's checksum, length and id are all checked,
    /// and so are the base of a delta and the chunks of a chunked blob:
    /// damaged data is an error, never a wrong answer. A miss rescans the
    /// directory once before answering `None`.
    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Error> {
        self.segments.get(id)
    }

    /// Appends an object, unless this handle already knows a copy: a blob
    /// of 4 MiB or more in chunks, anything else whole. Nothing is flushed:
    /// see [`Store::sync`].
    fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Error> {
        self.segments.put(kind, payload)
    }

    /// Appends an object like [`Objects::put`], as a delta against
    /// `similar` (or against its base, if it is a delta itself) when it is a
    /// blob under 4 MiB, a tree or a derived object and the delta takes at
    /// most a quarter of the bytes. Changesets are never deltas. A hint
    /// naming an object that is missing or cannot serve as a base is
    /// ignored.
    fn put_similar(
        &self,
        kind: Kind,
        payload: &[u8],
        similar: &ObjectId,
    ) -> Result<ObjectId, Error> {
        self.segments.put_similar(kind, payload, similar)
    }

    /// Whether an index or a journal lists the object. A miss rescans the
    /// directory once before answering `false`.
    fn contains(&self, id: &ObjectId) -> Result<bool, Error> {
        self.segments.contains(id)
    }

    /// The kind and payload size of an object, from its record's header: the
    /// payload is neither read nor decompressed, so this costs the same for
    /// any size. The header is checked against the object's index or journal
    /// entry; damage to the payload only shows on [`Objects::get`]. A miss
    /// rescans the directory once before answering `None`.
    fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, Error> {
        self.segments.stat(id)
    }
}
