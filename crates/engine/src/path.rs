// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Small helpers for paths: names joined by `/`, as raw bytes.

use std::borrow::Cow;

use crate::error::Error;

/// The most names a path may have: 1,024.
///
/// This is a store policy, not part of the hashed format, which sets no
/// limit. A store refuses a change whose paths are deeper, so no tree it holds
/// is deeper either, and every walk over a tree stops with
/// [`Error::TooDeep`] instead of going further.
pub const MAX_PATH_DEPTH: usize = 1024;

/// How many bytes of a path messages show.
const SHOWN_BYTES: usize = 256;

/// `dir/name`, or `name` when `dir` is the root (empty).
pub(crate) fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(dir.len() + 1 + name.len());
    if !dir.is_empty() {
        path.extend_from_slice(dir);
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// The last name of a path.
pub(crate) fn last_name(path: &[u8]) -> &[u8] {
    path.rsplit(|byte| *byte == b'/').next().unwrap_or(path)
}

/// How many names a path has; the root, empty, has none.
pub(crate) fn depth(path: &[u8]) -> usize {
    if path.is_empty() {
        0
    } else {
        1 + path.iter().filter(|byte| **byte == b'/').count()
    }
}

/// Fails if `path` has more than [`MAX_PATH_DEPTH`] names.
pub(crate) fn check_depth(path: &[u8]) -> Result<(), Error> {
    if depth(path) > MAX_PATH_DEPTH {
        return Err(Error::TooDeep {
            path: path.to_vec(),
        });
    }
    Ok(())
}

/// A path for messages, cut short if it is long.
pub(crate) fn show(path: &[u8]) -> Cow<'_, str> {
    if path.len() <= SHOWN_BYTES {
        return String::from_utf8_lossy(path);
    }
    let mut shown = String::from_utf8_lossy(&path[..SHOWN_BYTES]).into_owned();
    shown.push('…');
    Cow::Owned(shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(join(b"", b"a"), b"a");
        assert_eq!(join(b"a/b", b"c"), b"a/b/c");
        assert_eq!(last_name(b"a/b/c"), b"c");
        assert_eq!(last_name(b"c"), b"c");
        assert_eq!(show(b"caf\xc3\xa9"), "caf\u{e9}");
        assert_eq!(show(&[b'a'; 300]).chars().count(), SHOWN_BYTES + 1);
        assert_eq!(depth(b""), 0);
        assert_eq!(depth(b"a"), 1);
        assert_eq!(depth(b"a/b/c"), 3);
    }

    #[test]
    fn depth_limit() {
        let deepest = vec!["d"; MAX_PATH_DEPTH].join("/");
        assert!(check_depth(deepest.as_bytes()).is_ok());
        let deeper = format!("{deepest}/d");
        assert!(matches!(
            check_depth(deeper.as_bytes()),
            Err(Error::TooDeep { path }) if path == deeper.as_bytes()
        ));
    }
}
