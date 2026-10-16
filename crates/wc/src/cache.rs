// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The stat cache: what each file of the working copy hashed to, and the
//! metadata it had then, so that unchanged files are not read again; and,
//! for each directory, what the walk found in it and the base tree it was
//! compared with, so that a directory that did not change is neither listed
//! nor compared with its tree again.
//!
//! A file's record vouches for its blob only when the file's modification
//! time, size, inode number and type are all still the same, and its
//! modification time is strictly older than the time the cache's scan
//! started, read from the workspace's own file system. A file written in the
//! same instant as the scan, or later, may have changed without its metadata
//! showing it, so it is hashed again. Symlinks, and files that changed while
//! they were read, are recorded with their blob but never vouched for.
//!
//! A directory's record lists the entries the walk used: the files,
//! symlinks and gitlinks that count, and the subdirectories it went into. It
//! stands for the directory's listing while the directory's stamp (its
//! modification and status change times and its inode number) is the same,
//! both times strictly older than the cache's scan, under the same ignore
//! rules and against the same base tree: the same guard as for files, since
//! adding, removing or renaming an entry changes both times.
//!
//! A directory is clean when its own entries were exactly those of the base
//! tree it was compared with: each file, symlink and gitlink of that tree
//! there with the same content, each of its subdirectories there as a
//! directory, and nothing else counted. Its record then lists that tree's
//! entries, and while the base has the same tree at that path and the
//! directory's entries still match the record, nothing in the directory
//! differs from the base, and the tree is not read.
//!
//! Records have a fixed width and are sorted, so that loading the cache is
//! reading it: a directory is found by binary search on its path, and its
//! entries lie together, by name.
//!
//! ```text
//! [8] "braidwsc" | u32 version = 3 | u32 0
//! i64 seconds | u32 nanoseconds            when the scan started
//! u32 directories | u32 entries | u32 name bytes
//! directories × 96 bytes, by path, strictly ascending
//! entries × 72 bytes, each directory's together, by name, strictly ascending
//! name bytes
//! u32 CRC-32C of everything before it
//!
//! directory: u32 names offset | u32 path length | u32 first entry
//!            u32 entries | [32] base tree, zero if none | u32 flags
//!            u32 mtime nanoseconds | i64 mtime seconds | i64 ctime seconds
//!            u32 ctime nanoseconds | u32 names length | u64 inode
//!            u64 ignore rules
//! entry:     u32 name offset | u32 name length | i64 mtime seconds
//!            u32 mtime nanoseconds | u8 kind | u8 flags (1 vouches) | u16 0
//!            u64 size | u64 inode | [32] blob
//! ```
//!
//! A directory's flags: 1 it has a base tree, 2 it is clean, 4 its stamp is
//! recorded, 8 it is ignored, 16 it has a `.braidignore`, 32 a `.gitignore`.
//! Its ignore rules are a digest of the ignore files in force for its
//! entries. An entry's kind is 0 for a regular file, 1 an executable, 2 a
//! symlink, 3 a gitlink of the base whose directory is there, 4 a
//! subdirectory that the base has, with its tree as the blob, and 5 a
//! subdirectory that the base does not have. Entries of kinds 3 to 5 have no
//! metadata, and of them only subdirectories of the base have a blob.
//!
//! Each directory has its own block of name bytes, its path and then its
//! entries' names: a directory's names offset counts from the start of the
//! name bytes, and its entries' name offsets from the start of its block, so
//! that the records of a directory that did not change are copied as they
//! are. The cache is only a cache: if it is missing or damaged, everything is
//! read again. It is written whole, through a scratch file and a rename, and
//! only when a scan found something its records do not say.

use std::fs;
use std::io::Write;
use std::path::Path;

use braid_format::ObjectId;

use crate::error::Error;
use crate::fsprobe::random_hex;
use crate::safe::{DirStamp, Stat, StatKind};

const MAGIC: &[u8; 8] = b"braidwsc";
const VERSION: u32 = 3;

const HEADER_LEN: usize = 40;
const DIR_LEN: usize = 96;
const FILE_LEN: usize = 72;
const CHECK_LEN: usize = 4;

