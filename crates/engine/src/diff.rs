// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The canonical change list between two trees.

use braid_format::{Change, ChangeOp, EntryKind, ObjectId, Objects};

use crate::access::Trees;
use crate::error::Error;
use crate::path::join;

/// The canonical change list that turns the tree `old_root` into `new_root`:
/// exactly the paths whose file differs (in kind or id), sorted by path bytes,
/// without duplicates. A directory replaced by a file is a delete for every
/// file below it plus a set for the file, and the other way round.
///
/// Only subtrees whose ids differ are read, so the cost follows the size of
/// the difference. For trees without empty subdirectories,
/// `apply(old, diff(old, new)) == new`, and `diff(x, x)` is empty.
pub fn diff<O: Objects>(
    objects: &O,
    old_root: ObjectId,
    new_root: ObjectId,
) -> Result<Vec<Change>, Error> {
    let mut trees = Trees::new(objects);
    let items = diff_trees(&mut trees, b"", Some(old_root), Some(new_root))?;
    Ok(items
        .into_iter()
        .map(|item| Change {
            path: item.path,
            op: item.new.unwrap_or(ChangeOp::Delete),
        })
        .collect())
}

/// One path whose file differs between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiffItem {
    /// The full path.
    pub(crate) path: Vec<u8>,
    /// The file before, or `None` if there was none (nothing, or a directory).
    pub(crate) old: Option<ChangeOp>,
    /// The file after, or `None` if there is none.
    pub(crate) new: Option<ChangeOp>,
}

/// The files that differ between the directories `old` and `new` found at
/// `dir`, either of which may not exist, sorted by path.
pub(crate) fn diff_trees<O: Objects>(
    trees: &mut Trees<'_, O>,
    dir: &[u8],
    old: Option<ObjectId>,
    new: Option<ObjectId>,
) -> Result<Vec<DiffItem>, Error> {
    let mut items = Vec::new();
    walk(trees, dir, old, new, &mut items)?;
    // A directory's entries are in name order, but `a/x` sorts after `a-b`.
    items.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(items)
}

fn walk<O: Objects>(
    trees: &mut Trees<'_, O>,
    dir: &[u8],
    old: Option<ObjectId>,
    new: Option<ObjectId>,
    out: &mut Vec<DiffItem>,
) -> Result<(), Error> {
    if old == new {
        return Ok(());
    }
    let olds = trees.read_or_empty(old)?;
    let news = trees.read_or_empty(new)?;
    let (mut i, mut j) = (0, 0);
    loop {
        let (name, before, after) = match (olds.get(i), news.get(j)) {
            (None, None) => break,
            (Some(o), Some(n)) if o.name == n.name => {
                i += 1;
                j += 1;
                (&o.name, Some(&o.kind), Some(&n.kind))
            }
            (Some(o), Some(n)) if o.name < n.name => {
                i += 1;
                (&o.name, Some(&o.kind), None)
            }
            (Some(o), None) => {
                i += 1;
                (&o.name, Some(&o.kind), None)
            }
            (_, Some(n)) => {
                j += 1;
                (&n.name, None, Some(&n.kind))
            }
        };
        let path = join(dir, name);
        let (old_file, old_tree) = split(before);
        let (new_file, new_tree) = split(after);
        if old_tree != new_tree {
            walk(trees, &path, old_tree, new_tree, out)?;
        }
        if old_file != new_file {
            out.push(DiffItem {
                path,
                old: old_file,
                new: new_file,
            });
        }
    }
    Ok(())
}

/// An entry as a file and as a directory: at most one of the two.
fn split(entry: Option<&EntryKind>) -> (Option<ChangeOp>, Option<ObjectId>) {
    match entry {
        Some(EntryKind::Tree { tree }) => (None, Some(*tree)),
        Some(kind) => (ChangeOp::from_entry(kind), None),
        None => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::EMPTY_TREE;
    use crate::apply::apply;
    use crate::testing::{Repo, delete, gitlink};

    #[test]
    fn identical_trees_have_no_changes() {
        let repo = Repo::new();
        let root = repo.tree(&[("a/b", "1"), ("c", "2")]);
        assert_eq!(diff(&repo.objects, root, root).unwrap(), []);
        assert_eq!(diff(&repo.objects, EMPTY_TREE, EMPTY_TREE).unwrap(), []);
    }

    #[test]
    fn changes_are_per_file_and_sorted_by_path_bytes() {
        let repo = Repo::new();
        let old = repo.tree(&[("a/x", "1"), ("a-b", "2"), ("a0", "3"), ("keep/k", "4")]);
        let new = repo.tree(&[("a", "5"), ("a-b", "6"), ("a0", "3"), ("keep/k", "4")]);
        let changes = diff(&repo.objects, old, new).unwrap();
        assert_eq!(
            changes,
            [repo.set("a", "5"), repo.set("a-b", "6"), delete("a/x")]
        );
        assert_eq!(apply(&repo.objects, old, &changes).unwrap(), new);

        // The other way: a file replaced by a directory.
        let back = diff(&repo.objects, new, old).unwrap();
        assert_eq!(
            back,
            [delete("a"), repo.set("a-b", "2"), repo.set("a/x", "1")]
        );
        assert_eq!(apply(&repo.objects, new, &back).unwrap(), old);
    }

    #[test]
    fn from_and_to_the_empty_tree() {
        let repo = Repo::new();
        let root = repo.tree(&[("d/e/f", "1"), ("g", "2")]);
        assert_eq!(
            diff(&repo.objects, EMPTY_TREE, root).unwrap(),
            [repo.set("d/e/f", "1"), repo.set("g", "2")]
        );
        assert_eq!(
            diff(&repo.objects, root, EMPTY_TREE).unwrap(),
            [delete("d/e/f"), delete("g")]
        );
    }

    #[test]
    fn kind_changes_are_changes() {
        let repo = Repo::new();
        let old = repo.tree(&[("f", "same")]);
        let exec = apply(&repo.objects, old, &[repo.exec("f", "same")]).unwrap();
        assert_eq!(
            diff(&repo.objects, old, exec).unwrap(),
            [repo.exec("f", "same")]
        );
        let link = apply(&repo.objects, old, &[gitlink("f", 1)]).unwrap();
        assert_eq!(diff(&repo.objects, old, link).unwrap(), [gitlink("f", 1)]);
    }
}
