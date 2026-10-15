// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! File system helpers: positional I/O, syncs, unique names, and files
//! published whole through a temporary file and a rename.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::Durability;
use crate::error::{Error, io_error};

/// Names of temporary files start with this. Readers ignore them.
pub(crate) const TEMP_PREFIX: &str = "tmp-";

/// A temporary file this old whose lock is free belongs to a writer that died.
const STALE_TEMP: Duration = Duration::from_secs(60);

/// Reads exactly `buf.len()` bytes at `offset`, without moving any cursor.
#[cfg(unix)]
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// Writes all of `buf` at `offset`, without moving any cursor.
#[cfg(unix)]
pub(crate) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

/// Reads exactly `buf.len()` bytes at `offset`.
#[cfg(windows)]
pub(crate) fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Writes all of `buf` at `offset`.
#[cfg(windows)]
pub(crate) fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Reads `len` bytes at `offset` into a new buffer.
pub(crate) fn read_range(file: &File, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    let mut buf = vec![0u8; len];
    read_exact_at(file, &mut buf, offset)?;
    Ok(buf)
}

/// Flushes a file's data: its contents and what is needed to read them back
/// (`Ordered`), or its contents and all of its metadata (`Full`).
pub(crate) fn sync_file(file: &File, durability: Durability) -> io::Result<()> {
    match durability {
        Durability::Ordered => file.sync_data(),
        Durability::Full => file.sync_all(),
    }
}

/// Flushes a directory, so that files created or renamed in it stay there.
pub(crate) fn sync_dir(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|dir| dir.sync_all())
            .map_err(io_error("sync directory", path))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// 32 random lowercase hex digits, for a file name in `dir`.
pub(crate) fn random_name(dir: &Path) -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|err| Error::Io {
        action: "pick a random file name in",
        path: dir.to_path_buf(),
        source: io::Error::other(err),
    })?;
    let mut name = String::with_capacity(32);
    for byte in bytes {
        name.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        name.push(char::from(b"0123456789abcdef"[usize::from(byte & 0x0f)]));
    }
    Ok(name)
}

/// Whether `name` is 32 lowercase hex digits.
pub(crate) fn is_random_name(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Writes `bytes` as `dir/name` all at once: into a temporary file, flushed,
/// then renamed over `name`, then the directory flushed. Readers see the old
/// file or the new one, never a mix, even after a crash.
pub(crate) fn publish_file(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), Error> {
    let (temp_path, mut file) = create_temp(dir)?;
    let target = dir.join(name);
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(io_error("write", &temp_path))
        .and_then(|()| fs::rename(&temp_path, &target).map_err(io_error("rename", &temp_path)))
        .and_then(|()| sync_dir(dir));
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// Creates a new temporary file in `dir` and locks it. The lock lives as long
/// as the file is open, which tells [`remove_stale_temps`] that its writer is
/// alive.
fn create_temp(dir: &Path) -> Result<(PathBuf, File), Error> {
    loop {
        let path = dir.join(format!("{TEMP_PREFIX}{}", random_name(dir)?));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                file.lock().map_err(io_error("lock", &path))?;
                return Ok((path, file));
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(io_error("create", &path)(err)),
        }
    }
}

/// Removes temporary files that writers which died left behind: old enough,
/// and with no live process holding their lock. Best effort: anything that
/// fails is left for next time. Returns how many were removed.
pub(crate) fn remove_stale_temps(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(TEMP_PREFIX) {
            continue;
        }
        let path = entry.path();
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= STALE_TEMP);
        if !old {
            continue;
        }
        let Ok(file) = File::open(&path) else {
            continue;
        };
        match file.try_lock() {
            Ok(()) => {
                if fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
            Err(TryLockError::WouldBlock | TryLockError::Error(_)) => {}
        }
    }
    removed
}

/// Number of temporary files in `dir`.
pub(crate) fn count_temps(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
                .count()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_names_are_hex_and_differ() {
        let dir = Path::new(".");
        let a = random_name(dir).unwrap();
        let b = random_name(dir).unwrap();
        assert!(is_random_name(&a), "{a}");
        assert!(is_random_name(&b), "{b}");
        assert_ne!(a, b);
        assert!(!is_random_name("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_random_name("0123"));
    }

    #[test]
    fn publish_replaces_whole_files() {
        let dir = tempfile::tempdir().unwrap();
        publish_file(dir.path(), "f", b"one").unwrap();
        publish_file(dir.path(), "f", b"two").unwrap();
        assert_eq!(fs::read(dir.path().join("f")).unwrap(), b"two");
        assert_eq!(count_temps(dir.path()), 0);
    }

    #[test]
    fn locked_or_young_temps_survive_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let (path, file) = create_temp(dir.path()).unwrap();
        // Young, and locked: kept either way.
        assert_eq!(remove_stale_temps(dir.path()), 0);
        drop(file);
        // Unlocked but young: still kept.
        assert_eq!(remove_stale_temps(dir.path()), 0);
        assert!(path.exists());
        assert_eq!(count_temps(dir.path()), 1);
    }

    #[test]
    fn positional_io() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        write_all_at(&file, b"world", 6).unwrap();
        write_all_at(&file, b"hello ", 0).unwrap();
        assert_eq!(read_range(&file, 0, 11).unwrap(), b"hello world");
        assert_eq!(read_range(&file, 6, 5).unwrap(), b"world");
        assert!(read_range(&file, 6, 6).is_err());
    }
}
