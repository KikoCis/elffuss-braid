// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! What a workspace's file system considers the same name.
//!
//! Many file systems ignore letter case, including the default ones of two
//! common desktop systems, and some ignore Unicode normalisation too. A tree
//! that holds `README` and `readme`, or `café` written two ways, is valid,
//! but on such a file system the two names are one file: writing both would
//! let the second overwrite the first, or write through a symlink the first
//! one made. Each workspace finds out once, when it is created, by creating
//! a file and looking it up under other spellings.

use std::borrow::Cow;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use cap_std::fs::{Dir, OpenOptions};
use unicode_normalization::UnicodeNormalization;

/// Prefix of the names braid gives its scratch files inside a workspace.
/// Status skips them.
pub(crate) const SCRATCH_PREFIX: &str = ".braid-";

/// Which differences between names a file system ignores.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileSystem {
    /// `README` and `readme` are one file.
    pub ignores_case: bool,
    /// `café` precomposed and decomposed are one file.
    pub ignores_normalization: bool,
}

impl FileSystem {
    /// The most forgiving file system: one that ignores both.
    pub const FORGIVING: FileSystem = FileSystem {
        ignores_case: true,
        ignores_normalization: true,
    };

    /// Finds out by creating a scratch file in `dir` and looking it up under
    /// another case and another normalisation. If the probe itself fails,
    /// the answer is the most forgiving file system, which refuses the most.
    pub fn probe(dir: &Dir) -> FileSystem {
        probe_names(dir).unwrap_or(FileSystem::FORGIVING)
    }

    /// The form of `name` under which this file system finds it: lowercased
    /// if case is ignored, in Unicode NFC if normalisation is. A name that
    /// is not UTF-8 is lowercased as ASCII.
    pub fn fold<'a>(&self, name: &'a [u8]) -> Cow<'a, [u8]> {
        if !self.ignores_case && !self.ignores_normalization {
            return Cow::Borrowed(name);
        }
        match std::str::from_utf8(name) {
            Ok(text) => {
                let folded: String = match (self.ignores_case, self.ignores_normalization) {
                    (true, true) => text.nfc().flat_map(char::to_lowercase).nfc().collect(),
                    (true, false) => text.chars().flat_map(char::to_lowercase).collect(),
                    _ => text.nfc().collect(),
                };
                Cow::Owned(folded.into_bytes())
            }
            Err(_) if self.ignores_case => Cow::Owned(name.to_ascii_lowercase()),
            Err(_) => Cow::Borrowed(name),
        }
    }
}

fn probe_names(dir: &Dir) -> io::Result<FileSystem> {
    let stem = format!("{SCRATCH_PREFIX}probe-{}-", random_hex());
    let created = format!("{stem}X\u{e9}");
    let other_case = format!("{stem}x\u{e9}");
    let other_form = format!("{stem}Xe\u{301}");
    dir.open_with(&created, OpenOptions::new().write(true).create_new(true))?;
    let found = |name: &str| dir.symlink_metadata(name).is_ok();
    let probed = FileSystem {
        ignores_case: found(&other_case),
        ignores_normalization: found(&other_form),
    };
    dir.remove_file(&created)?;
    Ok(probed)
}

/// Sixteen hex digits that differ between calls and processes, for scratch
/// file names. Not for secrets: names are always created exclusively.
pub(crate) fn random_hex() -> String {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding() {
        let exact = FileSystem::default();
        assert_eq!(exact.fold(b"README"), &b"README"[..]);
        let case = FileSystem {
            ignores_case: true,
            ignores_normalization: false,
        };
        assert_eq!(case.fold(b"ReadMe"), &b"readme"[..]);
        assert_ne!(
            case.fold("caf\u{e9}".as_bytes()),
            case.fold("cafe\u{301}".as_bytes())
        );
        let both = FileSystem::FORGIVING;
        assert_eq!(
            both.fold("CAF\u{c9}".as_bytes()),
            both.fold("cafe\u{301}".as_bytes())
        );
        assert_eq!(both.fold(b"\xffA"), &b"\xffa"[..]);
    }

    #[test]
    fn probing_leaves_nothing_behind() {
        let temp = tempfile::tempdir().unwrap();
        let dir = Dir::open_ambient_dir(temp.path(), cap_std::ambient_authority()).unwrap();
        let probed = FileSystem::probe(&dir);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        // Whatever the answer, it is the same the second time.
        assert_eq!(FileSystem::probe(&dir), probed);
        assert_ne!(random_hex(), random_hex());
    }
}
