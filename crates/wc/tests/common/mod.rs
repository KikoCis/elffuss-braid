// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Helpers for the workspace tests: a repository in a temporary directory,
//! changesets made straight through the engine, and branches moved by hand.

#![allow(dead_code)]

use std::convert::Infallible;
use std::path::{Path, PathBuf};

use braid_format::{Change, ChangeOp, Kind, ObjectId, Objects, Signature};
use braid_repo::Repo;
use braid_store::RefUpdate;
use braid_wc::Identity;

pub struct Fixture {
    pub temp: tempfile::TempDir,
    pub repo: Repo,
}

impl Fixture {
    pub fn new() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(&temp.path().join("repo")).unwrap();
        Fixture { temp, repo }
    }

    /// A path in the fixture's temporary directory.
    pub fn path(&self, name: &str) -> PathBuf {
        self.temp.path().join(name)
    }

    pub fn file(&self, content: &str) -> ChangeOp {
        ChangeOp::File {
            blob: self
                .repo
                .store()
                .put(Kind::Blob, content.as_bytes())
                .unwrap(),
            size: content.len() as u64,
        }
    }

    pub fn exec(&self, content: &str) -> ChangeOp {
        ChangeOp::Executable {
            blob: self
                .repo
                .store()
                .put(Kind::Blob, content.as_bytes())
                .unwrap(),
            size: content.len() as u64,
        }
    }

    pub fn symlink(&self, target: &[u8]) -> ChangeOp {
        ChangeOp::Symlink {
            blob: self.repo.store().put(Kind::Blob, target).unwrap(),
            size: target.len() as u64,
        }
    }

    /// A changeset on `parents` with these changes.
    pub fn commit(&self, parents: &[ObjectId], changes: Vec<(&str, ChangeOp)>) -> ObjectId {
        let changes = changes
            .into_iter()
            .map(|(path, op)| Change {
                path: path.as_bytes().to_vec(),
                op,
            })
            .collect();
        let id = braid_engine::commit(
            self.repo.store(),
            parents.to_vec(),
            changes,
            signature(),
            signature(),
            b"test change\n".to_vec(),
            Vec::new(),
        )
        .unwrap();
        self.repo.store().sync().unwrap();
        id
    }

    /// Points `branch` at `id`.
    pub fn set_branch(&self, branch: &str, id: ObjectId) {
        self.repo
            .store()
            .transact("test", "move a branch", |refs| {
                Ok::<_, Infallible>(vec![RefUpdate::new(branch, refs.get(branch), Some(id))])
            })
            .unwrap();
    }

    pub fn head(&self, branch: &str) -> Option<ObjectId> {
        self.repo.store().refs().unwrap().get(branch)
    }
}

pub fn signature() -> Signature {
    Signature {
        name: b"Ada".to_vec(),
        email: b"ada@example.org".to_vec(),
        seconds: 1_700_000_000,
        offset_minutes: 0,
    }
}

pub fn ada() -> Identity {
    Identity::new("Ada", "ada@example.org").unwrap()
}

/// Every entry under `dir`, recursively, without following symlinks.
pub fn listing(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            found.push(
                path.strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            if entry.file_type().unwrap().is_dir() {
                pending.push(path);
            }
        }
    }
    found.sort();
    found
}
