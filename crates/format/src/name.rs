// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Rules for the names that make up paths.
//!
//! The store accepts nearly any name, about what `git fsck` accepts. It refuses
//! names that cannot be one path component, and names that some file system
//! would treat as `.braid` or `.git`: a crafted repository could otherwise
//! write into the metadata of the checkout that receives it.
//!
//! Every name of a tree is checked each time the tree is built or read, so
//! the common case is cheap: a name that is all ASCII is folded with ASCII
//! lowercasing alone, without allocating. [`fold_for_aliases`] says why that
//! gives the same answer as the full Unicode fold, which every other name
//! goes through.

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
    match ascii_stem(name) {
        Some(stem) => {
            RESERVED
                .iter()
                .any(|reserved| stem.eq_ignore_ascii_case(reserved))
                || is_short_stem(stem)
        }
        None => {
            let folded = fold_unicode(name);
            RESERVED.contains(&folded.as_slice()) || is_short_name(&folded)
        }
    }
}

/// Whether `name` must not be a symlink.
pub fn forbidden_as_symlink(name: &[u8]) -> bool {
    match ascii_stem(name) {
        Some(stem) => NO_SYMLINK
            .iter()
            .any(|special| stem.eq_ignore_ascii_case(special)),
        None => NO_SYMLINK.contains(&fold_unicode(name).as_slice()),
    }
}

/// The name as the most forgiving file systems would compare it: without the
/// code points HFS+ ignores, in Unicode NFC, lowercased, and without anything
/// from the first `:` on (NTFS streams) or trailing dots and spaces (NTFS).
///
/// Only meant to detect aliases of special names; checkouts use a fold that
/// matches the file system they write to.
///
/// A name that is all ASCII is folded by ASCII lowercasing alone, which gives
/// the same bytes as the full fold: such a name is valid UTF-8; every code
/// point HFS+ ignores is outside ASCII; NFC leaves ASCII text as it is, since
/// no ASCII character has a decomposition or composes with another; and the
/// Unicode lowercase of an ASCII character is its ASCII lowercase. ASCII
/// lowercasing keeps every byte in place, so cutting at the first `:` and
/// trimming dots and spaces can come first, and the checks above can compare
/// that stem without letter case instead of lowercasing a copy. The tests
/// and the `names` fuzz target compare both paths.
pub fn fold_for_aliases(name: &[u8]) -> Vec<u8> {
    match ascii_stem(name) {
        Some(stem) => stem.to_ascii_lowercase(),
        None => fold_unicode(name),
    }
}

/// For a name that is all ASCII, what [`fold_for_aliases`] keeps of it, before
/// lowercasing: everything before the first `:`, without trailing dots and
/// spaces. `None` for any other name.
fn ascii_stem(name: &[u8]) -> Option<&[u8]> {
    if !name.is_ascii() {
        return None;
    }
    let end = name
        .iter()
        .position(|byte| *byte == b':')
        .unwrap_or(name.len());
    let mut stem = &name[..end];
    while let [rest @ .., b'.' | b' '] = stem {
        stem = rest;
    }
    Some(stem)
}

