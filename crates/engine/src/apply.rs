// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Applying a change list to a tree by path copying: only the directories on
//! changed paths are read and rebuilt; every other subtree is shared with the
//! base tree.

use std::collections::BTreeMap;

use braid_format::name::forbidden_as_symlink;
use braid_format::{
    Change, ChangeOp, Entry, EntryKind, Kind, ObjectId, Objects, build_directory, validate_path,
};

use crate::access::{EMPTY_TREE, Trees, empty_tree_node, read_object};
use crate::error::{ApplyProblem, Error, store_error};
use crate::path::{join, last_name};

/// Applies `changes` to the tree `base_root`, stores the directories it
/// rebuilds, and returns the new root.
///
/// The changes may come in any order. Directories are created as needed, and
/// a directory left without entries is removed; the root becomes the empty
/// tree instead. A structural problem is an error, never a panic: a change
/// below a file, a file set where a directory with entries remains, deleting
/// a directory or a missing path, setting a path to the entry it already has
/// (a no-op is not canonical), or two changes to one path.
pub fn apply<O: Objects>(
    objects: &O,
    base_root: ObjectId,
    changes: &[Change],
) -> Result<ObjectId, Error> {
    let sorted = prepare(changes)?;
    let mut trees = Trees::new(objects);
    let mut problems = Vec::new();
    let applied = apply_sorted(&mut trees, base_root, &sorted, &mut problems)?;
    if let Some(problem) = first_problem(problems) {
        return Err(problem.into_error());
    }
    write_nodes(objects, &applied.nodes)?;
    Ok(applied.root)
}

/// The result of applying a change list, before anything is stored.
#[derive(Debug)]
pub(crate) struct Applied {
    /// The new root tree.
    pub(crate) root: ObjectId,
    /// Every node of every directory rebuilt, children before parents.
    pub(crate) nodes: Vec<(ObjectId, Vec<u8>)>,
}

/// A structural problem found while applying, at `path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Problem {
    pub(crate) path: Vec<u8>,
    pub(crate) problem: ApplyProblem,
}

impl Problem {
    pub(crate) fn into_error(self) -> Error {
        Error::Apply {
            path: self.path,
            problem: self.problem,
        }
    }
}

/// The problem with the smallest path, so that errors do not depend on the
/// order of the walk.
pub(crate) fn first_problem(problems: Vec<Problem>) -> Option<Problem> {
    problems.into_iter().min_by(|a, b| a.path.cmp(&b.path))
}

/// Checks every change on its own, and returns them sorted by path.
pub(crate) fn prepare(changes: &[Change]) -> Result<Vec<&Change>, Error> {
    for change in changes {
        let invalid = |problem| Error::Apply {
            path: change.path.clone(),
            problem,
        };
        validate_path(&change.path).map_err(|error| invalid(ApplyProblem::InvalidPath(error)))?;
        if matches!(change.op, ChangeOp::Symlink { .. })
            && forbidden_as_symlink(last_name(&change.path))
        {
            return Err(invalid(ApplyProblem::SymlinkName));
        }
    }
    let mut sorted: Vec<&Change> = changes.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(pair) = sorted.windows(2).find(|pair| pair[0].path == pair[1].path) {
        return Err(Error::Apply {
            path: pair[0].path.clone(),
            problem: ApplyProblem::Duplicate,
        });
    }
    Ok(sorted)
}