const DIR_HAS_BASE: u32 = 1;
const DIR_CLEAN: u32 = 2;
const DIR_STAMPED: u32 = 4;
const DIR_IGNORED: u32 = 8;
const DIR_IGNORE_FILES: u32 = 16;
const FILE_VOUCHES: u8 = 1;

/// A regular file.
pub(crate) const KIND_FILE: u8 = 0;
/// An executable file.
pub(crate) const KIND_EXECUTABLE: u8 = 1;
/// A symlink.
pub(crate) const KIND_SYMLINK: u8 = 2;
/// A gitlink of the base whose directory is there.
pub(crate) const KIND_GITLINK: u8 = 3;
/// A subdirectory that the base has: its tree is the blob.
pub(crate) const KIND_TREE: u8 = 4;
/// A subdirectory that the base does not have.
pub(crate) const KIND_SUBDIR: u8 = 5;

/// The file the cache lives in, inside the workspace's state directory.
pub(crate) const FILE: &str = "stat";

const ZERO: ObjectId = ObjectId::from_bytes([0; 32]);

/// The kind code of a file with this metadata, or `None` for something that
/// is not a file or a symlink.
pub(crate) fn kind_code(stat: &Stat) -> Option<u8> {
    match stat.kind {
        StatKind::File if stat.executable => Some(KIND_EXECUTABLE),
        StatKind::File => Some(KIND_FILE),
        StatKind::Symlink => Some(KIND_SYMLINK),
        StatKind::Dir | StatKind::Other => None,
    }
}

/// The cache of one workspace, as loaded: the file's bytes, checked once,
/// read in place.
#[derive(Debug, Default)]
pub(crate) struct Cache {
    bytes: Vec<u8>,
    time: (i64, u32),
    dirs: usize,
    files: usize,
}

impl Cache {
    /// The cache in `state_dir`, or an empty one if there is none or it is
    /// damaged.
    pub(crate) fn load(state_dir: &Path) -> Cache {
        fs::read(state_dir.join(FILE))
            .ok()
            .and_then(Cache::decode)
            .unwrap_or_default()
    }

    /// When the scan that wrote it started, by the file system's clock.
    pub(crate) fn time(&self) -> (i64, u32) {
        self.time
    }

    /// How many directories it records.
    pub(crate) fn dir_count(&self) -> usize {
        self.dirs
    }

    /// The record of the directory at `path` (empty for the root).
    pub(crate) fn dir(&self, path: &[u8]) -> Option<CachedDir<'_>> {
        let (mut low, mut high) = (0, self.dirs);
        while low < high {
            let middle = low + (high - low) / 2;
            let dir = CachedDir {
                cache: self,
                at: HEADER_LEN + middle * DIR_LEN,
            };
            match dir.path().cmp(path) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(dir),
            }
        }
        None
    }

    fn names_at(&self) -> usize {
        HEADER_LEN + self.dirs * DIR_LEN + self.files * FILE_LEN
    }

    /// The bytes of entry `index`'s record.
    fn entry(&self, index: usize) -> &[u8] {
        let at = HEADER_LEN + self.dirs * DIR_LEN + index * FILE_LEN;
        &self.bytes[at..at + FILE_LEN]
    }

    /// Checks the bytes of a cache file: the checksum, the header, and that
    /// every record lies within the file. The order of records is not
    /// checked: records out of order can only fail to be found.
    fn decode(bytes: Vec<u8>) -> Option<Cache> {
        if bytes.len() < HEADER_LEN + CHECK_LEN {
            return None;
        }
        let (body, check) = bytes.split_at(bytes.len() - CHECK_LEN);
        if crc32c::crc32c(body).to_le_bytes()[..] != *check {
            return None;
        }
        if body[..8] != MAGIC[..] || u32_at(body, 8) != VERSION {
            return None;
        }
        let time = (i64_at(body, 16), u32_at(body, 24));
        let dirs = u32_at(body, 28) as usize;
        let files = u32_at(body, 32) as usize;
        let names = u32_at(body, 36) as usize;
        let expected = HEADER_LEN
            .checked_add(dirs.checked_mul(DIR_LEN)?)?
            .checked_add(files.checked_mul(FILE_LEN)?)?
            .checked_add(names)?;
        if expected != body.len() || time.1 >= 1_000_000_000 {
            return None;
        }
        let within = |offset: u32, len: u32, limit: usize| {
            (offset as usize)
                .checked_add(len as usize)
                .is_some_and(|end| end <= limit)
        };
        for index in 0..dirs {
            let at = HEADER_LEN + index * DIR_LEN;
            let record = &body[at..at + DIR_LEN];
            let block = u32_at(record, 76);
            let first = u32_at(record, 8) as usize;
            let count = u32_at(record, 12) as usize;
            if !within(u32_at(record, 0), block, names)
                || u32_at(record, 4) > block
                || first.checked_add(count)? > files
            {
                return None;
            }
            for entry in first..first + count {
                let at = HEADER_LEN + dirs * DIR_LEN + entry * FILE_LEN;
                let entry = &body[at..at + FILE_LEN];
                if !within(u32_at(entry, 0), u32_at(entry, 4), block as usize) {
                    return None;
                }
            }
        }
        Some(Cache {
            bytes,
            time,
            dirs,
            files,
        })
    }
}

