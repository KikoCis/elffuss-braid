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
//! - A segment past the size threshold is sealed: flushed, given a sorted
//!   index through a temporary file and a rename, and its journal removed. It
//!   is never written again.
//! - Each handle maps ids to records from the indexes and journals, and on a
//!   miss reads the directory again once. Short ids, hex prefixes of ids,
//!   are looked up in the same map ([`Store::ids_with_prefix`]).
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
//! format             object-format=blake3-dk-v1
//! seg/<name>.seg     object records, append-only
//! seg/<name>.jrn     journal of a segment that is still open
//! seg/<name>.idx     sorted index of a sealed segment
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
//! Codec 0 is raw (`stored_len == raw_len`), 1 is one zstd frame; 2 and 3 are
//! reserved and not decoded by this version. Payloads up to 64 bytes are
//! stored raw, larger ones compressed at zstd level 3 when that makes them
//! smaller. After a crash, the next writer appends at the next multiple of 8
//! after the end of the file, so the remains of a cut append stay in place
//! and scans step over them 8 bytes at a time.
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
//! **Index**, written once when the segment is sealed:
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
//! Only entries whose record checks out go in, the first copy of an object
//! wins, and the segment length is the size of the segment when it was
//! sealed. There is no filter for fast misses yet, and no merged indexes.
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

mod derived;
mod error;
mod fsck;
mod fsutil;
mod index;
mod journal;
mod oplog;
mod prefix;
mod record;
mod scan;
mod segments;

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub use braid_format::{Kind, ObjectId, Objects};

pub use derived::{DERIVED_DIR, DerivedIndexFile};
pub use error::{Error, TransactError};
pub use fsck::{FsckReport, Problem};
pub use oplog::{Operation, RefUpdate, Refs};

use error::io_error;
use oplog::OpLog;
use segments::Segments;

/// The contents of the `format` file, without its final newline.
pub const OBJECT_FORMAT: &str = "object-format=blake3-dk-v1";

/// How hard flushes push data to stable storage.
///
/// In both modes [`Objects::put`] only appends, [`Store::sync`] flushes
/// segments before their journals, and a transaction flushes the objects its
/// updates point at, then appends its record and flushes the log before it
/// returns. Order is what keeps the store consistent after a crash or a power
/// loss: a journal entry is never durable before its record, and an op log
/// record never before the objects it refers to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// The default for local work. Files are flushed with `fdatasync`: their
    /// contents and the metadata needed to read them back, such as their
    /// length. Directories are flushed when a file is created in them or
    /// renamed into them.
    #[default]
    Ordered,
    /// The default for servers. Files are flushed with `fsync`, which also
    /// writes the rest of their metadata, and every sync of segments also
    /// flushes their directory.
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
    /// After this many operations the refs are saved in a checkpoint, so that
    /// opening the op log does not replay all of it. 0 writes none.
    pub checkpoint_interval: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            durability: Durability::Ordered,
            segment_size: 256 << 20,
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
        fsutil::sync_dir(&seg)?;
        fsutil::sync_dir(&oplog)?;
        fsutil::sync_dir(root)?;
        if let Some(parent) = root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fsutil::sync_dir(parent)?;
        }
        let format = root.join("format");
        match fs::metadata(&format) {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Several processes may get here at once; they all publish
                // the same bytes.
                fsutil::publish_file(root, "format", format!("{OBJECT_FORMAT}\n").as_bytes())?;
            }
            Err(err) => return Err(io_error("read", &format)(err)),
        }
        Self::open(root, options)
    }

    /// Opens an existing store and reads every segment index and journal.
    pub fn open(root: impl AsRef<Path>, options: Options) -> Result<Self, Error> {
        let root = root.as_ref().to_path_buf();
        check_format(&root)?;
        let segments = Segments::open(root.join("seg"), &options)?;
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
    /// to, then their journals, and the open segments of other writers whose
    /// copies it relied on instead of writing an object again.
    pub fn sync(&self) -> Result<(), Error> {
        self.segments.sync()
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
    /// come from the indexes and journals this handle has read, and no record
    /// is read; when none matches, the directory is read again once, as a
    /// lookup by id does on a miss. The cost follows the number of objects
    /// the handle knows of.
    pub fn ids_with_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<ObjectId>, Error> {
        let Some(prefix) = prefix::HexPrefix::parse(prefix) else {
            return Ok(Vec::new());
        };
        let mut found = self.segments.ids_where(|id| prefix.matches(id));
        if found.is_empty() {
            self.segments.rescan()?;
            found = self.segments.ids_where(|id| prefix.matches(id));
        }
        found.sort_unstable();
        found.truncate(limit);
        Ok(found)
    }
}

impl Objects for Store {
    type Error = Error;

    /// Reads an object. The record's checksum, length and id are all checked:
    /// damaged data is an error, never a wrong answer. A miss rescans the
    /// directory once before answering `None`.
    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Error> {
        self.segments.get(id)
    }

    /// Appends an object, unless this handle already knows a copy. Nothing
    /// is flushed: see [`Store::sync`].
    fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Error> {
        self.segments.put(kind, payload)
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

/// Checks that `root` holds a store in a format this version understands.
pub(crate) fn check_format(root: &Path) -> Result<(), Error> {
    let path = root.join("format");
    let text = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(Error::NotAStore {
                path: root.to_path_buf(),
            });
        }
        Err(err) => return Err(io_error("read", &path)(err)),
    };
    let text = String::from_utf8_lossy(&text);
    let line = text.strip_suffix('\n').unwrap_or(&text);
    if line != OBJECT_FORMAT {
        return Err(Error::UnsupportedFormat {
            found: line.to_owned(),
        });
    }
    Ok(())
}