/// Applies valid changes with distinct paths, sorted by path, without storing
/// anything. Structural problems are collected in `problems` and the walk
/// goes on, so that a caller can report all of them; where there is one, the
/// result keeps the base entry.
pub(crate) fn apply_sorted<O: Objects>(
    trees: &mut Trees<'_, O>,
    base_root: ObjectId,
    changes: &[&Change],
    problems: &mut Vec<Problem>,
) -> Result<Applied, Error> {
    if changes.is_empty() {
        return Ok(Applied {
            root: base_root,
            nodes: Vec::new(),
        });
    }
    let items = changes
        .iter()
        .map(|change| Item {
            rest: &change.path,
            op: &change.op,
        })
        .collect();
    let mut ctx = Ctx {
        trees,
        nodes: Vec::new(),
        problems,
    };
    let root = match apply_dir(&mut ctx, b"", Some(base_root), items)? {
        Some(root) => root,
        None => {
            ctx.nodes.push((EMPTY_TREE, empty_tree_node()));
            EMPTY_TREE
        }
    };
    Ok(Applied {
        root,
        nodes: ctx.nodes,
    })
}

/// Stores rebuilt nodes.
pub(crate) fn write_nodes<O: Objects>(
    objects: &O,
    nodes: &[(ObjectId, Vec<u8>)],
) -> Result<(), Error> {
    for (_, bytes) in nodes {
        objects.put(Kind::Tree, bytes).map_err(store_error)?;
    }
    Ok(())
}

/// Checks that rebuilt nodes are stored, byte for byte.
pub(crate) fn check_nodes<O: Objects>(
    objects: &O,
    nodes: &[(ObjectId, Vec<u8>)],
) -> Result<(), Error> {
    for (id, bytes) in nodes {
        if read_object(objects, *id, Kind::Tree)? != *bytes {
            return Err(Error::Corrupt { id: *id });
        }
    }
    Ok(())
}

/// One change, seen from the directory being rebuilt.
#[derive(Clone, Copy)]
struct Item<'a> {
    /// The part of the path below the directory.
    rest: &'a [u8],
    op: &'a ChangeOp,
}

/// The changes under one name of a directory.
#[derive(Default)]
struct Group<'a> {
    /// The change to the name itself.
    direct: Option<&'a ChangeOp>,
    /// Changes below the name, which need it to be a directory.
    nested: Vec<Item<'a>>,
}

struct Ctx<'t, 'o, O> {
    trees: &'t mut Trees<'o, O>,
    nodes: Vec<(ObjectId, Vec<u8>)>,
    problems: &'t mut Vec<Problem>,
}

impl<O> Ctx<'_, '_, O> {
    /// Records a problem and returns `keep`, the entry to leave in place.
    fn problem(
        &mut self,
        path: Vec<u8>,
        problem: ApplyProblem,
        keep: Option<EntryKind>,
    ) -> Option<EntryKind> {
        self.problems.push(Problem { path, problem });
        keep
    }
}

/// Rebuilds the directory at `dir` (empty for the root), whose current tree
/// is `base` (`None` if it does not exist), with `items`. Returns the new
/// tree, or `None` if the directory ends up without entries.
fn apply_dir<'a, O: Objects>(
    ctx: &mut Ctx<'_, '_, O>,
    dir: &[u8],
    base: Option<ObjectId>,
    items: Vec<Item<'a>>,
) -> Result<Option<ObjectId>, Error> {
    let mut groups: BTreeMap<&'a [u8], Group<'a>> = BTreeMap::new();
    for item in items {
        match item.rest.iter().position(|byte| *byte == b'/') {
            None => groups.entry(item.rest).or_default().direct = Some(item.op),
            Some(slash) => groups
                .entry(&item.rest[..slash])
                .or_default()
                .nested
                .push(Item {
                    rest: &item.rest[slash + 1..],
                    ..item
                }),
        }
    }

    let existing = ctx.trees.read_or_empty(base)?;
    let mut entries = Vec::with_capacity(existing.len() + groups.len());
    let mut old = existing.iter().peekable();
    for (name, group) in groups {
        while let Some(entry) = old.next_if(|entry| entry.name.as_slice() < name) {
            entries.push(entry.clone());
        }
        let current = old
            .next_if(|entry| entry.name.as_slice() == name)
            .map(|entry| &entry.kind);
        if let Some(kind) = resolve(ctx, join(dir, name), current, group)? {
            entries.push(Entry {
                name: name.to_vec(),
                kind,
            });
        }
    }
    entries.extend(old.cloned());

    if entries.is_empty() {
        return Ok(None);
    }
    let directory = build_directory(entries).map_err(Error::Entries)?;
    let id = directory.root();
    ctx.nodes.extend(directory.nodes);
    Ok(Some(id))
}