/// The record of one directory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CachedDir<'c> {
    cache: &'c Cache,
    at: usize,
}

impl<'c> CachedDir<'c> {
    fn record(&self) -> &'c [u8] {
        &self.cache.bytes[self.at..self.at + DIR_LEN]
    }

    /// The directory's block of name bytes: its path, then its entries'
    /// names.
    fn block(&self) -> &'c [u8] {
        let record = self.record();
        let start = self.cache.names_at() + u32_at(record, 0) as usize;
        &self.cache.bytes[start..start + u32_at(record, 76) as usize]
    }

    /// The directory's path.
    pub(crate) fn path(&self) -> &'c [u8] {
        &self.block()[..u32_at(self.record(), 4) as usize]
    }

    fn entry(&self, index: usize) -> CachedFile<'c> {
        let bytes = self.cache.entry(index);
        let (offset, len) = (u32_at(bytes, 0) as usize, u32_at(bytes, 4) as usize);
        CachedFile {
            name: &self.block()[offset..offset + len],
            mtime: (i64_at(bytes, 8), u32_at(bytes, 16)),
            kind: bytes[20],
            vouches: bytes[21] & FILE_VOUCHES != 0,
            size: u64_at(bytes, 24),
            ino: u64_at(bytes, 32),
            blob: id_at(bytes, 40),
        }
    }

    fn flags(&self) -> u32 {
        u32_at(self.record(), 48)
    }

    /// The base tree the directory was compared with, if it had one.
    pub(crate) fn base(&self) -> Option<ObjectId> {
        (self.flags() & DIR_HAS_BASE != 0).then(|| id_at(self.record(), 16))
    }

    /// Whether its entries were exactly those of that tree.
    pub(crate) fn clean(&self) -> bool {
        self.flags() & DIR_CLEAN != 0
    }

    /// Its stamp when it was listed, if it was.
    pub(crate) fn stamp(&self) -> Option<DirStamp> {
        let record = self.record();
        (self.flags() & DIR_STAMPED != 0).then(|| DirStamp {
            mtime: (i64_at(record, 56), u32_at(record, 52)),
            ctime: (i64_at(record, 64), u32_at(record, 72)),
            ino: u64_at(record, 80),
        })
    }

    /// Whether it was ignored: only what the base had counted inside.
    pub(crate) fn ignored(&self) -> bool {
        self.flags() & DIR_IGNORED != 0
    }

    /// The digest of the ignore rules in force for its entries.
    pub(crate) fn rules(&self) -> u64 {
        u64_at(self.record(), 88)
    }

    /// Which ignore files it had: bit 0 for `.braidignore`, 1 for
    /// `.gitignore`.
    pub(crate) fn ignore_files(&self) -> u8 {
        ((self.flags() / DIR_IGNORE_FILES) & 3) as u8
    }

    fn range(&self) -> std::ops::Range<usize> {
        let record = self.record();
        let first = u32_at(record, 8) as usize;
        first..first + u32_at(record, 12) as usize
    }

    /// The bytes of its entries' records.
    fn entries(&self) -> &'c [u8] {
        let range = self.range();
        let start = HEADER_LEN + self.cache.dirs * DIR_LEN + range.start * FILE_LEN;
        &self.cache.bytes[start..start + range.len() * FILE_LEN]
    }

    /// How many entries it recorded.
    pub(crate) fn file_count(&self) -> usize {
        self.range().len()
    }

    /// Its entries' records, by name.
    pub(crate) fn files(&self) -> impl Iterator<Item = CachedFile<'c>> + 'c {
        let dir = *self;
        self.range().map(move |index| dir.entry(index))
    }

    /// The record of the entry `name`.
    pub(crate) fn file(&self, name: &[u8]) -> Option<CachedFile<'c>> {
        let range = self.range();
        let (mut low, mut high) = (range.start, range.end);
        while low < high {
            let middle = low + (high - low) / 2;
            let file = self.entry(middle);
            match file.name.cmp(name) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(file),
            }
        }
        None
    }
}

