// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Rules for the names that make up paths.
//!
//! The store accepts nearly any name, about what `git fsck` accepts. It refuses
//! names that cannot be one path component, and names that some file system
//! would treat as `.braid` or `.git`: a crafted repository could otherwise
//! write into the metadata of the checkout that receives it.

use core::fmt;

use unicode_normalization::UnicodeNormalization;

/// Longest name accepted, in bytes.
pub const MAX_NAME_BYTES: usize = 4096;

/// Directory names that hold version control metadata.
const RESERVED: [&[u8]; 2] = [b".braid", b".git"];

/// Their 8.3 short-name stems (without the `~N` suffix).
const RESERVED_SHORT: [&[u8]; 2] = [b"braid", b"git"];

/// Files that git or braid read, and that must therefore never be symlinks.
const NO_SYMLINK: [&[u8]; 4] = [
    b".gitmodules",
    b".gitignore",
    b".gitattributes",
    b".braidignore",
];

/// Why a name is not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameError {
    /// The name is empty.
    Empty,
    /// The name is longer than [`MAX_NAME_BYTES`].
    TooLong(usize),
    /// The name contains `/`.
    Slash,
    /// The name contains a NUL byte.
    Nul,
    /// The name is `.` or `..`.
    DotOrDotDot,
    /// Some file system would treat the name as `.braid` or `.git`.
    Reserved,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::Empty => f.write_str("empty name"),
            NameError::TooLong(len) => {
                write!(f, "name of {len} bytes is longer than {MAX_NAME_BYTES}")
            }
            NameError::Slash => f.write_str("name contains '/'"),
            NameError::Nul => f.write_str("name contains a NUL byte"),
            NameError::DotOrDotDot => f.write_str("name is '.' or '..'"),
            NameError::Reserved => {
                f.write_str("name would be read as .braid or .git on some file system")
            }
        }
    }
}

impl std::error::Error for NameError {}

/// Why a path is not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// The component at this position (0-based) is not a valid name. An empty
    /// component means a leading, trailing or doubled `/`.
    Component { index: usize, error: NameError },
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::Component { index, error } => write!(f, "component {index}: {error}"),
        }
    }
}

impl std::error::Error for PathError {}

/// Checks one name, as stored in a tree.
pub fn validate_name(name: &[u8]) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(NameError::TooLong(name.len()));
    }
    if name.contains(&b'/') {
        return Err(NameError::Slash);
    }
    if name.contains(&0) {
        return Err(NameError::Nul);
    }
    if name == b"." || name == b".." {
        return Err(NameError::DotOrDotDot);
    }
    if is_reserved(name) {
        return Err(NameError::Reserved);
    }
    Ok(())
}

/// Checks a path: names joined by single `/`, no leading or trailing `/`.
pub fn validate_path(path: &[u8]) -> Result<(), PathError> {
    for (index, component) in path.split(|byte| *byte == b'/').enumerate() {
        validate_name(component).map_err(|error| PathError::Component { index, error })?;
    }
    Ok(())
}

/// Whether some file system would treat `name` as `.braid` or `.git`.
pub fn is_reserved(name: &[u8]) -> bool {
    let folded = fold_for_aliases(name);
    RESERVED.contains(&folded.as_slice()) || is_short_name(&folded)
}

/// Whether `name` must not be a symlink.
pub fn forbidden_as_symlink(name: &[u8]) -> bool {
    NO_SYMLINK.contains(&fold_for_aliases(name).as_slice())
}

/// The name as the most forgiving file systems would compare it: without the
/// code points HFS+ ignores, in Unicode NFC, lowercased, and without anything
/// from the first `:` on (NTFS streams) or trailing dots and spaces (NTFS).
///
/// Only meant to detect aliases of special names; checkouts use a fold that
/// matches the file system they write to.
pub fn fold_for_aliases(name: &[u8]) -> Vec<u8> {
    let mut folded = match core::str::from_utf8(name) {
        Ok(text) => text
            .chars()
            .filter(|c| !is_hfs_ignorable(*c))
            .nfc()
            .flat_map(char::to_lowercase)
            .collect::<String>()
            .into_bytes(),
        // A name that is not UTF-8 can only alias an ASCII name byte for byte.
        Err(_) => name.to_ascii_lowercase(),
    };
    if let Some(colon) = folded.iter().position(|byte| *byte == b':') {
        folded.truncate(colon);
    }
    while matches!(folded.last(), Some(b'.' | b' ')) {
        folded.pop();
    }
    folded
}

