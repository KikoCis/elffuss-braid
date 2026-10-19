// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The link between a repository and the hub it lives on: the file
//! `.braid/hub`, a JSON object `{"url", "repo"}`, with `"paths"` for a
//! sparse clone, written by `braid hub publish` and `braid hub clone`.
//!
//! The other entries of `.braid` are `store/`, `git/`, `ws/`, `work/` and
//! `config`, so the name clashes with none of them; a workspace's own
//! `.braid` is a file in the workspace's directory, never this one.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use braid_hubproto::RepoPath;
use braid_repo::Repo;
use serde::{Deserialize, Serialize};

use super::creds::normalize_url;

/// The link's file name inside `.braid`.
pub(crate) const FILE: &str = "hub";

/// Where a repository lives on a hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Link {
    /// The hub's URL, normalized.
    pub(crate) url: String,
    /// The repository on it.
    pub(crate) repo: RepoPath,
    /// For a sparse clone, the path prefixes whose trees it holds.
    pub(crate) paths: Option<Vec<String>>,
}

/// The file's JSON.
#[derive(Serialize, Deserialize)]
struct LinkFile {
    url: String,
    repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    paths: Option<Vec<String>>,
}

impl Link {
    /// The link file of the repository whose root is `root`.
    pub(crate) fn file(root: &Path) -> PathBuf {
        root.join(braid_repo::DIR).join(FILE)
    }

    /// The repository's link, or `None` when it is not linked to a hub.
    pub(crate) fn read(repo: &Repo) -> Result<Option<Link>> {
        let path = Link::file(repo.root());
        match fs::read(&path) {
            Ok(bytes) => Link::decode(&bytes)
                .map(Some)
                .with_context(|| format!("{} is not a valid hub link", path.display())),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Writes the repository's link, through a scratch file and a rename.
    pub(crate) fn write(&self, repo: &Repo) -> Result<()> {
        let path = Link::file(repo.root());
        let scratch = path.with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&scratch, self.encode())
            .and_then(|()| fs::rename(&scratch, &path))
            .with_context(|| format!("cannot write {}", path.display()))
    }

    /// The file's bytes.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let file = LinkFile {
            url: self.url.clone(),
            repo: self.repo.to_string(),
            paths: self.paths.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file).unwrap_or_default();
        bytes.push(b'\n');
        bytes
    }

    /// Reads the file's bytes, checking every field.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Link> {
        let file: LinkFile = serde_json::from_slice(bytes)?;
        let paths = file
            .paths
            .map(|paths| paths.iter().map(|path| path_prefix(path)).collect())
            .transpose()?;
        Ok(Link {
            url: normalize_url(&file.url)?,
            repo: file.repo.parse()?,
            paths,
        })
    }

    /// The path prefixes of a sparse clone, as the object walk takes them.
    pub(crate) fn prefixes(&self) -> Option<Vec<Vec<u8>>> {
        self.paths
            .as_ref()
            .map(|paths| paths.iter().map(|path| path.as_bytes().to_vec()).collect())
    }
}

/// A path prefix as the hub takes it: names separated by `/`, without a
/// `/` at either end, and no empty, `.` or `..` name.
pub(crate) fn path_prefix(text: &str) -> Result<String> {
    let trimmed = text.trim_matches('/');
    if trimmed.is_empty()
        || trimmed
            .split('/')
            .any(|name| name.is_empty() || name == "." || name == "..")
    {
        bail!(
            "{text:?} is not a path prefix: give a directory or a file of the repository, such as src/net"
        );
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        assert_eq!(Link::read(&repo).unwrap(), None);
        let link = Link {
            url: "https://hub.example.org".to_owned(),
            repo: "Ada/demo".parse().unwrap(),
            paths: None,
        };
        link.write(&repo).unwrap();
        assert_eq!(Link::read(&repo).unwrap(), Some(link.clone()));
        let text = fs::read_to_string(dir.path().join(".braid/hub")).unwrap();
        assert!(text.contains("\"repo\": \"ada/demo\""), "{text}");
        assert!(!text.contains("paths"), "{text}");
        let sparse = Link {
            paths: Some(vec!["src/net".to_owned(), "docs".to_owned()]),
            ..link
        };
        sparse.write(&repo).unwrap();
        let read = Link::read(&repo).unwrap().unwrap();
        assert_eq!(read, sparse);
        assert_eq!(
            read.prefixes().unwrap(),
            [b"src/net".to_vec(), b"docs".to_vec()]
        );
        // Nothing else was left in .braid, and the repository opens and
        // checks as before.
        let mut entries: Vec<String> = fs::read_dir(dir.path().join(".braid"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, ["hub", "store"]);
        let reopened = Repo::open(dir.path()).unwrap();
        assert!(reopened.store().fsck().unwrap().is_clean());
    }

    #[test]
    fn a_workspace_named_hub_is_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        Link {
            url: "https://hub.example.org".to_owned(),
            repo: "ada/demo".parse().unwrap(),
            paths: None,
        }
        .write(&repo)
        .unwrap();
        let ws_dir = braid_wc::Workspace::default_dir(&repo, "hub");
        braid_wc::Workspace::create(&repo, "hub", &ws_dir, None, "main").unwrap();
        assert!(Link::read(&repo).unwrap().is_some());
        assert!(ws_dir.join(".braid").is_file());
    }

    #[test]
    fn invalid_links_are_refused() {
        for bad in [
            &b"not json"[..],
            br#"{"url": "https://hub.example.org"}"#,
            br#"{"url": "http://hub.example.org", "repo": "ada/demo"}"#,
            br#"{"url": "https://hub.example.org", "repo": "ada"}"#,
            br#"{"url": "https://hub.example.org", "repo": "ada/demo", "paths": ["../x"]}"#,
        ] {
            assert!(
                Link::decode(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn path_prefixes() {
        assert_eq!(path_prefix("/src/net/").unwrap(), "src/net");
        assert_eq!(path_prefix("README.md").unwrap(), "README.md");
        for bad in ["", "/", "a//b", "./a", "a/..", "../etc"] {
            assert!(path_prefix(bad).is_err(), "{bad}");
        }
    }
}
