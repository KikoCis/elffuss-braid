// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! File system helpers: positional I/O, flushes, unique names, and files
//! published whole through a temporary file and a rename.
//!
//! # Flushes
//!
//! Every flush goes through [`flush`], which takes a batch of files and
//! makes everything written to them so far durable, `Full`, or ordered
//! before anything written after it returns, `Ordered`:
//!
//! - **Apple systems.** `fsync` moves a file's data to the drive, but the
//!   drive may keep it in its cache and write it out in any order;
//!   `F_FULLFSYNC` also has the drive write out its whole cache, which can
//!   cost many times what the rest of a transaction does; `F_BARRIERFSYNC`
//!   makes every write issued before it reach the medium before any write
//!   issued after it, without waiting for the cache. A batch moves every
//!   file but the last with `fsync`, then ends with one `F_BARRIERFSYNC`
//!   (`Ordered`) or `F_FULLFSYNC` (`Full`), which covers the writes of the
//!   whole batch, since cache and barrier are the drive's.
//! - **Elsewhere.** `fdatasync` (`Ordered`) or `fsync` (`Full`), file by
//!   file. Linux has no barrier to ask for: each of them writes out what
//!   the drive holds.
//!
//! Directories are flushed the same way on Unix systems, and not at all
//! elsewhere.

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

/// Flushes a batch of files, each with the path errors name: everything
/// written to them before the call is durable once it returns (`Full`), or
/// reaches the disk before anything written after it (`Ordered`). See the
/// module documentation for what that takes on each system.
pub(crate) fn flush(files: &[(&Path, &File)], durability: Durability) -> Result<(), Error> {
    let Some(((last_path, last), rest)) = files.split_last() else {
        return Ok(());
    };
    #[cfg(test)]
    observe::before(files);
    #[cfg(target_vendor = "apple")]
    {
        for (path, file) in rest {
            apple::push(file).map_err(io_error("sync", path))?;
        }
        match durability {
            Durability::Ordered => apple::barrier(last),
            Durability::Full => apple::full(last),
        }
        .map_err(io_error("sync", last_path))?;
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = (rest, last, last_path);
        for (path, file) in files {
            match durability {
                Durability::Ordered => file.sync_data(),
                Durability::Full => file.sync_all(),
            }
            .map_err(io_error("sync", path))?;
        }
    }
    #[cfg(test)]
    observe::after(files);
    Ok(())
}

/// Flushes one file, as [`flush`] does a batch.
pub(crate) fn flush_file(path: &Path, file: &File, durability: Durability) -> Result<(), Error> {
    flush(&[(path, file)], durability)
}

/// A directory opened to flush it, so that files created, renamed or
/// removed in it stay that way. `None` where directories cannot be opened
/// as files, and need no flush.
pub(crate) fn open_dir(path: &Path) -> Result<Option<File>, Error> {
    #[cfg(unix)]
    {
        File::open(path)
            .map(Some)
            .map_err(io_error("open directory", path))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

/// Flushes a directory, so that files created, renamed or removed in it
/// stay that way.
pub(crate) fn sync_dir(path: &Path, durability: Durability) -> Result<(), Error> {
    match open_dir(path)? {
        Some(dir) => flush_file(path, &dir, durability),
        None => Ok(()),
    }
}

/// The flushes of Apple systems, through `nix`, which wraps the calls safely.
#[cfg(target_vendor = "apple")]
mod apple {
    use std::fs::File;
    use std::io;

    use nix::errno::Errno;
    use nix::fcntl::{FcntlArg, fcntl};

    /// `fsync`: the drive has the data, in its cache, in no particular order.
    pub fn push(file: &File) -> io::Result<()> {
        retry(|| nix::unistd::fsync(file))
    }

    /// `F_BARRIERFSYNC`: `fsync`, then a barrier, so that the writes before
    /// it reach the medium before any after it. Where the file system does
    /// not take barriers, a full flush stands in for one.
    pub fn barrier(file: &File) -> io::Result<()> {
        match retry(|| fcntl(file, FcntlArg::F_BARRIERFSYNC).map(drop)) {
            Ok(()) => Ok(()),
            Err(_) => full(file),
        }
    }

    /// `F_FULLFSYNC`: `fsync`, then the drive writes out its whole cache.
    pub fn full(file: &File) -> io::Result<()> {
        retry(|| fcntl(file, FcntlArg::F_FULLFSYNC).map(drop))
    }

    fn retry(mut call: impl FnMut() -> nix::Result<()>) -> io::Result<()> {
        loop {
            match call() {
                Err(Errno::EINTR) => continue,
                result => return result.map_err(io::Error::from),
            }
        }
    }
}

/// What tests see of flushes: a hook called before and after every batch,
/// with the paths of its files, on the thread that flushes.
#[cfg(test)]
pub(crate) mod observe {
    use std::cell::RefCell;
    use std::fs::File;
    use std::path::{Path, PathBuf};

    /// When a batch is reported.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum When {
        /// About to flush: what was written may or may not be durable.
        Before,
        /// Flushed.
        After,
    }

    type Hook = Box<dyn FnMut(When, &[PathBuf])>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Calls `hook` for every batch this thread flushes, until the returned
    /// guard is dropped.
    pub(crate) fn watch(hook: impl FnMut(When, &[PathBuf]) + 'static) -> Guard {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Guard
    }

    /// Stops the hook when dropped.
    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with(|slot| *slot.borrow_mut() = None);
        }
    }

    fn call(when: When, files: &[(&Path, &File)]) {
        let paths: Vec<PathBuf> = files.iter().map(|(path, _)| path.to_path_buf()).collect();
        HOOK.with(|slot| {
            // A hook that flushes itself does not see its own batches.
            if let Ok(mut hook) = slot.try_borrow_mut()
                && let Some(hook) = hook.as_mut()
            {
                hook(when, &paths);
            }
        });
    }

    pub(super) fn before(files: &[(&Path, &File)]) {
        call(When::Before, files);
    }

    pub(super) fn after(files: &[(&Path, &File)]) {
        call(When::After, files);
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

/// Writes `bytes` as `dir/name` all at once: into a temporary file, flushed
/// in one batch with `with`, then renamed over `name`, then the directory
/// flushed. Readers see the old file or the new one, never a mix, even after
/// a crash, and the new one never reaches the disk before what `with` holds.
pub(crate) fn publish_file(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    durability: Durability,
    with: &[(&Path, &File)],
) -> Result<(), Error> {
    let (temp_path, mut file) = create_temp(dir)?;
    let target = dir.join(name);
    let result = file
        .write_all(bytes)
        .map_err(io_error("write", &temp_path))
        .and_then(|()| {
            let mut batch = with.to_vec();
            batch.push((&temp_path, &file));
            flush(&batch, durability)
        })
        .and_then(|()| fs::rename(&temp_path, &target).map_err(io_error("rename", &temp_path)))
        .and_then(|()| sync_dir(dir, durability));
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
        publish_file(dir.path(), "f", b"one", Durability::Ordered, &[]).unwrap();
        publish_file(dir.path(), "f", b"two", Durability::Full, &[]).unwrap();
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