/// The new entry for one name of a directory, given its `current` entry and
/// the changes to it and below it.
fn resolve<O: Objects>(
    ctx: &mut Ctx<'_, '_, O>,
    path: Vec<u8>,
    current: Option<&EntryKind>,
    group: Group<'_>,
) -> Result<Option<EntryKind>, Error> {
    let keep = current.cloned();
    let Group { direct, nested } = group;

    if nested.is_empty() {
        // A change to this name only. A group always holds a change.
        let Some(op) = direct else {
            return Ok(keep);
        };
        return Ok(match (op.to_entry(), current) {
            (None, None) => ctx.problem(path, ApplyProblem::DeleteMissing, keep),
            (None, Some(EntryKind::Tree { .. })) => {
                ctx.problem(path, ApplyProblem::IsADirectory, keep)
            }
            (None, Some(_)) => None,
            (Some(_), Some(EntryKind::Tree { tree })) if *tree != EMPTY_TREE => {
                ctx.problem(path, ApplyProblem::DirectoryNotEmpty, keep)
            }
            (Some(new), Some(old)) if new == *old => ctx.problem(path, ApplyProblem::NoOp, keep),
            (Some(new), _) => Some(new),
        });
    }

    // Changes below this name: it must be, or become, a directory. A file in
    // the way must be deleted by the same change list.
    let below = match current {
        Some(EntryKind::Tree { tree }) => Some(*tree),
        Some(_) if matches!(direct, Some(ChangeOp::Delete)) => None,
        Some(_) => return Ok(ctx.problem(path, ApplyProblem::NotADirectory, keep)),
        None => None,
    };
    let directory = apply_dir(ctx, &path, below, nested)?;
    Ok(match direct {
        None => directory.map(|tree| EntryKind::Tree { tree }),
        Some(ChangeOp::Delete) => match current {
            None => ctx.problem(path, ApplyProblem::DeleteMissing, keep),
            Some(EntryKind::Tree { .. }) => ctx.problem(path, ApplyProblem::IsADirectory, keep),
            // A file replaced by a directory.
            Some(_) => directory.map(|tree| EntryKind::Tree { tree }),
        },
        // A directory replaced by a file: nothing may be left below it.
        Some(op) => match directory {
            Some(_) => ctx.problem(path, ApplyProblem::DirectoryNotEmpty, keep),
            None => op.to_entry(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::read_tree;
    use crate::testing::{Repo, delete, gitlink, text_map};

    fn problem(result: Result<ObjectId, Error>) -> (String, ApplyProblem) {
        match result {
            Err(Error::Apply { path, problem }) => (String::from_utf8(path).unwrap(), problem),
            other => panic!("expected an apply error, got {other:?}"),
        }
    }

    #[test]
    fn creates_nested_directories_and_shares_untouched_subtrees() {
        let repo = Repo::new();
        let base = repo.tree(&[("a/b/c.txt", "1"), ("a/d.txt", "2"), ("z/keep.txt", "3")]);
        let new = apply(
            &repo.objects,
            base,
            &[repo.set("a/b/new/deep.txt", "4"), repo.set("a/d.txt", "5")],
        )
        .unwrap();
        assert_eq!(
            repo.files(new),
            text_map(&[
                ("a/b/c.txt", "1"),
                ("a/b/new/deep.txt", "4"),
                ("a/d.txt", "5"),
                ("z/keep.txt", "3"),
            ])
        );
        let subtree = |root, name: &str| {
            read_tree(&repo.objects, root)
                .unwrap()
                .into_iter()
                .find(|entry| entry.name == name.as_bytes())
                .unwrap()
                .kind
        };
        assert_eq!(subtree(base, "z"), subtree(new, "z"));
        assert_ne!(subtree(base, "a"), subtree(new, "a"));
    }

    #[test]
    fn order_of_changes_does_not_matter() {
        let repo = Repo::new();
        let base = repo.tree(&[("a", "1"), ("d/x", "2")]);
        let changes = vec![
            repo.set("d/y", "3"),
            delete("a"),
            repo.set("a-b", "4"),
            repo.set("a0", "5"),
        ];
        let forward = apply(&repo.objects, base, &changes).unwrap();
        let mut reversed = changes.clone();
        reversed.reverse();
        assert_eq!(apply(&repo.objects, base, &reversed).unwrap(), forward);
    }

    #[test]
    fn empty_directories_are_removed() {
        let repo = Repo::new();
        let base = repo.tree(&[("a/b/c/d.txt", "1"), ("a/keep.txt", "2")]);
        let new = apply(&repo.objects, base, &[delete("a/b/c/d.txt")]).unwrap();
        assert_eq!(repo.files(new), text_map(&[("a/keep.txt", "2")]));
        assert_eq!(new, repo.tree(&[("a/keep.txt", "2")]));
        // Deleting everything leaves the empty tree, stored.
        let gone = apply(&repo.objects, new, &[delete("a/keep.txt")]).unwrap();
        assert_eq!(gone, EMPTY_TREE);
        assert!(repo.objects.contains(&EMPTY_TREE).unwrap());
    }

    #[test]
    fn file_and_directory_replace_each_other() {
        let repo = Repo::new();
        let base = repo.tree(&[("p/x", "1"), ("p/y", "2"), ("q", "3")]);
        // A directory replaced by a file: every file below it is deleted.
        let new = apply(
            &repo.objects,
            base,
            &[
                delete("p/x"),
                delete("p/y"),
                repo.set("p", "4"),
                delete("q"),
                repo.set("q/z", "5"),
            ],
        )
        .unwrap();
        assert_eq!(repo.files(new), text_map(&[("p", "4"), ("q/z", "5")]));

        // Not deleting everything below is an error.
        let partial = apply(&repo.objects, base, &[delete("p/x"), repo.set("p", "4")]);
        assert_eq!(
            problem(partial),
            ("p".to_owned(), ApplyProblem::DirectoryNotEmpty)
        );
        // Neither is setting a file where a directory is.
        assert_eq!(
            problem(apply(&repo.objects, base, &[repo.set("p", "4")])),
            ("p".to_owned(), ApplyProblem::DirectoryNotEmpty)
        );
        // A change below a file that stays.
        assert_eq!(
            problem(apply(&repo.objects, base, &[repo.set("q/z", "5")])),
            ("q".to_owned(), ApplyProblem::NotADirectory)
        );
        assert_eq!(
            problem(apply(
                &repo.objects,
                base,
                &[repo.set("q", "6"), repo.set("q/z", "5")]
            )),
            ("q".to_owned(), ApplyProblem::NotADirectory)
        );
        // A file and files below it, both new.
        assert_eq!(
            problem(apply(
                &repo.objects,
                base,
                &[repo.set("n", "6"), repo.set("n/z", "5")]
            )),
            ("n".to_owned(), ApplyProblem::DirectoryNotEmpty)
        );
        // Gitlinks are not directories either.
        let with_link = apply(&repo.objects, base, &[gitlink("g", 7)]).unwrap();
        assert_eq!(
            problem(apply(&repo.objects, with_link, &[repo.set("g/file", "1")])),
            ("g".to_owned(), ApplyProblem::NotADirectory)
        );
    }

    #[test]
    fn an_empty_subdirectory_can_be_replaced_by_a_file() {
        let repo = Repo::new();
        let root = crate::access::write_tree(
            &repo.objects,
            vec![Entry {
                name: b"e".to_vec(),
                kind: EntryKind::Tree { tree: EMPTY_TREE },
            }],
        )
        .unwrap();
        repo.objects.put(Kind::Tree, &empty_tree_node()).unwrap();
        let new = apply(&repo.objects, root, &[repo.set("e", "1")]).unwrap();
        assert_eq!(repo.files(new), text_map(&[("e", "1")]));
    }

    #[test]
    fn rejects_invalid_change_lists() {
        let repo = Repo::new();
        let base = repo.tree(&[("a/b", "1"), ("c", "2")]);
        let cases: Vec<(Vec<Change>, &str, ApplyProblem)> = vec![
            (
                vec![delete("missing")],
                "missing",
                ApplyProblem::DeleteMissing,
            ),
            (vec![delete("x/y")], "x/y", ApplyProblem::DeleteMissing),
            (vec![delete("a")], "a", ApplyProblem::IsADirectory),
            (
                vec![delete("a"), delete("a/b")],
                "a",
                ApplyProblem::IsADirectory,
            ),
            (vec![repo.set("c", "2")], "c", ApplyProblem::NoOp),
            (vec![repo.set("a/b", "1")], "a/b", ApplyProblem::NoOp),
            (
                vec![repo.set("c", "3"), repo.set("c", "4")],
                "c",
                ApplyProblem::Duplicate,
            ),
            (
                vec![repo.symlink("d/.gitmodules", "x")],
                "d/.gitmodules",
                ApplyProblem::SymlinkName,
            ),
        ];
        for (changes, path, expected) in cases {
            assert_eq!(
                problem(apply(&repo.objects, base, &changes)),
                (path.to_owned(), expected),
                "{changes:?}"
            );
        }
        for bad in ["", "/a", "a/", "a//b", "a/../b", ".git/config"] {
            let (path, problem) = problem(apply(&repo.objects, base, &[repo.set(bad, "1")]));
            assert_eq!(path, bad);
            assert!(matches!(problem, ApplyProblem::InvalidPath(_)), "{bad}");
        }
        // Nothing was written for the failed attempts beyond blobs: the base
        // is unchanged and still readable.
        assert_eq!(repo.files(base), text_map(&[("a/b", "1"), ("c", "2")]));
    }

    #[test]
    fn kinds_can_change() {
        let repo = Repo::new();
        let base = repo.tree(&[("run", "#!/bin/sh\n")]);
        let exec = apply(&repo.objects, base, &[repo.exec("run", "#!/bin/sh\n")]).unwrap();
        assert_eq!(repo.files(exec), text_map(&[("run", "exec:#!/bin/sh\n")]));
        let link = apply(&repo.objects, exec, &[repo.symlink("run", "other")]).unwrap();
        assert_eq!(repo.files(link), text_map(&[("run", "link:other")]));
        let sub = apply(&repo.objects, link, &[gitlink("run", 9)]).unwrap();
        assert_eq!(repo.files(sub), text_map(&[("run", "gitlink:9")]));
    }

    #[test]
    fn split_directories_are_edited_in_place() {
        let repo = Repo::new();
        let files: Vec<(String, String)> = (0..2000)
            .map(|i| (format!("big/f{i:05}"), format!("{i}")))
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let base = repo.tree(&refs);
        let new = apply(
            &repo.objects,
            base,
            &[
                repo.set("big/f00010", "changed"),
                delete("big/f01999"),
                repo.set("big/zzz", "new"),
            ],
        )
        .unwrap();
        let mut expected: Vec<(&str, &str)> = refs.clone();
        expected[10].1 = "changed";
        expected.pop();
        expected.push(("big/zzz", "new"));
        assert_eq!(new, repo.tree(&expected));
        // Shrinking below the split limit gives a flat node again.
        let deletes: Vec<Change> = (0..1500).map(|i| delete(&format!("big/f{i:05}"))).collect();
        let small = apply(&repo.objects, base, &deletes).unwrap();
        assert_eq!(small, repo.tree(&refs[1500..]));
    }
}