/// Code points that HFS+ drops when it compares names.
fn is_hfs_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}'
    )
}

/// `braid~N` or `git~N`: the 8.3 short names NTFS can give `.braid` and `.git`.
fn is_short_name(folded: &[u8]) -> bool {
    RESERVED_SHORT.iter().any(|stem| {
        folded.len() == stem.len() + 2
            && folded.starts_with(stem)
            && folded[stem.len()] == b'~'
            && matches!(folded[stem.len() + 1], b'1'..=b'9')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        for name in [
            &b"README.md"[..],
            b"src",
            b".github",
            b".gitkeep",
            b".git-blame-ignore-revs",
            b"git",
            b"braid",
            b"...",
            b"a b",
            b"caf\xc3\xa9",
            b"\xff\xfe",
            b"git~10",
        ] {
            assert_eq!(
                validate_name(name),
                Ok(()),
                "{:?}",
                String::from_utf8_lossy(name)
            );
        }
    }

    #[test]
    fn rejects_structural_problems() {
        assert_eq!(validate_name(b""), Err(NameError::Empty));
        assert_eq!(validate_name(b"a/b"), Err(NameError::Slash));
        assert_eq!(validate_name(b"a\0b"), Err(NameError::Nul));
        assert_eq!(validate_name(b"."), Err(NameError::DotOrDotDot));
        assert_eq!(validate_name(b".."), Err(NameError::DotOrDotDot));
        let long = vec![b'a'; MAX_NAME_BYTES + 1];
        assert_eq!(
            validate_name(&long),
            Err(NameError::TooLong(MAX_NAME_BYTES + 1))
        );
        assert_eq!(validate_name(&long[..MAX_NAME_BYTES]), Ok(()));
    }

    #[test]
    fn rejects_aliases_of_metadata_directories() {
        let aliases: &[&str] = &[
            ".git",
            ".braid",
            ".GIT",
            ".Braid",
            ".git.",
            ".git ",
            ".git . .",
            ".git::$INDEX_ALLOCATION",
            ".braid:stream",
            "GIT~1",
            "git~2",
            "BRAID~1",
            // HFS+ ignores these code points.
            ".g\u{200c}it",
            "\u{feff}.git",
            ".bra\u{206f}id",
        ];
        for alias in aliases {
            assert_eq!(
                validate_name(alias.as_bytes()),
                Err(NameError::Reserved),
                "{alias:?}"
            );
        }
    }

    #[test]
    fn unicode_normalisation_is_applied() {
        // "é" precomposed and decomposed fold to the same bytes.
        assert_eq!(
            fold_for_aliases("caf\u{e9}".as_bytes()),
            fold_for_aliases("cafe\u{301}".as_bytes())
        );
    }

    #[test]
    fn symlink_restrictions() {
        assert!(forbidden_as_symlink(b".gitmodules"));
        assert!(forbidden_as_symlink(b".GITIGNORE"));
        assert!(forbidden_as_symlink(b".braidignore."));
        assert!(forbidden_as_symlink(".gitattr\u{200d}ibutes".as_bytes()));
        assert!(!forbidden_as_symlink(b"gitmodules"));
        assert!(!forbidden_as_symlink(b"README"));
    }

    #[test]
    fn paths() {
        assert_eq!(validate_path(b"a"), Ok(()));
        assert_eq!(validate_path(b"a/b/c.txt"), Ok(()));
        let empty = |index| PathError::Component {
            index,
            error: NameError::Empty,
        };
        assert_eq!(validate_path(b""), Err(empty(0)));
        assert_eq!(validate_path(b"/a"), Err(empty(0)));
        assert_eq!(validate_path(b"a/"), Err(empty(1)));
        assert_eq!(validate_path(b"a//b"), Err(empty(1)));
        assert_eq!(
            validate_path(b"a/.git/config"),
            Err(PathError::Component {
                index: 1,
                error: NameError::Reserved
            })
        );
        assert_eq!(
            validate_path(b"a/../b"),
            Err(PathError::Component {
                index: 1,
                error: NameError::DotOrDotDot
            })
        );
    }
}