/// [`fold_for_aliases`] for any name, through Unicode: what names that are
/// not all ASCII go through.
fn fold_unicode(name: &[u8]) -> Vec<u8> {
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

/// [`is_short_name`] on the stem of an ASCII name, in any letter case.
fn is_short_stem(stem: &[u8]) -> bool {
    RESERVED_SHORT.iter().any(|short| {
        stem.len() == short.len() + 2
            && stem[..short.len()].eq_ignore_ascii_case(short)
            && stem[short.len()] == b'~'
            && matches!(stem[short.len() + 1], b'1'..=b'9')
    })
}

/// The alias checks without the ASCII fast path, every name through the full
/// Unicode fold, as they were written before it: what the tests and the
/// `names` fuzz target compare the fast path with. Not part of the stable
/// API.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod reference {
    use super::{NO_SYMLINK, RESERVED, fold_unicode, is_short_name};

    /// [`super::fold_for_aliases`] through Unicode, whatever the name.
    pub fn fold_for_aliases(name: &[u8]) -> Vec<u8> {
        fold_unicode(name)
    }

    /// [`super::is_reserved`] through Unicode, whatever the name.
    pub fn is_reserved(name: &[u8]) -> bool {
        let folded = fold_unicode(name);
        RESERVED.contains(&folded.as_slice()) || is_short_name(&folded)
    }

    /// [`super::forbidden_as_symlink`] through Unicode, whatever the name.
    pub fn forbidden_as_symlink(name: &[u8]) -> bool {
        NO_SYMLINK.contains(&fold_unicode(name).as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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

    /// The fast path gives the same fold and the same answers as the full
    /// Unicode fold.
    fn assert_agrees(name: &[u8]) {
        let shown = String::from_utf8_lossy(name);
        assert_eq!(
            fold_for_aliases(name),
            reference::fold_for_aliases(name),
            "{shown:?}"
        );
        assert_eq!(is_reserved(name), reference::is_reserved(name), "{shown:?}");
        assert_eq!(
            forbidden_as_symlink(name),
            reference::forbidden_as_symlink(name),
            "{shown:?}"
        );
    }

    /// The special names, as the full fold gives them.
    const SPECIAL: [&str; 10] = [
        ".git",
        ".braid",
        "git~1",
        "braid~9",
        ".gitmodules",
        ".gitignore",
        ".gitattributes",
        ".braidignore",
        "git~0",
        "braid~10",
    ];

    /// Characters that make folding interesting: those of the special names
    /// in both cases, what the fold cuts or trims, bytes no name may hold,
    /// code points HFS+ ignores, combining marks, and characters that NFC or
    /// lowercasing turn into others, some of them into ASCII.
    const PALETTE: [char; 59] = [
        '.', 'g', 'G', 'i', 'I', 't', 'T', 'b', 'B', 'r', 'R', 'a', 'A', 'd', 'D', 'm', 'M', 'o',
        'u', 'l', 'e', 's', 'n', 'k', 'K', '~', '0', '1', '9', ':', ' ', '$', '/', '\0', '\u{7f}',
        '\u{200c}', '\u{200f}', '\u{202a}', '\u{202e}', '\u{206a}', '\u{206f}', '\u{feff}',
        '\u{300}', '\u{301}', '\u{307}', '\u{338}',
        // KELVIN SIGN, whose NFC is K; ANGSTROM SIGN, whose NFC is Å.
        '\u{212a}', '\u{212b}',
        // İ lowercases to i and a combining dot; dotless ı; long s.
        '\u{130}', '\u{131}', '\u{17f}',
        // GREEK QUESTION MARK and GREEK VARIA, whose NFC are ; and `.
        '\u{37e}', '\u{1fef}', // Lookalikes that stay what they are.
        '\u{ff27}', '\u{ff0e}', '\u{2024}', '\u{e9}', '\u{1e9e}', '\u{fb01}',
    ];

    #[test]
    fn fast_path_agrees_on_known_aliases_and_near_misses() {
        let names: &[&[u8]] = &[
            b"",
            b".",
            b"..",
            b":",
            b". :",
            b".git",
            b".GiT",
            b".git.",
            b".git . .",
            b".git::$INDEX_ALLOCATION",
            b".git:",
            b":.git",
            b"x.git",
            b".gitx",
            b"GIT~1",
            b"Git~9",
            b"git~0",
            b"git~10",
            b"git~1.",
            b"git~1:x",
            b"BRAID~1",
            b".Braid",
            b".GITMODULES",
            b".gitattributes. ",
            b".braidignore:stream",
            b"\xff.git",
            b".git\xff",
            b"\x80",
            ".g\u{200c}it".as_bytes(),
            "\u{feff}.git".as_bytes(),
            ".bra\u{206f}id".as_bytes(),
            ".gi\u{307}t".as_bytes(),
            ".g\u{130}t".as_bytes(),
            ".g\u{131}t".as_bytes(),
            "git\u{ff5e}1".as_bytes(),
            "\u{212a}".as_bytes(),
            ".git\u{2024}".as_bytes(),
            ".gitmodule\u{17f}".as_bytes(),
            ".git\u{37e}".as_bytes(),
        ];
        for name in names {
            assert_agrees(name);
        }
    }

    #[test]
    fn fast_path_agrees_on_every_short_ascii_name() {
        assert_agrees(b"");
        for a in 0..0x80u8 {
            assert_agrees(&[a]);
            for b in 0..0x80u8 {
                assert_agrees(&[a, b]);
            }
        }
    }

    #[test]
    fn fast_path_agrees_on_every_case_and_ending_of_the_special_names() {
        let endings: [&[u8]; 8] = [b"", b".", b" ", b". .", b":", b":x", b"::$DATA", b"x"];
        for special in SPECIAL {
            let special = special.as_bytes();
            let letters = special.iter().filter(|b| b.is_ascii_alphabetic()).count();
            for case in 0..1u32 << letters {
                let mut name = Vec::new();
                let mut bit = 0;
                for byte in special {
                    if byte.is_ascii_alphabetic() {
                        name.push(if (case >> bit) & 1 == 1 {
                            byte.to_ascii_uppercase()
                        } else {
                            *byte
                        });
                        bit += 1;
                    } else {
                        name.push(*byte);
                    }
                }
                for ending in endings {
                    let mut with_ending = name.clone();
                    with_ending.extend_from_slice(ending);
                    assert_agrees(&with_ending);
                }
            }
        }
    }

    /// A special name, with letters in either case, a few characters of the
    /// palette inserted, and maybe dots, spaces or a stream name after it.
    fn near_special() -> impl Strategy<Value = Vec<u8>> {
        (
            prop::sample::select(SPECIAL.to_vec()),
            any::<u64>(),
            prop::collection::vec(
                (
                    any::<prop::sample::Index>(),
                    prop::sample::select(PALETTE.to_vec()),
                ),
                0..3,
            ),
            "[. ]{0,3}(:[a-z$]{0,4})?",
        )
            .prop_map(|(special, case, inserts, ending)| {
                let mut chars: Vec<char> = special
                    .chars()
                    .enumerate()
                    .map(|(i, c)| {
                        if (case >> (i % 64)) & 1 == 1 {
                            c.to_ascii_uppercase()
                        } else {
                            c
                        }
                    })
                    .collect();
                for (at, c) in inserts {
                    let at = at.index(chars.len() + 1);
                    chars.insert(at, c);
                }
                let mut name: String = chars.into_iter().collect();
                name.push_str(&ending);
                name.into_bytes()
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn fast_path_agrees_on_any_bytes(name in prop::collection::vec(any::<u8>(), 0..24)) {
            assert_agrees(&name);
        }

        #[test]
        fn fast_path_agrees_on_ascii(name in prop::collection::vec(0u8..0x80, 0..24)) {
            assert_agrees(&name);
        }

        #[test]
        fn fast_path_agrees_on_mixed_text(
            chars in prop::collection::vec(prop::sample::select(PALETTE.to_vec()), 0..16),
        ) {
            let name: String = chars.into_iter().collect();
            assert_agrees(name.as_bytes());
        }

        #[test]
        fn fast_path_agrees_near_the_special_names(name in near_special()) {
            assert_agrees(&name);
        }
    }
}
