// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Helpers for the unit tests: an in-memory repository with short ways to
//! write files, trees and changesets, and to read them back.

use std::collections::BTreeMap;

use braid_format::{
    Change, ChangeOp, EntryKind, GitId, Kind, MemoryObjects, ObjectId, Objects, Signature,
};

use crate::access::{EMPTY_TREE, read_blob, read_changeset, read_tree};
use crate::apply::apply;
use crate::commit::commit;
use crate::path::join;

pub(crate) fn signature(name: &str) -> Signature {
    Signature {
        name: name.as_bytes().to_vec(),
        email: format!("{name}@example.org").into_bytes(),
        seconds: 1_700_000_000,
        offset_minutes: 60,
    }
}

pub(crate) fn delete(path: &str) -> Change {
    Change {
        path: path.as_bytes().to_vec(),
        op: ChangeOp::Delete,
    }
}

pub(crate) fn gitlink(path: &str, byte: u8) -> Change {
    Change {
        path: path.as_bytes().to_vec(),
        op: ChangeOp::Gitlink {
            commit: GitId::Sha1([byte; 20]),
        },
    }
}

pub(crate) struct Repo {
    pub(crate) objects: MemoryObjects,
}

impl Repo {
    pub(crate) fn new() -> Self {
        Repo {
            objects: MemoryObjects::new(),
        }
    }

    pub(crate) fn blob(&self, content: &str) -> ObjectId {
        self.objects.put(Kind::Blob, content.as_bytes()).unwrap()
    }

    pub(crate) fn file_kind(&self, content: &str) -> EntryKind {
        EntryKind::File {
            blob: self.blob(content),
            size: content.len() as u64,
        }
    }

    pub(crate) fn set(&self, path: &str, content: &str) -> Change {
        Change {
            path: path.as_bytes().to_vec(),
            op: ChangeOp::File {
                blob: self.blob(content),
                size: content.len() as u64,
            },
        }
    }

    pub(crate) fn exec(&self, path: &str, content: &str) -> Change {
        Change {
            path: path.as_bytes().to_vec(),
            op: ChangeOp::Executable {
                blob: self.blob(content),
                size: content.len() as u64,
            },
        }
    }

    pub(crate) fn symlink(&self, path: &str, target: &str) -> Change {
        Change {
            path: path.as_bytes().to_vec(),
            op: ChangeOp::Symlink {
                blob: self.blob(target),
                size: target.len() as u64,
            },
        }
    }

    /// A tree with these regular files.
    pub(crate) fn tree(&self, files: &[(&str, &str)]) -> ObjectId {
        let changes: Vec<Change> = files
            .iter()
            .map(|(path, content)| self.set(path, content))
            .collect();
        apply(&self.objects, EMPTY_TREE, &changes).unwrap()
    }

    pub(crate) fn commit(&self, parents: &[ObjectId], changes: Vec<Change>) -> ObjectId {
        commit(
            &self.objects,
            parents.to_vec(),
            changes,
            signature("author"),
            signature("author"),
            b"change\n".to_vec(),
            Vec::new(),
        )
        .unwrap()
    }

    pub(crate) fn root(&self, changeset: ObjectId) -> ObjectId {
        read_changeset(&self.objects, changeset).unwrap().root
    }

    /// Every file under `root`, as text: regular files by content,
    /// executables as `exec:<content>`, symlinks as `link:<target>` and
    /// gitlinks as `gitlink:<first byte>`.
    pub(crate) fn files(&self, root: ObjectId) -> BTreeMap<String, String> {
        let mut files = BTreeMap::new();
        let mut pending = vec![(Vec::new(), root)];
        while let Some((dir, tree)) = pending.pop() {
            for entry in read_tree(&self.objects, tree).unwrap() {
                let path = join(&dir, &entry.name);
                let text = |blob: ObjectId| {
                    String::from_utf8(read_blob(&self.objects, blob).unwrap()).unwrap()
                };
                let value = match entry.kind {
                    EntryKind::Tree { tree } => {
                        pending.push((path, tree));
                        continue;
                    }
                    EntryKind::File { blob, .. } => text(blob),
                    EntryKind::Executable { blob, .. } => format!("exec:{}", text(blob)),
                    EntryKind::Symlink { blob, .. } => format!("link:{}", text(blob)),
                    EntryKind::Gitlink { commit } => format!("gitlink:{}", commit.as_bytes()[0]),
                };
                files.insert(String::from_utf8(path).unwrap(), value);
            }
        }
        files
    }
}

/// A path with `depth` names: `d/d/…/d/file`.
pub(crate) fn deep_path(depth: usize) -> String {
    let mut names = vec!["d"; depth - 1];
    names.push("file");
    names.join("/")
}

/// A map from path to text, for comparing with [`Repo::files`].
pub(crate) fn text_map(files: &[(&str, &str)]) -> BTreeMap<String, String> {
    files
        .iter()
        .map(|(path, value)| (path.to_string(), value.to_string()))
        .collect()
}