/// The record of one entry of a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CachedFile<'c> {
    pub name: &'c [u8],
    pub mtime: (i64, u32),
    pub size: u64,
    pub ino: u64,
    pub kind: u8,
    /// Whether the record may vouch for the file by its metadata.
    pub vouches: bool,
    pub blob: ObjectId,
}

impl CachedFile<'_> {
    /// The blob of the file, if this record may be trusted for a file that
    /// now has the metadata `stat`, in a cache whose scan started at `time`.
    pub(crate) fn vouch(&self, stat: &Stat, time: (i64, u32)) -> Option<ObjectId> {
        let same = self.mtime == stat.mtime
            && self.size == stat.size
            && self.ino == stat.ino
            && Some(self.kind) == kind_code(stat);
        (self.vouches && same && self.mtime < time).then_some(self.blob)
    }
}

/// A directory's record, as a scan makes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirRecord {
    pub path: Vec<u8>,
    pub base: Option<ObjectId>,
    pub clean: bool,
    /// Its stamp, taken before it was listed; `None` where the system gives
    /// none.
    pub stamp: Option<DirStamp>,
    pub ignored: bool,
    pub rules: u64,
    /// See [`CachedDir::ignore_files`].
    pub ignore_files: u8,
    /// By name, strictly ascending.
    pub files: Vec<FileRecord>,
}

impl DirRecord {
    fn flags(&self) -> u32 {
        let bit = |set: bool, bit: u32| if set { bit } else { 0 };
        bit(self.base.is_some(), DIR_HAS_BASE)
            | bit(self.clean, DIR_CLEAN)
            | bit(self.stamp.is_some(), DIR_STAMPED)
            | bit(self.ignored, DIR_IGNORED)
            | (u32::from(self.ignore_files & 3) * DIR_IGNORE_FILES)
    }

    /// Whether `cached` says exactly the same.
    pub(crate) fn same_as(&self, cached: &CachedDir<'_>) -> bool {
        self.flags() == cached.flags()
            && self.base == cached.base()
            && self.stamp == cached.stamp()
            && self.rules == cached.rules()
            && self.files.len() == cached.file_count()
            && self
                .files
                .iter()
                .zip(cached.files())
                .all(|(new, old)| new.view() == old)
    }
}

/// A file's record, as a scan makes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileRecord {
    pub name: Vec<u8>,
    pub mtime: (i64, u32),
    pub size: u64,
    pub ino: u64,
    pub kind: u8,
    pub vouches: bool,
    pub blob: ObjectId,
}

impl FileRecord {
    /// The record of a file or symlink with this metadata and blob.
    pub(crate) fn new(name: Vec<u8>, stat: &Stat, kind: u8, vouches: bool, blob: ObjectId) -> Self {
        FileRecord {
            name,
            mtime: stat.mtime,
            size: stat.size,
            ino: stat.ino,
            kind,
            vouches,
            blob,
        }
    }

