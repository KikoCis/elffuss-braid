// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The change list of a changeset: every file whose entry differs between two
//! braid trees.

use std::sync::Arc;

use braid_format::{Change, ChangeOp, Entry, EntryKind, ObjectId, Objects};

use crate::error::Error;
use crate::fetch::TreeCache;

/// The per-file changes from `old` to `new`, in no particular order. `None`
/// stands for the empty tree. Subtrees with equal ids are skipped without
/// being read, so the cost follows what changed. A directory replaced by a
/// file is a delete for every file below it and one set for the file.
/// Directories are read through `trees`.
pub(crate) fn changes<O: Objects>(
    objects: &O,
    trees: &TreeCache,
    old: Option<ObjectId>,
    new: ObjectId,
) -> Result<Vec<Change>, Error> {
    let mut changes = Vec::new();
    // Directories still to compare: path prefix, old side, new side.
    let mut pending: Vec<(Vec<u8>, Option<ObjectId>, Option<ObjectId>)> =
        vec![(Vec::new(), old, Some(new))];
    let read = |side: Option<ObjectId>| -> Result<Arc<[Entry]>, Error> {
        match side {
            Some(id) => trees.entries(objects, id),
            None => Ok(Arc::from(Vec::new())),
        }
    };
    while let Some((prefix, old, new)) = pending.pop() {
        if old == new {
            continue;
        }
        let old_entries = read(old)?;
        let new_entries = read(new)?;
        let mut olds = old_entries.iter().peekable();
        let mut news = new_entries.iter().peekable();
        loop {
            // Both sides are sorted by name bytes.
            let (before, after) = match (olds.peek(), news.peek()) {
                (None, None) => break,
                (Some(_), None) => (olds.next(), None),
                (None, Some(_)) => (None, news.next()),
                (Some(o), Some(n)) => match o.name.cmp(&n.name) {
                    core::cmp::Ordering::Less => (olds.next(), None),
                    core::cmp::Ordering::Greater => (None, news.next()),
                    core::cmp::Ordering::Equal => (olds.next(), news.next()),
                },
            };
            let name = match (&before, &after) {
                (Some(entry), _) | (None, Some(entry)) => entry.name.clone(),
                (None, None) => unreachable!("one side always has an entry"),
            };
            let path = join(&prefix, &name);
            compare(
                path,
                before.map(|e: &Entry| e.kind.clone()),
                after.map(|e: &Entry| e.kind.clone()),
                &mut changes,
                &mut pending,
            );
        }
    }
    Ok(changes)
}

/// Records the changes for one name, queueing directories to compare.
fn compare(
    path: Vec<u8>,
    before: Option<EntryKind>,
    after: Option<EntryKind>,
    changes: &mut Vec<Change>,
    pending: &mut Vec<(Vec<u8>, Option<ObjectId>, Option<ObjectId>)>,
) {
    let tree_of = |kind: &Option<EntryKind>| match kind {
        Some(EntryKind::Tree { tree }) => Some(*tree),
        _ => None,
    };
    let (old_tree, new_tree) = (tree_of(&before), tree_of(&after));
    if old_tree != new_tree {
        pending.push((path.clone(), old_tree, new_tree));
    }
    let old_file = before.filter(|kind| !matches!(kind, EntryKind::Tree { .. }));
    let new_file = after.and_then(|kind| ChangeOp::from_entry(&kind));
    match (old_file, new_file) {
        (None, None) => {}
        (Some(_), None) => changes.push(Change {
            path,
            op: ChangeOp::Delete,
        }),
        (old, Some(op)) => {
            if old.and_then(|kind| ChangeOp::from_entry(&kind)).as_ref() != Some(&op) {
                changes.push(Change { path, op });
            }
        }
    }
}

