// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The `format` file: which ids a store's objects have, and which storage
//! version its files follow (format spec, section 7).
//!
//! ```text
//! object-format=blake3-dk-v1
//! storage=2
//! ```
//!
//! The first line names the object ids. The second is the storage version:
//! what a reader must understand to use the store. A store without it has
//! storage version 1: records stored whole (codecs 0 and 1), which every
//! version of braid reads. Version 2 adds deltas (codec 3) and chunked
//! blobs (codec 4). Versions of braid from before storage versions compare
//! the whole file with the first line, so they refuse a store of version 2,
//! and this version refuses any version after 2, and any line it does not
//! know.
//!
//! New stores start at the current version. A store of version 1 is raised
//! to 2 only when a writer is about to append its first record of codec 3
//! or 4, or when a handle opening it finds such records already there. The
//! file is rewritten through a temporary file and a rename, under an
//! exclusive lock on `format.lock`, after reading it again: a store is never
//! lowered, and its new version is durable before any record that needs it.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;

use crate::error::{Error, io_error};
use crate::fsutil::publish_file;
use crate::{OBJECT_FORMAT, STORAGE_VERSION};

/// The bytes of the `format` file of a store of storage `version`.
pub(crate) fn contents(version: u32) -> String {
    if version <= 1 {
        format!("{OBJECT_FORMAT}\n")
    } else {
        format!("{OBJECT_FORMAT}\nstorage={version}\n")
    }
}

/// The storage version of the store at `root`. Refuses a directory that is
/// not a store, a store whose objects have other ids, and a store that
/// needs a newer version of braid.
pub(crate) fn read(root: &Path) -> Result<u32, Error> {
    let path = root.join("format");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(Error::NotAStore {
                path: root.to_path_buf(),
            });
        }
        Err(err) => return Err(io_error("read", &path)(err)),
    };
    let unsupported = || Error::UnsupportedFormat {
        found: String::from_utf8_lossy(&bytes).trim_end().to_owned(),
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| unsupported())?;
    let mut lines = text.strip_suffix('\n').unwrap_or(text).split('\n');
    if lines.next() != Some(OBJECT_FORMAT) {
        return Err(unsupported());
    }
    let version = match lines.next() {
        None => 1,
        Some(line) => line
            .strip_prefix("storage=")
            .filter(|digits| {
                !digits.is_empty()
                    && !digits.starts_with('0')
                    && digits.bytes().all(|b| b.is_ascii_digit())
            })
            .and_then(|digits| digits.parse::<u32>().ok())
            .ok_or_else(unsupported)?,
    };
    if lines.next().is_some() {
        return Err(unsupported());
    }
    if version > STORAGE_VERSION {
        return Err(Error::NewerStore {
            path,
            detail: format!(
                "it has storage version {version}, and this version reads versions up to \
                 {STORAGE_VERSION}"
            ),
        });
    }
    Ok(version)
}

/// Raises the storage version of the store at `root` to at least `version`,
/// under the lock that every writer takes to change it. Fails, changing
/// nothing, if the store needs a newer version of braid by now.
pub(crate) fn raise(root: &Path, version: u32) -> Result<(), Error> {
    let lock_path = root.join("format.lock");
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(io_error("create", &lock_path))?;
    lock.lock().map_err(io_error("lock", &lock_path))?;
    if read(root)? < version {
        publish_file(root, "format", contents(version).as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_text(text: &str) -> Result<u32, Error> {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("format"), text).unwrap();
        read(dir.path())
    }

    #[test]
    fn versions_are_read_and_written() {
        assert_eq!(read_text(&contents(1)).unwrap(), 1);
        assert_eq!(read_text(&contents(2)).unwrap(), 2);
        assert_eq!(contents(1), "object-format=blake3-dk-v1\n");
        assert_eq!(contents(2), "object-format=blake3-dk-v1\nstorage=2\n");
        // Without the final newline too.
        assert_eq!(
            read_text("object-format=blake3-dk-v1\nstorage=2").unwrap(),
            2
        );
    }

    #[test]
    fn newer_versions_and_unknown_lines_are_refused() {
        let err = read_text("object-format=blake3-dk-v1\nstorage=3\n").unwrap_err();
        assert!(matches!(err, Error::NewerStore { .. }), "{err}");
        assert!(err.to_string().contains("storage version 3"), "{err}");
        for bad in [
            "object-format=sha1\n",
            "object-format=blake3-dk-v1\nstorage=\n",
            "object-format=blake3-dk-v1\nstorage=02\n",
            "object-format=blake3-dk-v1\nstorage=two\n",
            "object-format=blake3-dk-v1\nstorage=2\nmore=1\n",
            "object-format=blake3-dk-v1\nchunks=yes\n",
            "",
        ] {
            assert!(
                matches!(read_text(bad), Err(Error::UnsupportedFormat { .. })),
                "{bad:?}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(read(dir.path()), Err(Error::NotAStore { .. })));
    }

    #[test]
    fn raising_never_lowers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("format");
        fs::write(&path, contents(1)).unwrap();
        raise(dir.path(), 2).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), contents(2));
        raise(dir.path(), 1).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), contents(2));
        // A store that went past this version is left alone.
        fs::write(&path, "object-format=blake3-dk-v1\nstorage=9\n").unwrap();
        assert!(matches!(
            raise(dir.path(), 2),
            Err(Error::NewerStore { .. })
        ));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "object-format=blake3-dk-v1\nstorage=9\n"
        );
    }
}