    /// The record of an entry without metadata: a gitlink whose directory
    /// is there, or a subdirectory, with the base's tree for it if any.
    pub(crate) fn bare(name: Vec<u8>, kind: u8, blob: Option<ObjectId>) -> Self {
        FileRecord {
            name,
            mtime: (0, 0),
            size: 0,
            ino: 0,
            kind,
            vouches: false,
            blob: blob.unwrap_or(ZERO),
        }
    }

    fn view(&self) -> CachedFile<'_> {
        CachedFile {
            name: &self.name,
            mtime: self.mtime,
            size: self.size,
            ino: self.ino,
            kind: self.kind,
            vouches: self.vouches,
            blob: self.blob,
        }
    }
}

/// A directory's record for the next cache.
#[derive(Debug)]
pub(crate) enum Record<'c> {
    /// As the old cache has it.
    Kept(CachedDir<'c>),
    /// Made by this scan.
    Made(DirRecord),
}

impl Record<'_> {
    /// The directory's path.
    pub(crate) fn path(&self) -> &[u8] {
        match self {
            Record::Kept(dir) => dir.path(),
            Record::Made(dir) => &dir.path,
        }
    }

    fn file_count(&self) -> usize {
        match self {
            Record::Kept(dir) => dir.file_count(),
            Record::Made(dir) => dir.files.len(),
        }
    }

    /// The length of its block of name bytes.
    fn block_len(&self) -> usize {
        match self {
            Record::Kept(dir) => dir.block().len(),
            Record::Made(dir) => {
                dir.path.len() + dir.files.iter().map(|file| file.name.len()).sum::<usize>()
            }
        }
    }
}

