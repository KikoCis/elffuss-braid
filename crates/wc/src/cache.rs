// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The stat cache: what each file of the working copy hashed to, and the
//! metadata it had then, so that unchanged files are not read again.
//!
//! An entry is used only when the file's modification time, size, inode
//! number and type are all still the same, and its modification time is
//! strictly older than the time the cache's scan started, read from the
//! workspace's own file system. A file written in the same instant as the
//! scan, or later, may have changed without its metadata showing it, so it
//! is hashed again.
//!
//! ```text
//! [8] "braidwsc" | u32 version = 1 | u32 0
//! sv seconds | uv nanoseconds              when the scan started
//! uv n | n × entry, paths strictly ascending
//! [32] BLAKE3 (the derived-object context) of everything before it
//!
//! entry: bytes path | sv mtime seconds | uv mtime nanoseconds | uv size
//!        uv inode | u8 kind (0 file, 1 executable, 2 symlink) | [32] blob
//! ```
//!
//! The cache is only a cache: if it is missing or damaged, everything is
//! hashed again.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use braid_format::codec::{Reader, put_bytes, put_id, put_sv, put_uv};
use braid_format::{DecodeError, Kind, ObjectId};

use crate::error::Error;
use crate::fsprobe::random_hex;
use crate::safe::{Stat, StatKind};

const MAGIC: &[u8; 8] = b"braidwsc";
const VERSION: u32 = 1;

/// The file the cache lives in, inside the workspace's state directory.
pub(crate) const FILE: &str = "stat";

/// What a file was when it was hashed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub mtime: (i64, u32),
    pub size: u64,
    pub ino: u64,
    /// 0 regular file, 1 executable, 2 symlink.
    pub kind: u8,
    pub blob: ObjectId,
}

impl Entry {
    /// The entry for a file with this metadata and blob, or `None` for
    /// something that is not a file or a symlink.
    pub(crate) fn new(stat: &Stat, blob: ObjectId) -> Option<Entry> {
        Some(Entry {
            mtime: stat.mtime,
            size: stat.size,
            ino: stat.ino,
            kind: kind_code(stat)?,
            blob,
        })
    }
}

fn kind_code(stat: &Stat) -> Option<u8> {
    match stat.kind {
        StatKind::File if stat.executable => Some(1),
        StatKind::File => Some(0),
        StatKind::Symlink => Some(2),
        StatKind::Dir | StatKind::Other => None,
    }
}

/// The cache of one workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Cache {
    /// When the scan that wrote it started, by the file system's clock.
    pub time: (i64, u32),
    pub entries: HashMap<Vec<u8>, Entry>,
}

impl Cache {
    /// The cache in `state_dir`, or an empty one if there is none or it is
    /// damaged.
    pub(crate) fn load(state_dir: &Path) -> Cache {
        fs::read(state_dir.join(FILE))
            .ok()
            .and_then(|bytes| decode(&bytes).ok())
            .unwrap_or_default()
    }

    /// The blob of the file at `path`, if the cache may be trusted for a
    /// file that now has the metadata `stat`.
    pub(crate) fn lookup(&self, path: &[u8], stat: &Stat) -> Option<ObjectId> {
        let entry = self.entries.get(path)?;
        let same = entry.mtime == stat.mtime
            && entry.size == stat.size
            && entry.ino == stat.ino
            && Some(entry.kind) == kind_code(stat);
        (same && entry.mtime < self.time).then_some(entry.blob)
    }

    /// Writes the cache into `state_dir`, through a scratch file and a
    /// rename.
    pub(crate) fn save(&self, state_dir: &Path) -> Result<(), Error> {
        let bytes = self.encode();
        let scratch = state_dir.join(format!("{FILE}.tmp-{}", random_hex()));
        let target = state_dir.join(FILE);
        let written = fs::File::create_new(&scratch)
            .and_then(|mut file| file.write_all(&bytes))
            .and_then(|()| fs::rename(&scratch, &target));
        if written.is_err() {
            let _ = fs::remove_file(&scratch);
        }
        written.map_err(Error::io("write", &target))
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.entries.len() * 64);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        put_sv(&mut out, self.time.0);
        put_uv(&mut out, u64::from(self.time.1));
        let mut paths: Vec<&Vec<u8>> = self.entries.keys().collect();
        paths.sort();
        put_uv(&mut out, paths.len() as u64);
        for path in paths {
            let entry = &self.entries[path];
            put_bytes(&mut out, path);
            put_sv(&mut out, entry.mtime.0);
            put_uv(&mut out, u64::from(entry.mtime.1));
            put_uv(&mut out, entry.size);
            put_uv(&mut out, entry.ino);
            out.push(entry.kind);
            put_id(&mut out, &entry.blob);
        }
        let check = ObjectId::hash(Kind::Derived, &out);
        put_id(&mut out, &check);
        out
    }
}

