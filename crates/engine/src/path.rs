// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Small helpers for paths: names joined by `/`, as raw bytes.

use std::borrow::Cow;

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

/// A path for messages.
pub(crate) fn show(path: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(path)
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
    }
}
