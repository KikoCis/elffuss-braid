// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Trees as a workspace reads them.

use braid_engine::{EMPTY_TREE, read_changeset, read_tree};
use braid_format::{ChangeOp, EntryKind, ObjectId, Objects};

use crate::error::Error;
use crate::paths::join;

/// Every file, symlink and gitlink of the tree `root`, whose path is
/// `prefix`, with what the tree has there, by path. The walk keeps its own
/// stack.
pub(crate) fn leaves<O: Objects>(
    objects: &O,
    root: ObjectId,
    prefix: &[u8],
) -> Result<Vec<(Vec<u8>, ChangeOp)>, Error> {
    let mut found = Vec::new();
    let mut pending = vec![(prefix.to_vec(), root)];
    while let Some((dir, tree)) = pending.pop() {
        for entry in read_tree(objects, tree)? {
            let path = join(&dir, &entry.name);
            match &entry.kind {
                EntryKind::Tree { tree } => pending.push((path, *tree)),
                kind => {
                    if let Some(op) = ChangeOp::from_entry(kind) {
                        found.push((path, op));
                    }
                }
            }
        }
    }
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
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
    fn every_leaf_under_a_prefix() {
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
        let paths = |found: Vec<(Vec<u8>, ChangeOp)>| -> Vec<Vec<u8>> {
            found.into_iter().map(|(path, _)| path).collect()
        };
        assert_eq!(
            paths(leaves(&objects, root, b"").unwrap()),
            [b"a/b/c".to_vec(), b"a/d".to_vec(), b"e".to_vec()]
        );
        let EntryKind::Tree { tree } = braid_engine::entry_at(&objects, root, b"a")
            .unwrap()
            .unwrap()
        else {
            panic!("a is a directory");
        };
        assert_eq!(
            paths(leaves(&objects, tree, b"x/a").unwrap()),
            [b"x/a/b/c".to_vec(), b"x/a/d".to_vec()]
        );
        assert!(leaves(&objects, EMPTY_TREE, b"").unwrap().is_empty());
    }
}
