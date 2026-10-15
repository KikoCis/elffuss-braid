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

use std::io::Read;
use std::rc::Rc;

use cap_std::fs::Dir;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

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
    parent: Option<Rc<Rules>>,
}

/// The rules in force in a directory.
pub(crate) type Chain = Option<Rc<Rules>>;

/// The rules in force inside `dir`, whose path is `path`: those of its own
/// ignore files, if it has any, on top of `parent`. Ignore files that are
/// symlinks, unreadable, or not UTF-8 lines are skipped.
pub(crate) fn enter(dir: &Dir, path: &[u8], parent: &Chain) -> Chain {
    let mut matchers = Vec::new();
    for name in FILES {
        let Ok(mut file) = open_file(dir, name.as_ref()) else {
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
    Some(Rc::new(Rules {
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

    #[test]
    fn rules_nest_and_negate() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "*.log\nbuild/\n/top.txt\n").unwrap();
        std::fs::create_dir(temp.path().join("sub")).unwrap();
        std::fs::write(temp.path().join("sub/.gitignore"), "!keep.log\n").unwrap();
        std::fs::write(temp.path().join("sub/.braidignore"), "secret\n").unwrap();

        let root = enter(&dir(temp.path()), b"", &None);
        assert!(is_ignored(&root, b"a.log", false));
        assert!(is_ignored(&root, b"build", true));
        assert!(!is_ignored(&root, b"build", false));
        assert!(is_ignored(&root, b"top.txt", false));
        assert!(!is_ignored(&root, b"sub/top.txt", false));
        assert!(!is_ignored(&root, b"a.txt", false));

        let sub = enter(&dir(&temp.path().join("sub")), b"sub", &root);
        assert!(is_ignored(&sub, b"sub/x.log", false));
        assert!(!is_ignored(&sub, b"sub/keep.log", false));
        assert!(is_ignored(&sub, b"sub/secret", false));
        // Directories without ignore files share their parent's rules.
        std::fs::create_dir(temp.path().join("plain")).unwrap();
        let plain = enter(&dir(&temp.path().join("plain")), b"plain", &root);
        assert!(Rc::ptr_eq(plain.as_ref().unwrap(), root.as_ref().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ignore_files_are_not_read() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("rules"), "*\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("rules"), temp.path().join(".gitignore"))
            .unwrap();
        let root = enter(&dir(temp.path()), b"", &None);
        assert!(!is_ignored(&root, b"anything", false));
    }
}
