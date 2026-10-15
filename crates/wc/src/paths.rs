// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Paths of the repository, which are bytes, and the file system names they
//! become.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::error::Error;

/// How many bytes of a path messages show.
const SHOWN_BYTES: usize = 256;

/// A path for messages: lossy UTF-8, cut short if it is long.
pub(crate) fn show(path: &[u8]) -> Cow<'_, str> {
    if path.len() <= SHOWN_BYTES {
        return String::from_utf8_lossy(path);
    }
    let mut shown = String::from_utf8_lossy(&path[..SHOWN_BYTES]).into_owned();
    shown.push('…');
    Cow::Owned(shown)
}

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

/// The directory part and the last name of a path; the directory of a path
/// with one name is the root, empty.
pub(crate) fn split_last(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => (&path[..slash], &path[slash + 1..]),
        None => (&[], path),
    }
}

/// The names of a path.
pub(crate) fn names(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|byte| *byte == b'/')
        .filter(|name| !name.is_empty())
}

/// Every proper prefix of `path` that names a directory, shortest first:
/// `a`, `a/b` for `a/b/c`.
pub(crate) fn ancestors(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(|(at, _)| &path[..at])
}

/// Whether `path` is `prefix` or lies below it. The empty prefix is the
/// root, which holds everything.
pub(crate) fn is_within(path: &[u8], prefix: &[u8]) -> bool {
    prefix.is_empty()
        || path == prefix
        || (path.len() > prefix.len() && path.starts_with(prefix) && path[prefix.len()] == b'/')
}

/// The file system name for one name of a repository path, if this platform
/// can represent it at all.
pub(crate) fn os_name(name: &[u8]) -> Option<&OsStr> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(OsStr::from_bytes(name))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(name).ok().map(OsStr::new)
    }
}

/// The repository bytes of a file system name, if it has any.
pub(crate) fn name_bytes(name: &OsStr) -> Option<&[u8]> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(name.as_bytes())
    }
    #[cfg(not(unix))]
    {
        name.to_str().map(str::as_bytes)
    }
}

/// Why this platform cannot hold `name` as a file name, if it cannot.
///
/// The store accepts names that some systems cannot write: Apple file
/// systems store names as UTF-8 and Windows forbids a set of characters and
/// device names. Such a name is refused before anything is written.
pub(crate) fn platform_problem(name: &[u8]) -> Option<&'static str> {
    if os_name(name).is_none() {
        return Some("the name is not valid UTF-8, which this platform needs");
    }
    if cfg!(target_vendor = "apple") && std::str::from_utf8(name).is_err() {
        return Some("the name is not valid UTF-8, which this file system needs");
    }
    if cfg!(windows) {
        if name
            .iter()
            .any(|byte| *byte < 0x20 || br#"<>:"\|?*"#.contains(byte))
        {
            return Some("the name holds a character Windows does not allow");
        }
        if matches!(name.last(), Some(b'.' | b' ')) {
            return Some("Windows drops a final dot or space");
        }
        let stem = name.split(|byte| *byte == b'.').next().unwrap_or(name);
        let stem = stem.to_ascii_uppercase();
        let device = matches!(stem.as_slice(), b"CON" | b"PRN" | b"AUX" | b"NUL")
            || (stem.len() == 4
                && (stem.starts_with(b"COM") || stem.starts_with(b"LPT"))
                && stem[3].is_ascii_digit());
        if device {
            return Some("the name is a Windows device name");
        }
    }
    None
}

/// Resolves a path given on the command line, relative to `cwd`, into a
/// path of the workspace whose root is `root`. Both must be absolute and
/// free of symlinks. `.` and `..` are resolved by name, without looking at
/// the file system. The workspace root itself is the empty path.
pub fn workspace_path(root: &Path, cwd: &Path, given: &Path) -> Result<Vec<u8>, Error> {
    let joined = cwd.join(given);
    let mut normal = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other.as_os_str()),
        }
    }
    let relative = normal
        .strip_prefix(root)
        .map_err(|_| Error::OutsideWorkspace(given.to_path_buf()))?;
    let mut bytes = Vec::new();
    for component in relative.components() {
        let name = name_bytes(component.as_os_str())
            .ok_or_else(|| Error::OutsideWorkspace(given.to_path_buf()))?;
        if !bytes.is_empty() {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(name);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(join(b"", b"a"), b"a");
        assert_eq!(join(b"a/b", b"c"), b"a/b/c");
        assert_eq!(split_last(b"a/b/c"), (&b"a/b"[..], &b"c"[..]));
        assert_eq!(split_last(b"c"), (&b""[..], &b"c"[..]));
        assert_eq!(ancestors(b"a/b/c").collect::<Vec<_>>(), [&b"a"[..], b"a/b"]);
        assert_eq!(ancestors(b"c").count(), 0);
        assert_eq!(names(b"a/b").collect::<Vec<_>>(), [&b"a"[..], b"b"]);
        assert!(is_within(b"a/b", b"a"));
        assert!(is_within(b"a", b"a"));
        assert!(is_within(b"anything", b""));
        assert!(!is_within(b"ab", b"a"));
        assert!(!is_within(b"a", b"a/b"));
    }

    #[test]
    fn command_line_paths() {
        let root = Path::new("/w/ws");
        let cwd = Path::new("/w/ws/src");
        assert_eq!(
            workspace_path(root, cwd, Path::new("a.rs")).unwrap(),
            b"src/a.rs"
        );
        assert_eq!(workspace_path(root, cwd, Path::new("../b")).unwrap(), b"b");
        assert_eq!(
            workspace_path(root, cwd, Path::new("./x/../y")).unwrap(),
            b"src/y"
        );
        assert_eq!(workspace_path(root, cwd, Path::new("..")).unwrap(), b"");
        assert_eq!(
            workspace_path(root, cwd, Path::new("/w/ws/c")).unwrap(),
            b"c"
        );
        assert!(matches!(
            workspace_path(root, cwd, Path::new("../../other")),
            Err(Error::OutsideWorkspace(_))
        ));
    }

    #[test]
    fn platform_names() {
        assert_eq!(platform_problem(b"README.md"), None);
        if cfg!(target_vendor = "apple") {
            assert!(platform_problem(b"\xff").is_some());
        }
    }
}
