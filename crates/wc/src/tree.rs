// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Trees as a workspace sees them: every file by path.

use std::collections::{BTreeMap, BTreeSet};

use braid_engine::{EMPTY_TREE, read_changeset, read_tree};
use braid_format::{ChangeOp, EntryKind, ObjectId, Objects};

use crate::error::Error;
use crate::paths::join;

/// Every file of a tree, by path, and every directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flat {
    /// Regular files, executables, symlinks and gitlinks.
    pub files: BTreeMap<Vec<u8>, ChangeOp>,
    /// Paths of directories, the root excepted.
    pub dirs: BTreeSet<Vec<u8>>,
}

/// Reads the whole tree `root`. The walk keeps its own stack.
pub(crate) fn flatten<O: Objects>(objects: &O, root: ObjectId) -> Result<Flat, Error> {
    let mut flat = Flat::default();
    let mut pending = vec![(Vec::new(), root)];
    while let Some((dir, tree)) = pending.pop() {
        for entry in read_tree(objects, tree)? {
            let path = join(&dir, &entry.name);
            match &entry.kind {
                EntryKind::Tree { tree } => {
                    flat.dirs.insert(path.clone());
                    pending.push((path, *tree));
                }
                kind => {
                    if let Some(op) = ChangeOp::from_entry(kind) {
                        flat.files.insert(path, op);
                    }
                }
            }
        }
    }
    Ok(flat)
}

/// The root tree of a changeset, or the empty tree for none.
pub(crate) fn root_of<O: Objects>(
    objects: &O,
    changeset: Option<ObjectId>,
) -> Result<ObjectId, Error> {
    match changeset {
        Some(id) => Ok(read_changeset(objects, id)?.root),
        None => Ok(EMPTY_TREE),
    }
}

/// Whether two entries are the same kind of thing: regular files (with or
/// without the executable bit), symlinks, or gitlinks.
pub(crate) fn same_class(a: &ChangeOp, b: &ChangeOp) -> bool {
    let class = |op: &ChangeOp| match op {
        ChangeOp::File { .. } | ChangeOp::Executable { .. } => 0,
        ChangeOp::Symlink { .. } => 1,
        ChangeOp::Gitlink { .. } => 2,
        ChangeOp::Delete => 3,
    };
    class(a) == class(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::{Change, Kind, MemoryObjects};

    #[test]
    fn flattening() {
        let objects = MemoryObjects::new();
        let blob = objects.put(Kind::Blob, b"x").unwrap();
        let file = ChangeOp::File { blob, size: 1 };
        let changes: Vec<Change> = ["a/b/c", "a/d", "e"]
            .iter()
            .map(|path| Change {
                path: path.as_bytes().to_vec(),
                op: file.clone(),
            })
            .collect();
        let root = braid_engine::apply(&objects, EMPTY_TREE, &changes).unwrap();
        let flat = flatten(&objects, root).unwrap();
        assert_eq!(
            flat.files.keys().cloned().collect::<Vec<_>>(),
            [b"a/b/c".to_vec(), b"a/d".to_vec(), b"e".to_vec()]
        );
        assert_eq!(
            flat.dirs.iter().cloned().collect::<Vec<_>>(),
            [b"a".to_vec(), b"a/b".to_vec()]
        );
        assert_eq!(flatten(&objects, EMPTY_TREE).unwrap(), Flat::default());
    }
}
