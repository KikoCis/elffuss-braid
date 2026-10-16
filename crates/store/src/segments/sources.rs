// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The indexes of sealed segments, as one handle reads them.
//!
//! A sealed segment is covered by its own index, `seg/<name>.idx`, or by a
//! merged index, `seg/<name>.midx`, that a writer made out of several. A
//! handle registers an index when it lists the directory, and reads it as
//! lookups need it:
//!
//! - nothing of a segment's own index at first, since its name says what it
//!   covers; its head on first use;
//! - the head of a merged index right away, since only its head says what it
//!   covers;
//! - the entries of one fan-out bucket, the ids that share their first byte,
//!   the first time a lookup needs that bucket, checked entry by entry.
//!
//! An index of version 1, which older versions wrote, is read and checked
//! whole on first use. So is nothing else: what a handle reads follows the
//! lookups it makes, not the size of the store.
//!
//! Damage found in an index, at any point, is worked around as it always
//! was: the segments it covers are scanned, and their records stand for its
//! entries. fsck reports the damage.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use braid_format::ObjectId;

use super::{Segments, segment_path};
use crate::error::{Error, io_error};
use crate::fsutil::read_range;
use crate::index::{self, Covered, HEADER_LEN, Head, Header, IndexError, Numbered};
use crate::journal::{ENTRY_LEN, Entry};
use crate::prefix::HexPrefix;
use crate::{record, scan};

/// What kind of index a source is, in the order lookups try them: a
/// segment's own indexes are the youngest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SourceKind {
    /// A segment's own index.
    Own,
    /// A sealed segment that no readable index covers: its records are
    /// scanned.
    Scan,
    /// A merged index of several segments.
    Merged,
}

/// Why a lookup through an index failed.
#[derive(Debug)]
pub(crate) enum Lookup {
    /// The index file is gone: merged into another one since the directory
    /// was listed. Rescanning finds what replaced it.
    Gone(Error),
    /// Anything else.
    Failed(Error),
}

impl Lookup {
    pub fn into_error(self) -> Error {
        match self {
            Lookup::Gone(err) | Lookup::Failed(err) => err,
        }
    }
}

impl From<Error> for Lookup {
    fn from(err: Error) -> Self {
        Lookup::Failed(err)
    }
}

/// An index of one or more sealed segments.
#[derive(Debug)]
pub(crate) struct Source {
    /// Its file name in `seg/`.
    pub file: String,
    pub kind: SourceKind,
    /// The segments it covers, by their number in it: their names, and their
    /// numbers in the handle's catalog once registered.
    pub covered: Vec<(String, u32)>,
    path: PathBuf,
    /// Its entries, as far as they are read.
    content: OnceLock<Content>,
    /// Its entries made again from the records, once damage was found in it.
    rebuilt: OnceLock<Vec<(Entry, u16)>>,
    /// Held while the head is read or the entries are made again.
    loading: Mutex<()>,
    /// Set when its file turned out to be gone before it was read.
    gone: AtomicBool,
}

/// The 256 fan-out buckets of an index, each read on first use.
type Buckets = Box<[OnceLock<Box<[Numbered]>>]>;

/// What a handle has of an index.
#[derive(Debug)]
enum Content {
    /// A version 2 index: its file and head, and the buckets read so far.
    Lazy {
        file: File,
        head: Head,
        buckets: Buckets,
    },
    /// Every entry, sorted by id: a version 1 index, or the index this
    /// handle just wrote.
    Whole(Vec<(Entry, u16)>),
}

impl Source {
    /// A segment's own index. `known` is what it covers and its entries,
    /// when this handle just wrote it: then it is never read.
    pub fn own(dir: &Path, name: &str, known: Option<(Covered, Vec<Entry>)>) -> Self {
        let file = format!("{name}.idx");
        let source = Source {
            path: dir.join(&file),
            file,
            kind: SourceKind::Own,
            covered: vec![(name.to_owned(), 0)],
            content: OnceLock::new(),
            rebuilt: OnceLock::new(),
            loading: Mutex::new(()),
            gone: AtomicBool::new(false),
        };
        if let Some((_, entries)) = known {
            let entries = entries.into_iter().map(|entry| (entry, 0)).collect();
            let _ = source.content.set(Content::Whole(entries));
        }
        source
    }