/// Writes a cache into `state_dir`, through a scratch file and a rename:
/// the scan started at `time` and made `records`, sorted by path.
pub(crate) fn save(
    state_dir: &Path,
    time: (i64, u32),
    records: &[Record<'_>],
) -> Result<(), Error> {
    let target = state_dir.join(FILE);
    // A cache too large for its offsets is not kept: the next scan reads
    // what it must.
    let Some(bytes) = encode(time, records) else {
        let _ = fs::remove_file(&target);
        return Ok(());
    };
    let scratch = state_dir.join(format!("{FILE}.tmp-{}", random_hex()));
    let written = fs::File::create_new(&scratch)
        .and_then(|mut file| file.write_all(&bytes))
        .and_then(|()| fs::rename(&scratch, &target));
    if written.is_err() {
        let _ = fs::remove_file(&scratch);
    }
    written.map_err(Error::io("write", &target))
}

fn encode(time: (i64, u32), records: &[Record<'_>]) -> Option<Vec<u8>> {
    let files: usize = records.iter().map(Record::file_count).sum();
    let names: usize = records.iter().map(Record::block_len).sum();
    let small = |n: usize| u32::try_from(n).ok();
    let (dir_count, file_count, name_bytes) = (small(records.len())?, small(files)?, small(names)?);

    let mut out = Vec::with_capacity(
        HEADER_LEN + records.len() * DIR_LEN + files * FILE_LEN + names + CHECK_LEN,
    );
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&time.0.to_le_bytes());
    out.extend_from_slice(&time.1.to_le_bytes());
    out.extend_from_slice(&dir_count.to_le_bytes());
    out.extend_from_slice(&file_count.to_le_bytes());
    out.extend_from_slice(&name_bytes.to_le_bytes());

    // Offsets fit: the name bytes add up to no more than `u32::MAX`.
    let mut name_table: Vec<u8> = Vec::with_capacity(names);
    let mut file_table: Vec<u8> = Vec::with_capacity(files * FILE_LEN);
    let mut first = 0u32;
    for record in records {
        let block_at = name_table.len() as u32;
        let count = record.file_count() as u32;
        match record {
            Record::Kept(dir) => {
                // As it was, but for where its entries and names now lie.
                let mut bytes = dir.record().to_vec();
                bytes[0..4].copy_from_slice(&block_at.to_le_bytes());
                bytes[8..12].copy_from_slice(&first.to_le_bytes());
                out.extend_from_slice(&bytes);
                name_table.extend_from_slice(dir.block());
                file_table.extend_from_slice(dir.entries());
            }
            Record::Made(dir) => {
                let block_len = record.block_len() as u32;
                out.extend_from_slice(&block_at.to_le_bytes());
                out.extend_from_slice(&(dir.path.len() as u32).to_le_bytes());
                out.extend_from_slice(&first.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
                out.extend_from_slice(dir.base.unwrap_or(ZERO).as_bytes());
                out.extend_from_slice(&dir.flags().to_le_bytes());
                let stamp = dir.stamp.unwrap_or(DirStamp {
                    mtime: (0, 0),
                    ctime: (0, 0),
                    ino: 0,
                });
                out.extend_from_slice(&stamp.mtime.1.to_le_bytes());
                out.extend_from_slice(&stamp.mtime.0.to_le_bytes());
                out.extend_from_slice(&stamp.ctime.0.to_le_bytes());
                out.extend_from_slice(&stamp.ctime.1.to_le_bytes());
                out.extend_from_slice(&block_len.to_le_bytes());
                out.extend_from_slice(&stamp.ino.to_le_bytes());
                out.extend_from_slice(&dir.rules.to_le_bytes());
                name_table.extend_from_slice(&dir.path);
                for file in &dir.files {
                    let offset = name_table.len() as u32 - block_at;
                    name_table.extend_from_slice(&file.name);
                    file_table.extend_from_slice(&offset.to_le_bytes());
                    file_table.extend_from_slice(&(file.name.len() as u32).to_le_bytes());
                    file_table.extend_from_slice(&file.mtime.0.to_le_bytes());
                    file_table.extend_from_slice(&file.mtime.1.to_le_bytes());
                    file_table.push(file.kind);
                    file_table.push(if file.vouches { FILE_VOUCHES } else { 0 });
                    file_table.extend_from_slice(&0u16.to_le_bytes());
                    file_table.extend_from_slice(&file.size.to_le_bytes());
                    file_table.extend_from_slice(&file.ino.to_le_bytes());
                    file_table.extend_from_slice(file.blob.as_bytes());
                }
            }
        }
        first += count;
    }
    out.extend_from_slice(&file_table);
    out.extend_from_slice(&name_table);
    let check = crc32c::crc32c(&out);
    out.extend_from_slice(&check.to_le_bytes());
    Some(out)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

fn i64_at(bytes: &[u8], at: usize) -> i64 {
    u64_at(bytes, at) as i64
}

fn id_at(bytes: &[u8], at: usize) -> ObjectId {
    let mut id = [0u8; 32];
    id.copy_from_slice(&bytes[at..at + 32]);
    ObjectId::from_bytes(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::Kind;

    fn stat(mtime: i64) -> Stat {
        Stat {
            kind: StatKind::File,
            executable: false,
            size: 5,
            mtime: (mtime, 7),
            ino: 42,
        }
    }

    fn record(name: &str, mtime: i64, vouches: bool) -> FileRecord {
        let blob = ObjectId::hash(Kind::Blob, name.as_bytes());
        FileRecord::new(name.into(), &stat(mtime), KIND_FILE, vouches, blob)
    }

    fn dirs() -> Vec<DirRecord> {
        let tree = ObjectId::hash(Kind::Tree, b"t");
        let sub = ObjectId::hash(Kind::Tree, b"sub");
        vec![
            DirRecord {
                path: Vec::new(),
                base: Some(tree),
                clean: true,
                stamp: Some(DirStamp {
                    mtime: (40, 1),
                    ctime: (41, 2),
                    ino: 9,
                }),
                ignored: false,
                rules: 77,
                ignore_files: 2,
                files: vec![
                    record("a", 50, true),
                    record("b", 50, false),
                    FileRecord::bare(b"new".to_vec(), KIND_SUBDIR, None),
                    FileRecord::bare(b"sub".to_vec(), KIND_TREE, Some(sub)),
                ],
            },
            DirRecord {
                path: b"sub".to_vec(),
                base: Some(sub),
                clean: false,
                stamp: None,
                ignored: true,
                rules: 0,
                ignore_files: 0,
                files: Vec::new(),
            },
            DirRecord {
                path: b"sub/deep".to_vec(),
                base: None,
                clean: false,
                stamp: None,
                ignored: false,
                rules: 0,
                ignore_files: 1,
                files: vec![record("x", 60, true)],
            },
        ]
    }

    fn made(dirs: Vec<DirRecord>) -> Vec<Record<'static>> {
        dirs.into_iter().map(Record::Made).collect()
    }

    #[test]
    fn round_trip_and_damage() {
        let dir = tempfile::tempdir().unwrap();
        let empty = Cache::load(dir.path());
        assert_eq!((empty.dir_count(), empty.time()), (0, (0, 0)));
        assert!(empty.dir(b"").is_none());

        let written = dirs();
        save(dir.path(), (100, 9), &made(written.clone())).unwrap();
        let cache = Cache::load(dir.path());
        assert_eq!((cache.dir_count(), cache.time()), (3, (100, 9)));
        for record in &written {
            let found = cache.dir(&record.path).unwrap();
            assert!(record.same_as(&found), "{:?}", record.path);
            assert_eq!(found.path(), record.path);
            assert_eq!(found.stamp(), record.stamp);
            assert_eq!(
                (found.ignored(), found.rules(), found.ignore_files()),
                (record.ignored, record.rules, record.ignore_files)
            );
        }
        assert!(cache.dir(b"other").is_none());
        let root = cache.dir(b"").unwrap();
        assert_eq!(root.file(b"b").unwrap().name, b"b");
        assert_eq!(root.file(b"sub").unwrap().kind, KIND_TREE);
        assert!(root.file(b"c").is_none());
        // Only the directory's own entries are its own.
        assert!(root.file(b"x").is_none());
        // Nothing but the cache is left in the directory.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        // Records kept from a loaded cache are written again as they were.
        let mut kept: Vec<Record<'_>> = vec![
            Record::Kept(cache.dir(b"").unwrap()),
            Record::Made(written[1].clone()),
            Record::Kept(cache.dir(b"sub/deep").unwrap()),
        ];
        kept.sort_by(|a, b| a.path().cmp(b.path()));
        let other = tempfile::tempdir().unwrap();
        save(other.path(), (100, 9), &kept).unwrap();
        assert_eq!(
            fs::read(other.path().join(FILE)).unwrap(),
            fs::read(dir.path().join(FILE)).unwrap()
        );

        // A record that differs in anything is not the same.
        let mut changed = written[0].clone();
        changed.rules = 78;
        assert!(!changed.same_as(&root));
        let mut changed = written[0].clone();
        changed.files[1].vouches = true;
        assert!(!changed.same_as(&root));

        let mut bytes = fs::read(dir.path().join(FILE)).unwrap();
        bytes[60] ^= 1;
        fs::write(dir.path().join(FILE), &bytes).unwrap();
        assert_eq!(Cache::load(dir.path()).dir_count(), 0);
        fs::write(dir.path().join(FILE), b"braidwsc").unwrap();
        assert_eq!(Cache::load(dir.path()).dir_count(), 0);
    }

    #[test]
    fn the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let mut written = dirs();
        written[0].files.insert(2, record("racy", 100, true));
        save(dir.path(), (100, 0), &made(written)).unwrap();
        let cache = Cache::load(dir.path());
        let root = cache.dir(b"").unwrap();
        let time = cache.time();
        let blob = ObjectId::hash(Kind::Blob, b"a");
        let a = root.file(b"a").unwrap();
        assert_eq!(a.vouch(&stat(50), time), Some(blob));
        // Modified since, or written in the same instant as the scan.
        assert_eq!(a.vouch(&stat(51), time), None);
        let racy = root.file(b"racy").unwrap();
        assert_eq!(racy.vouch(&stat(100), time), None);
        // Another inode, size or kind.
        let mut moved = stat(50);
        moved.ino = 43;
        assert_eq!(a.vouch(&moved, time), None);
        let mut exec = stat(50);
        exec.executable = true;
        assert_eq!(a.vouch(&exec, time), None);
        let mut longer = stat(50);
        longer.size = 6;
        assert_eq!(a.vouch(&longer, time), None);
        // A record that does not vouch is never used.
        assert_eq!(root.file(b"b").unwrap().vouch(&stat(50), time), None);
    }
}