fn join(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return name.to_vec();
    }
    let mut path = Vec::with_capacity(prefix.len() + 1 + name.len());
    path.extend_from_slice(prefix);
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::{GitId, Kind, MemoryObjects, build_directory};

    fn put_tree(objects: &MemoryObjects, entries: Vec<Entry>) -> ObjectId {
        let dir = build_directory(entries).unwrap();
        for (_, bytes) in &dir.nodes {
            objects.put(Kind::Tree, bytes).unwrap();
        }
        dir.root()
    }

    fn file(name: &str, text: &str) -> Entry {
        Entry {
            name: name.as_bytes().to_vec(),
            kind: EntryKind::File {
                blob: ObjectId::hash(Kind::Blob, text.as_bytes()),
                size: text.len() as u64,
            },
        }
    }

    fn dir(name: &str, tree: ObjectId) -> Entry {
        Entry {
            name: name.as_bytes().to_vec(),
            kind: EntryKind::Tree { tree },
        }
    }

    fn summary(mut changes: Vec<Change>) -> Vec<(String, &'static str)> {
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
            .into_iter()
            .map(|change| {
                let op = match change.op {
                    ChangeOp::Delete => "delete",
                    ChangeOp::File { .. } => "file",
                    ChangeOp::Executable { .. } => "exec",
                    ChangeOp::Symlink { .. } => "symlink",
                    ChangeOp::Gitlink { .. } => "gitlink",
                };
                (String::from_utf8(change.path).unwrap(), op)
            })
            .collect()
    }

    #[test]
    fn from_nothing_lists_every_file() {
        let objects = MemoryObjects::new();
        let sub = put_tree(&objects, vec![file("b", "2"), file("c", "3")]);
        let root = put_tree(&objects, vec![file("a", "1"), dir("sub", sub)]);
        assert_eq!(
            summary(changes(&objects, &TreeCache::default(), None, root).unwrap()),
            [
                ("a".into(), "file"),
                ("sub/b".into(), "file"),
                ("sub/c".into(), "file")
            ]
        );
        assert!(
            changes(&objects, &TreeCache::default(), Some(root), root)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn edits_kinds_and_directory_swaps() {
        let objects = MemoryObjects::new();
        let empty = put_tree(&objects, Vec::new());
        let inner = put_tree(&objects, vec![file("x", "1"), file("y", "2")]);
        let shared = put_tree(&objects, vec![file("s", "same")]);
        let old = put_tree(
            &objects,
            vec![
                file("edit", "old"),
                file("mode", "m"),
                dir("gone", inner),
                file("becomes-dir", "f"),
                dir("same", shared),
                dir("empty", empty),
                Entry {
                    name: b"sub".to_vec(),
                    kind: EntryKind::Gitlink {
                        commit: GitId::Sha1([1; 20]),
                    },
                },
            ],
        );
        let new = put_tree(
            &objects,
            vec![
                file("edit", "new"),
                Entry {
                    name: b"mode".to_vec(),
                    kind: EntryKind::Executable {
                        blob: ObjectId::hash(Kind::Blob, b"m"),
                        size: 1,
                    },
                },
                file("gone", "now a file"),
                dir("becomes-dir", inner),
                dir("same", shared),
                Entry {
                    name: b"sub".to_vec(),
                    kind: EntryKind::Gitlink {
                        commit: GitId::Sha1([2; 20]),
                    },
                },
            ],
        );
        assert_eq!(
            summary(changes(&objects, &TreeCache::default(), Some(old), new).unwrap()),
            [
                ("becomes-dir".into(), "delete"),
                ("becomes-dir/x".into(), "file"),
                ("becomes-dir/y".into(), "file"),
                ("edit".into(), "file"),
                ("gone".into(), "file"),
                ("gone/x".into(), "delete"),
                ("gone/y".into(), "delete"),
                ("mode".into(), "exec"),
                ("sub".into(), "gitlink"),
            ]
        );
    }

    #[test]
    fn missing_trees_are_errors() {
        let objects = MemoryObjects::new();
        let missing = ObjectId::hash(Kind::Tree, b"nowhere");
        assert!(matches!(
            changes(&objects, &TreeCache::default(), None, missing),
            Err(Error::MissingBraidObject { .. })
        ));
    }
}
