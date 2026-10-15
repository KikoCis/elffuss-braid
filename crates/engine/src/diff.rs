// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The canonical change list between two trees.

use std::rc::Rc;

use braid_format::{Change, ChangeOp, Entry, EntryKind, ObjectId, Objects};

use crate::access::Trees;
use crate::error::Error;
use crate::path::{MAX_PATH_DEPTH, depth, join};

/// The canonical change list that turns the tree `old_root` into `new_root`:
/// exactly the paths whose file differs (in kind or id), sorted by path bytes,
/// without duplicates. A directory replaced by a file is a delete for every
/// file below it plus a set for the file, and the other way round.
///
/// Only subtrees whose ids differ are read, so the cost follows the size of
/// the difference. `apply(old, diff(old, new)) == new` (braid trees hold no
/// empty subdirectories), and `diff(x, x)` is empty. A tree deeper than
/// [`MAX_PATH_DEPTH`] is an error.
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
///
/// The walk keeps its own stack of directory pairs rather than recursing, and
/// one path buffer that each pair owns a prefix of, so neither the thread's
/// stack nor the memory for paths grows with the depth beyond the paths
/// themselves. A tree deeper than [`MAX_PATH_DEPTH`] is an error.
pub(crate) fn diff_trees<O: Objects>(
    trees: &mut Trees<'_, O>,
    dir: &[u8],
    old: Option<ObjectId>,
    new: Option<ObjectId>,
) -> Result<Vec<DiffItem>, Error> {
    let mut items = Vec::new();
    // The path of the entry being compared.
    let mut path = dir.to_vec();
    let mut stack = Vec::new();
    if old != new {
        stack.push(Pair::open(trees, &path, depth(dir), old, new)?);
    }
    while let Some(top) = stack.last_mut() {
        let (olds, news) = (Rc::clone(&top.olds), Rc::clone(&top.news));
        let (len, depth) = (top.len, top.depth);
        let mut below = None;
        while let Some((name, before, after)) = top.next(&olds, &news) {
            path.truncate(len);
            if len > 0 {
                path.push(b'/');
            }
            path.extend_from_slice(name);
            let (old_file, old_tree) = split(before);
            let (new_file, new_tree) = split(after);
            if old_file != new_file {
                items.push(DiffItem {
                    path: path.clone(),
                    old: old_file,
                    new: new_file,
                });
            }
            if old_tree != new_tree {
                below = Some((old_tree, new_tree));
                break;
            }
        }
        match below {
            Some((old, new)) => {
                let pair = Pair::open(trees, &path, depth + 1, old, new)?;
                stack.push(pair);
            }
            None => {
                stack.pop();
            }
        }
    }
    // A directory's entries are in name order, but `a/x` sorts after `a-b`.
    items.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(items)
}

/// A name, and its entry before and after.
type Versions<'e> = (&'e [u8], Option<&'e EntryKind>, Option<&'e EntryKind>);

/// Two versions of one directory, compared entry by entry.
struct Pair {
    olds: Rc<[Entry]>,
    news: Rc<[Entry]>,
    /// The next entry of each side to compare.
    i: usize,
    j: usize,
    /// Length of the directory's path in the path buffer.
    len: usize,
    /// How many names the directory's path has.
    depth: usize,
}

impl Pair {
    /// Opens the versions `old` and `new` of the directory at `path`, which
    /// has `depth` names. Its entries must not be deeper than the limit.
    fn open<O: Objects>(
        trees: &mut Trees<'_, O>,
        path: &[u8],
        depth: usize,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
    ) -> Result<Self, Error> {
        let olds = trees.read_or_empty(old)?;
        let news = trees.read_or_empty(new)?;
        if depth >= MAX_PATH_DEPTH
            && let Some(entry) = olds.first().or(news.first())
        {
            return Err(Error::TooDeep {
                path: join(path, &entry.name),
            });
        }
        Ok(Pair {
            olds,
            news,
            i: 0,
            j: 0,
            len: path.len(),
            depth,
        })
    }

    /// The next name, in order, with its entry on each side.
    fn next<'e>(&mut self, olds: &'e [Entry], news: &'e [Entry]) -> Option<Versions<'e>> {
        Some(match (olds.get(self.i), news.get(self.j)) {
            (None, None) => return None,
            (Some(o), Some(n)) if o.name == n.name => {
                self.i += 1;
                self.j += 1;
                (&o.name, Some(&o.kind), Some(&n.kind))
            }
            (Some(o), Some(n)) if o.name < n.name => {
                self.i += 1;
                (&o.name, Some(&o.kind), None)
            }
            (Some(o), None) => {
                self.i += 1;
                (&o.name, Some(&o.kind), None)
            }
            (_, Some(n)) => {
                self.j += 1;
                (&n.name, None, Some(&n.kind))
            }
        })
    }
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
    use crate::access::{EMPTY_TREE, write_tree};
    use crate::apply::apply;
    use crate::testing::{Repo, deep_path, delete, gitlink};

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

    /// A tree whose one file sits below `dirs` directories named `d`, built
    /// by hand: no change list can make one deeper than the limit.
    fn chain(repo: &Repo, dirs: usize) -> ObjectId {
        let file = Entry {
            name: b"file".to_vec(),
            kind: repo.file_kind("x"),
        };
        let mut tree = write_tree(&repo.objects, vec![file]).unwrap();
        for _ in 0..dirs {
            let dir = Entry {
                name: b"d".to_vec(),
                kind: EntryKind::Tree { tree },
            };
            tree = write_tree(&repo.objects, vec![dir]).unwrap();
        }
        tree
    }

    #[test]
    fn trees_deeper_than_the_limit_are_an_error() {
        let repo = Repo::new();
        let at_limit = chain(&repo, MAX_PATH_DEPTH - 1);
        assert_eq!(
            diff(&repo.objects, EMPTY_TREE, at_limit).unwrap(),
            [repo.set(&deep_path(MAX_PATH_DEPTH), "x")]
        );
        let too_deep = chain(&repo, MAX_PATH_DEPTH);
        for result in [
            diff(&repo.objects, EMPTY_TREE, too_deep),
            diff(&repo.objects, too_deep, EMPTY_TREE),
            diff(&repo.objects, at_limit, too_deep),
        ] {
            assert!(
                matches!(&result, Err(Error::TooDeep { path })
                    if *path == deep_path(MAX_PATH_DEPTH + 1).as_bytes()),
                "{result:?}"
            );
        }
    }
}
