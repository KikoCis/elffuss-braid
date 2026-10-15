// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Finding the repository from a repository root or from a workspace.
//!
//! A workspace directory holds a `.braid` file, never tracked, that names its
//! repository and the workspace:
//!
//! ```text
//! braid-workspace 1
//! name agent-1
//! repository /absolute/path/of/the/repository
//! ```
//!
//! Values are written with [`escape`](crate::text::escape), so any path fits.
//! The workspace's own state lives in the repository, under
//! `.braid/ws/<name>/`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::text::{escape, path_bytes, path_from_bytes, unescape};
use crate::{DIR, Error, STORE_DIR};

/// Directory inside `.braid` that holds one directory of state per
/// workspace.
pub const WORKSPACES_DIR: &str = "ws";

/// The first line of a workspace file.
const HEADER: &str = "braid-workspace 1";

/// What a workspace's `.braid` file records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceLink {
    /// The repository's root directory: absolute, so that the workspace can
    /// be anywhere.
    pub repository: PathBuf,
    /// The workspace's name; see [`WorkspaceLink::is_valid_name`].
    pub name: String,
}

impl WorkspaceLink {
    /// Whether `name` can name a workspace: 1 to 64 ASCII letters, digits,
    /// `.`, `_` or `-`, starting with a letter or a digit. Names are used as
    /// directory names inside the repository, so nothing else is accepted.
    pub fn is_valid_name(name: &str) -> bool {
        let bytes = name.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 64
            && bytes[0].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    }

    /// The file's contents.
    pub fn encode(&self) -> Vec<u8> {
        let repository = path_bytes(&self.repository).unwrap_or_default();
        format!(
            "{HEADER}\nname {}\nrepository {}\n",
            escape(self.name.as_bytes()),
            escape(&repository)
        )
        .into_bytes()
    }

    /// Reads the contents of a workspace file.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "not UTF-8".to_owned())?;
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(format!("the first line is not {HEADER:?}"));
        }
        let mut name = None;
        let mut repository = None;
        for line in lines {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            let value = unescape(value).ok_or_else(|| format!("bad escape in {key:?}"))?;
            match key {
                "name" => name = Some(value),
                "repository" => repository = Some(value),
                // Later versions may add lines; this one ignores them.
                _ => {}
            }
        }
        let name = name
            .and_then(|name| String::from_utf8(name).ok())
            .filter(|name| Self::is_valid_name(name))
            .ok_or("no valid workspace name")?;
        let repository = repository
            .and_then(|bytes| path_from_bytes(&bytes))
            .filter(|path| path.is_absolute())
            .ok_or("no absolute repository path")?;
        Ok(WorkspaceLink { repository, name })
    }

    /// Reads the workspace file at `path`.
    pub fn read(path: &Path) -> Result<Self, Error> {
        let bytes = fs::read(path)?;
        Self::decode(&bytes).map_err(|detail| Error::BadWorkspaceFile {
            path: path.to_path_buf(),
            detail,
        })
    }
}

/// Where a search for a repository stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// A repository root: a directory holding a `.braid` directory.
    Root(PathBuf),
    /// A workspace: a directory holding a `.braid` file.
    Workspace {
        /// The workspace directory.
        dir: PathBuf,
        /// What its `.braid` file says.
        link: WorkspaceLink,
    },
}

/// Finds the nearest directory, `start` or one of its parents, that is a
/// repository root or a workspace.
///
/// A `.braid` directory counts only when it holds a store. A `.braid` file
/// must be a regular file, not a symlink, and must be valid: a damaged one
/// is an error rather than a reason to keep looking further up.
pub fn locate(start: &Path) -> Result<Location, Error> {
    for dir in start.ancestors() {
        let marker = dir.join(DIR);
        if marker.join(STORE_DIR).is_dir() {
            return Ok(Location::Root(dir.to_path_buf()));
        }
        match fs::symlink_metadata(&marker) {
            Ok(meta) if meta.is_file() => {
                let link = WorkspaceLink::read(&marker)?;
                return Ok(Location::Workspace {
                    dir: dir.to_path_buf(),
                    link,
                });
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Err(Error::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for good in ["a", "agent-1", "A.b_c", "0"] {
            assert!(WorkspaceLink::is_valid_name(good), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "-x",
            "a/b",
            "a b",
            "é",
            &"x".repeat(65),
        ] {
            assert!(!WorkspaceLink::is_valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn links_round_trip() {
        let link = WorkspaceLink {
            repository: PathBuf::from("/srv/repos/with space/and%percent"),
            name: "agent-7".to_owned(),
        };
        let bytes = link.encode();
        assert!(bytes.starts_with(HEADER.as_bytes()));
        assert_eq!(WorkspaceLink::decode(&bytes).unwrap(), link);

        for bad in [
            &b""[..],
            b"braid-workspace 2\nname a\nrepository /r\n",
            b"braid-workspace 1\nname ../x\nrepository /r\n",
            b"braid-workspace 1\nname a\nrepository relative\n",
            b"braid-workspace 1\nname a\n",
        ] {
            assert!(WorkspaceLink::decode(bad).is_err(), "{bad:?}");
        }
    }
}