fn decode(bytes: &[u8]) -> Result<Cache, DecodeError> {
    let invalid = |at, what| DecodeError::Invalid { at, what };
    if bytes.len() < 16 + ObjectId::LEN {
        return Err(invalid(0, "stat cache length"));
    }
    let (body, check) = bytes.split_at(bytes.len() - ObjectId::LEN);
    if ObjectId::hash(Kind::Derived, body).as_bytes()[..] != *check {
        return Err(invalid(body.len(), "stat cache checksum"));
    }
    let mut reader = Reader::new(body);
    if reader.take(8)? != MAGIC || reader.array::<4>()? != VERSION.to_le_bytes() {
        return Err(invalid(0, "stat cache header"));
    }
    reader.array::<4>()?;
    let seconds = reader.sv()?;
    let nanos = u32::try_from(reader.uv()?).map_err(|_| invalid(12, "nanoseconds"))?;
    let count = reader.count(1 + 1 + 1 + 1 + 1 + 1 + 32)?;
    let mut entries = HashMap::with_capacity(count);
    for _ in 0..count {
        let path = reader.bytes()?.to_vec();
        let at = reader.position();
        let mtime = (
            reader.sv()?,
            u32::try_from(reader.uv()?).map_err(|_| invalid(at, "nanoseconds"))?,
        );
        let entry = Entry {
            mtime,
            size: reader.uv()?,
            ino: reader.uv()?,
            kind: reader.u8()?,
            blob: reader.id()?,
        };
        entries.insert(path, entry);
    }
    reader.finish()?;
    Ok(Cache {
        time: (seconds, nanos),
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(mtime: i64) -> Stat {
        Stat {
            kind: StatKind::File,
            executable: false,
            size: 5,
            mtime: (mtime, 7),
            ino: 42,
        }
    }

    #[test]
    fn round_trip_and_damage() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Cache::load(dir.path()), Cache::default());
        let blob = ObjectId::hash(Kind::Blob, b"hello");
        let mut cache = Cache {
            time: (100, 0),
            entries: HashMap::new(),
        };
        cache
            .entries
            .insert(b"a/b".to_vec(), Entry::new(&stat(50), blob).unwrap());
        cache.save(dir.path()).unwrap();
        assert_eq!(Cache::load(dir.path()), cache);

        let mut bytes = fs::read(dir.path().join(FILE)).unwrap();
        bytes[20] ^= 1;
        fs::write(dir.path().join(FILE), &bytes).unwrap();
        assert_eq!(Cache::load(dir.path()), Cache::default());
    }

    #[test]
    fn the_guard() {
        let blob = ObjectId::hash(Kind::Blob, b"hello");
        let mut cache = Cache {
            time: (100, 0),
            entries: HashMap::new(),
        };
        cache
            .entries
            .insert(b"old".to_vec(), Entry::new(&stat(50), blob).unwrap());
        cache
            .entries
            .insert(b"racy".to_vec(), Entry::new(&stat(100), blob).unwrap());
        assert_eq!(cache.lookup(b"old", &stat(50)), Some(blob));
        // Modified since, or written in the same instant as the scan.
        assert_eq!(cache.lookup(b"old", &stat(51)), None);
        assert_eq!(cache.lookup(b"racy", &stat(100)), None);
        // Another inode, size or kind.
        let mut moved = stat(50);
        moved.ino = 43;
        assert_eq!(cache.lookup(b"old", &moved), None);
        let mut exec = stat(50);
        exec.executable = true;
        assert_eq!(cache.lookup(b"old", &exec), None);
        assert_eq!(cache.lookup(b"absent", &stat(50)), None);
    }
}