    /// A sealed segment that no readable index covers, read by scanning it.
    pub fn scan(name: &str) -> Self {
        Source {
            path: PathBuf::new(),
            file: format!("{name}.seg"),
            kind: SourceKind::Scan,
            covered: vec![(name.to_owned(), 0)],
            content: OnceLock::new(),
            rebuilt: OnceLock::new(),
            loading: Mutex::new(()),
            gone: AtomicBool::new(false),
        }
    }

    /// Opens the merged index `file` in `dir` and reads its head: `None` if
    /// it is gone, the reason if it cannot be read. A merged index a newer
    /// version wrote is an error.
    pub fn open_merged(dir: &Path, file: &str) -> Result<Option<Result<Self, IndexError>>, Error> {
        let path = dir.join(file);
        let opened = match File::open(&path) {
            Ok(opened) => opened,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_error("open", &path)(err)),
        };
        let head = match read_head(&opened, &path).map_err(Lookup::into_error)? {
            Ok(Some(head)) => head,
            Ok(None) => {
                return Ok(Some(Err(IndexError::Damaged(
                    "a merged index of version 1",
                ))));
            }
            Err(IndexError::Newer(detail)) => return Err(newer(&path, detail)),
            Err(damaged) => return Ok(Some(Err(damaged))),
        };
        let source = Source {
            covered: head
                .covered
                .iter()
                .map(|segment| (segment.name.clone(), 0))
                .collect(),
            content: OnceLock::new(),
            rebuilt: OnceLock::new(),
            loading: Mutex::new(()),
            gone: AtomicBool::new(false),
            path,
            file: file.to_owned(),
            kind: SourceKind::Merged,
        };
        let _ = source.content.set(Content::Lazy {
            file: opened,
            head,
            buckets: buckets(),
        });
        Ok(Some(Ok(source)))
    }

    /// Whether its file turned out to be gone before it was read: merged
    /// into another index since it was registered.
    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// How good a source is at serving a segment it covers: a merged index
    /// over a segment's own, and the merged index that covers more, which is
    /// the newer, over one that covers less.
    pub fn rank(&self) -> (u8, usize, &str) {
        let kind = match self.kind {
            SourceKind::Scan => 0,
            SourceKind::Own => 1,
            SourceKind::Merged => 2,
        };
        (kind, self.covered.len(), &self.file)
    }

    /// The entry for `id`, with the number of its segment, if this index
    /// has one.
    pub fn find(&self, id: &ObjectId, segments: &Segments) -> Result<Option<(Entry, u16)>, Lookup> {
        let entries = self.bucket(id.as_bytes()[0], segments)?;
        Ok(entries
            .binary_search_by(|(entry, _)| entry.id.cmp(id))
            .ok()
            .map(|at| entries[at]))
    }

    /// The entries whose ids start with `prefix`.
    pub fn with_prefix(
        &self,
        prefix: &HexPrefix,
        segments: &Segments,
    ) -> Result<Vec<(Entry, u16)>, Lookup> {
        let mut out = Vec::new();
        for byte in prefix.first_bytes() {
            out.extend(
                self.bucket(byte, segments)?
                    .iter()
                    .filter(|(entry, _)| prefix.matches(&entry.id)),
            );
        }
        Ok(out)
    }

    /// Every entry, in id order.
    pub fn all(&self, segments: &Segments) -> Result<Vec<(Entry, u16)>, Lookup> {
        let mut out = Vec::new();
        for byte in 0..=255u8 {
            out.extend_from_slice(self.bucket(byte, segments)?);
        }
        Ok(out)
    }

    /// The entries whose ids start with `byte`, sorted by id.
    fn bucket(&self, byte: u8, segments: &Segments) -> Result<&[(Entry, u16)], Lookup> {
        if let Some(rebuilt) = self.rebuilt.get() {
            return Ok(slice_of(rebuilt, byte));
        }
        let content = match self.content(segments)? {
            Some(content) => content,
            None => return self.rebuild(segments).map(|all| slice_of(all, byte)),
        };
        match content {
            Content::Whole(all) => Ok(slice_of(all, byte)),
            Content::Lazy {
                file,
                head,
                buckets,
            } => {
                let slot = &buckets[usize::from(byte)];
                if let Some(entries) = slot.get() {
                    return Ok(entries);
                }
                let range = head.bucket(byte);
                let bytes = read_range(
                    file,
                    head.entry_offset(range.start),
                    (range.end - range.start) * ENTRY_LEN,
                );
                let checked = match bytes {
                    Ok(bytes) => index::bucket(head, byte, &bytes),
                    Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                        Err(IndexError::Damaged("index file cut short"))
                    }
                    Err(err) => return Err(io_error("read", &self.path)(err).into()),
                };
                match checked {
                    Ok(entries) => {
                        // Two threads may read the same bucket: one keeps it.
                        let _ = slot.set(entries.into_boxed_slice());
                        Ok(slot.get().map_or(&[], |entries| entries))
                    }
                    Err(IndexError::Newer(detail)) => Err(newer(&self.path, detail).into()),
                    Err(IndexError::Damaged(_)) => {
                        self.rebuild(segments).map(|all| slice_of(all, byte))
                    }
                }
            }
        }
    }

    /// What this handle has of the index, its head read on first use.
    /// `None` when the index cannot be used: its records are scanned
    /// instead.
    fn content(&self, segments: &Segments) -> Result<Option<&Content>, Lookup> {
        if let Some(content) = self.content.get() {
            return Ok(Some(content));
        }
        if self.kind == SourceKind::Scan {
            return Ok(None);
        }
        let _loading = self.loading.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(content) = self.content.get() {
            return Ok(Some(content));
        }
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.gone.store(true, Ordering::Release);
                return Err(Lookup::Gone(io_error("open", &self.path)(err)));
            }
            Err(err) => return Err(io_error("open", &self.path)(err).into()),
        };
        let content = match read_head(&file, &self.path)? {
            Ok(Some(head))
                if head.covered.len() == 1 && head.covered[0].name == self.covered[0].0 =>
            {
                Content::Lazy {
                    file,
                    head,
                    buckets: buckets(),
                }
            }
            Ok(Some(_)) => return Ok(None),
            Ok(None) => {
                let bytes = fs::read(&self.path).map_err(io_error("read", &self.path))?;
                match index::decode(&bytes, Some(&self.covered[0].0)) {
                    Ok(decoded) => {
                        // Written by a version from before storage versions,
                        // it may hold records the store's format file does
                        // not declare.
                        let needed = decoded
                            .entries
                            .iter()
                            .map(|(entry, _)| record::storage_version(entry.codec))
                            .max()
                            .unwrap_or(1);
                        segments.need_storage(needed)?;
                        Content::Whole(decoded.entries)
                    }
                    Err(IndexError::Newer(detail)) => {
                        return Err(newer(&self.path, detail).into());
                    }
                    Err(IndexError::Damaged(_)) => return Ok(None),
                }
            }
            Err(IndexError::Newer(detail)) => return Err(newer(&self.path, detail).into()),
            Err(IndexError::Damaged(_)) => return Ok(None),
        };
        let _ = self.content.set(content);
        Ok(self.content.get())
    }

    /// Makes the entries again from the records of the covered segments,
    /// which describe themselves: for an index found damaged, or a segment
    /// no readable index covers. Of several copies of an object, the best
    /// ranked wins, and the first of those.
    fn rebuild(&self, segments: &Segments) -> Result<&[(Entry, u16)], Lookup> {
        let _loading = self.loading.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(rebuilt) = self.rebuilt.get() {
            return Ok(rebuilt);
        }
        let mut found: Vec<(Entry, u16, usize)> = Vec::new();
        for (number, (name, _)) in self.covered.iter().enumerate() {
            let path = segment_path(segments.dir(), name, "seg");
            let file = File::open(&path).map_err(io_error("open", &path))?;
            let len = file.metadata().map_err(io_error("read", &path))?.len();
            let local = u16::try_from(number).unwrap_or(u16::MAX);
            scan::scan(&file, len, |offset, record| {
                let entry = Entry::new(offset, &record.header, record.crc);
                found.push((entry, local, found.len()));
            })
            .map_err(io_error("read", &path))?;
        }
        found.sort_by_key(|(entry, _, order)| (entry.id, record::rank(entry.codec), *order));
        found.dedup_by_key(|(entry, _, _)| entry.id);
        let entries = found
            .into_iter()
            .map(|(entry, local, _)| (entry, local))
            .collect();
        let _ = self.rebuilt.set(entries);
        Ok(self.rebuilt.get().map_or(&[], Vec::as_slice))
    }
}

