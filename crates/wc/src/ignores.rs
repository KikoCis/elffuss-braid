// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `.gitignore` and `.braidignore` rules, as the walk over a working copy
//! meets them.
//!
//! Each directory may hold either file, with git's pattern syntax. Rules in a
//! deeper directory win over rules above it, and within one directory
//! `.braidignore` wins over `.gitignore`. As in git, a file under an ignored
//! directory cannot be brought back by a later `!` rule, since the walk does
//! not go into ignored directories. Tracked files are never ignored: the
//! walk checks them whatever the rules say.
//!
//! The walk reads a directory's entries once, and opens an ignore file only
//! where the listing has one.

use std::io::Read;
use std::sync::Arc;

use cap_std::fs::Dir;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::fsprobe::FileSystem;
use crate::paths::os_name;
use crate::safe::open_file;

/// Ignore files read in each directory, highest precedence first.
const FILES: [&str; 2] = [".braidignore", ".gitignore"];

/// The rules of one directory, and those of the directories above it.
#[derive(Debug)]
pub(crate) struct Rules {
    /// The directory's path in the working copy; empty for the root.
    dir: Vec<u8>,
    /// Its matchers, highest precedence first.
    matchers: Vec<Gitignore>,
    parent: Option<Arc<Rules>>,
}

/// The rules in force in a directory.
pub(crate) type Chain = Option<Arc<Rules>>;

/// A directory's ignore files, as named in its listing, in the order of
/// [`FILES`].
pub(crate) type IgnoreFiles<'n> = [Option<&'n [u8]>; 2];

/// The ignore files among `names`, the entries of one directory: each
/// entry that `fs` takes for `.braidignore` or `.gitignore`, as it would
/// find it by that name.
pub(crate) fn ignore_files<'n>(
    fs: FileSystem,
    names: impl Iterator<Item = &'n [u8]>,
) -> IgnoreFiles<'n> {
    let mut found: IgnoreFiles<'n> = [None; 2];
    for name in names {
        // Folding maps ASCII letters to ASCII letters and changes no length
        // here, so only a dot name of the same length can be one of them.
        for (slot, wanted) in found.iter_mut().zip(FILES) {
            if slot.is_none()
                && name.len() == wanted.len()
                && name.first() == Some(&b'.')
                && fs.fold(name) == fs.fold(wanted.as_bytes())
            {
                *slot = Some(name);
            }
        }
    }
    found
}

/// The rules in force inside `dir`, whose path is `path`: those of its
/// ignore files `files` (see [`ignore_files`]) on top of `parent`. Ignore
/// files that are symlinks, unreadable, or not UTF-8 lines are skipped.
pub(crate) fn enter(dir: &Dir, path: &[u8], parent: &Chain, files: &IgnoreFiles<'_>) -> Chain {
    let mut matchers = Vec::new();
    for name in files {
        let Some(os) = name.and_then(os_name) else {
            continue;
        };
        let Ok(mut file) = open_file(dir, os) else {
            continue;
        };
        let mut text = Vec::new();
        if file.read_to_end(&mut text).is_err() {
            continue;
        }
        let mut builder = GitignoreBuilder::new(".");
        for line in text.split(|byte| *byte == b'\n') {
            if let Ok(line) = std::str::from_utf8(line) {
                // A pattern that does not parse is left out, as git does.
                let _ = builder.add_line(None, line.trim_end_matches('\r'));
            }
        }
        if let Ok(matcher) = builder.build()
            && !matcher.is_empty()
        {
            matchers.push(matcher);
        }
    }
    if matchers.is_empty() {
        return parent.clone();
    }
    Some(Arc::new(Rules {
        dir: path.to_vec(),
        matchers,
        parent: parent.clone(),
    }))
}

/// Whether the rules ignore `path`, a directory if `is_dir`.
pub(crate) fn is_ignored(chain: &Chain, path: &[u8], is_dir: bool) -> bool {
    let mut rules = chain.as_deref();
    while let Some(current) = rules {
        let skip = if current.dir.is_empty() {
            0
        } else {
            current.dir.len() + 1
        };
        if let Some(relative) = path.get(skip..).and_then(os_name) {
            for matcher in &current.matchers {
                match matcher.matched(relative, is_dir) {
                    Match::Ignore(_) => return true,
                    Match::Whitelist(_) => return false,
                    Match::None => {}
                }
            }
        }
        rules = current.parent.as_deref();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(path: &std::path::Path) -> Dir {
        Dir::open_ambient_dir(path, cap_std::ambient_authority()).unwrap()
    }

    const BOTH: IgnoreFiles<'static> = [Some(b".braidignore"), Some(b".gitignore")];
    const GIT: IgnoreFiles<'static> = [None, Some(b".gitignore")];

    #[test]
    fn rules_nest_and_negate() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "*.log\nbuild/\n/top.txt\n").unwrap();
        std::fs::create_dir(temp.path().join("sub")).unwrap();
        std::fs::write(temp.path().join("sub/.gitignore"), "!keep.log\n").unwrap();
        std::fs::write(temp.path().join("sub/.braidignore"), "secret\n").unwrap();

        let root = enter(&dir(temp.path()), b"", &None, &GIT);
        assert!(is_ignored(&root, b"a.log", false));
        assert!(is_ignored(&root, b"build", true));
        assert!(!is_ignored(&root, b"build", false));
        assert!(is_ignored(&root, b"top.txt", false));
        assert!(!is_ignored(&root, b"sub/top.txt", false));
        assert!(!is_ignored(&root, b"a.txt", false));

        let sub = enter(&dir(&temp.path().join("sub")), b"sub", &root, &BOTH);
        assert!(is_ignored(&sub, b"sub/x.log", false));
        assert!(!is_ignored(&sub, b"sub/keep.log", false));
        assert!(is_ignored(&sub, b"sub/secret", false));
        // Directories without ignore files share their parent's rules.
        std::fs::create_dir(temp.path().join("plain")).unwrap();
        let plain = enter(
            &dir(&temp.path().join("plain")),
            b"plain",
            &root,
            &[None, None],
        );
        assert!(Arc::ptr_eq(plain.as_ref().unwrap(), root.as_ref().unwrap()));
    }

    #[test]
    fn ignore_files_are_found_in_the_listing() {
        let exact = FileSystem::default();
        let names: [&[u8]; 4] = [b".gitignore", b"a.txt", b".braidignore", b".GITIGNORE"];
        assert_eq!(ignore_files(exact, names.into_iter()), BOTH);
        let only: [&[u8]; 2] = [b"x", b".GitIgnore"];
        assert_eq!(ignore_files(exact, only.into_iter()), [None, None]);
        // Where case is ignored, the file is found under any case, as an
        // open by its name would find it.
        assert_eq!(
            ignore_files(FileSystem::FORGIVING, only.into_iter()),
            [None, Some(&b".GitIgnore"[..])]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ignore_files_are_not_read() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("rules"), "*\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("rules"), temp.path().join(".gitignore"))
            .unwrap();
        let root = enter(&dir(temp.path()), b"", &None, &GIT);
        assert!(!is_ignored(&root, b"anything", false));
    }
}
