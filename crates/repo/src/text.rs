// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Byte strings in braid's small text files.
//!
//! Paths are bytes, not text, but the files that record a workspace are
//! meant to be read by people too. A value is written with every byte
//! outside printable ASCII, and `%` itself, as `%` and two uppercase hex
//! digits, so any byte string fits on one line and reads back exactly.

use std::path::{Path, PathBuf};

/// The text form of `bytes`: printable ASCII as it is, any other byte and
/// `%` as `%XX`.
pub fn escape(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len());
    for &byte in bytes {
        if (0x20..0x7f).contains(&byte) && byte != b'%' {
            text.push(char::from(byte));
        } else {
            text.push('%');
            text.push(char::from(b"0123456789ABCDEF"[usize::from(byte >> 4)]));
            text.push(char::from(b"0123456789ABCDEF"[usize::from(byte & 0x0f)]));
        }
    }
    text
}

/// The bytes that [`escape`] wrote as `text`, or `None` if `text` is not
/// such a form: a `%` not followed by two hex digits, or a byte that should
/// have been escaped.
pub fn unescape(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'%' {
            let high = hex_value(*bytes.get(i + 1)?)?;
            let low = hex_value(*bytes.get(i + 2)?)?;
            out.push((high << 4) | low);
            i += 3;
        } else if (0x20..0x7f).contains(&byte) {
            out.push(byte);
            i += 1;
        } else {
            return None;
        }
    }
    Some(out)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// The bytes of a path as the operating system stores it. On systems where
/// paths are not byte strings, only paths that are valid Unicode have bytes.
pub fn path_bytes(path: &Path) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        path.to_str().map(|text| text.as_bytes().to_vec())
    }
}

/// The path whose bytes are `bytes`, the inverse of [`path_bytes`].
pub fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255).collect();
        let text = escape(&all);
        assert!(text.bytes().all(|byte| (0x20..0x7f).contains(&byte)));
        assert_eq!(unescape(&text).unwrap(), all);
        assert_eq!(escape(b"a b/c.txt"), "a b/c.txt");
        assert_eq!(escape(b"100%\n"), "100%25%0A");
        assert_eq!(unescape("caf%C3%a9").unwrap(), "caf\u{e9}".as_bytes());
    }

    #[test]
    fn malformed_escapes_are_refused() {
        for bad in ["%", "%4", "%zz", "tab\there", "caf\u{e9}"] {
            assert_eq!(unescape(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn paths_round_trip() {
        let path = Path::new("dir/sub dir/file.txt");
        let bytes = path_bytes(path).unwrap();
        assert_eq!(path_from_bytes(&bytes).unwrap(), path);
    }
}