#[cfg(test)]
impl Source {
    /// Whether its head is read, and how many of its buckets.
    pub fn read_so_far(&self) -> (bool, usize) {
        match self.content.get() {
            None => (false, 0),
            Some(Content::Whole(_)) => (true, 256),
            Some(Content::Lazy { buckets, .. }) => (
                true,
                buckets
                    .iter()
                    .filter(|bucket| bucket.get().is_some())
                    .count(),
            ),
        }
    }
}

/// Reads the head of an index: `Ok(None)` for a version 1 index, which has
/// none of its own.
fn read_head(file: &File, path: &Path) -> Result<Result<Option<Head>, IndexError>, Lookup> {
    let len = file.metadata().map_err(io_error("read", path))?.len();
    let read = |at: u64, n: u64| match read_range(file, at, n) {
        Ok(bytes) => Ok(Ok(bytes)),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
            Ok(Err(IndexError::Damaged("index file cut short")))
        }
        Err(err) => Err(Lookup::Failed(io_error("read", path)(err))),
    };
    let first = match read(0, HEADER_LEN as u64)? {
        Ok(first) => first,
        Err(damaged) => return Ok(Err(damaged)),
    };
    let segments = match index::header(&first) {
        Ok(Header::V1) => return Ok(Ok(None)),
        Ok(Header::V2 { segments, .. }) => segments,
        Err(err) => return Ok(Err(err)),
    };
    let bytes = match read(0, Header::head_len(segments) as u64)? {
        Ok(bytes) => bytes,
        Err(damaged) => return Ok(Err(damaged)),
    };
    Ok(index::head(&bytes).and_then(|head| {
        if head.file_len() == len {
            Ok(Some(head))
        } else {
            Err(IndexError::Damaged(
                "index length does not match its entry count",
            ))
        }
    }))
}

/// Empty slots for the 256 buckets of an index.
fn buckets() -> Buckets {
    (0..256).map(|_| OnceLock::new()).collect()
}

/// The entries of `all`, sorted by id, whose ids start with `byte`.
fn slice_of(all: &[(Entry, u16)], byte: u8) -> &[(Entry, u16)] {
    let start = all.partition_point(|(entry, _)| entry.id.as_bytes()[0] < byte);
    let end = all.partition_point(|(entry, _)| entry.id.as_bytes()[0] <= byte);
    &all[start..end]
}

/// The error for an index a newer version of braid wrote.
fn newer(path: &Path, detail: &str) -> Error {
    Error::NewerStore {
        path: path.to_path_buf(),
        detail: format!("it holds {detail}"),
    }
}
