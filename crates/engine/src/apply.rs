// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Applying a change list to a tree by path copying: only the directories on
//! changed paths are rebuilt; every other subtree is shared with the base
//! tree. In a large directory, split into nodes, only the nodes on the way to
//! the changed names are read and rewritten.

use std::collections::{BTreeMap, btree_map};

use braid_format::name::forbidden_as_symlink;
use braid_format::{
    Change, ChangeOp, EntryEdit, EntryKind, Kind, NewNode, ObjectId, Objects, validate_path,
};

use crate::access::{EMPTY_TREE, Trees, empty_tree_node, read_object};
use crate::error::{ApplyProblem, Error, store_error};
use crate::path::{check_depth, join, last_name};

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
    /// Every node the rebuilt directories did not have before, children
    /// before parents, each with the node of the same directory it most
    /// likely replaces: a hint for the store, which may keep it as a delta.
    pub(crate) nodes: Vec<NewNode>,
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
///
/// Paths deeper than [`MAX_PATH_DEPTH`](crate::MAX_PATH_DEPTH) are refused before the walk starts,
/// and the walk keeps its own stack of directories rather than recursing, so
/// it does not depend on the size of the thread's stack.
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
    for change in changes {
        check_depth(&change.path)?;
    }
    let items = changes
        .iter()
        .map(|change| Item {
            path: &change.path,
            rest: &change.path,
            op: &change.op,
        })
        .collect();
    let mut ctx = Ctx {
        trees,
        nodes: Vec::new(),
        problems,
    };
    let root = match rebuild(&mut ctx, base_root, items)? {
        Some(root) => root,
        None => {
            ctx.nodes.push(NewNode {
                id: EMPTY_TREE,
                bytes: empty_tree_node(),
                similar: None,
            });
            EMPTY_TREE
        }
    };
    Ok(Applied {
        root,
        nodes: ctx.nodes,
    })
}

/// Stores rebuilt nodes, each with the node it replaces as a hint.
pub(crate) fn write_nodes<O: Objects>(objects: &O, nodes: &[NewNode]) -> Result<(), Error> {
    for node in nodes {
        match &node.similar {
            Some(similar) => objects.put_similar(Kind::Tree, &node.bytes, similar),
            None => objects.put(Kind::Tree, &node.bytes),
        }
        .map_err(store_error)?;
    }
    Ok(())
}

/// Checks that rebuilt nodes are stored, byte for byte.
pub(crate) fn check_nodes<O: Objects>(objects: &O, nodes: &[NewNode]) -> Result<(), Error> {
    for node in nodes {
        if read_object(objects, node.id, Kind::Tree)? != node.bytes {
            return Err(Error::Corrupt { id: node.id });
        }
    }
    Ok(())
}

/// One change, seen from the directory being rebuilt.
#[derive(Clone, Copy)]
struct Item<'a> {
    /// The full path.
    path: &'a [u8],
    /// The part of the path below the directory.
    rest: &'a [u8],
    op: &'a ChangeOp,
}

impl<'a> Item<'a> {
    /// The path of the directory the change is seen from; empty for the root.
    fn dir(&self) -> &'a [u8] {
        let cut = self.path.len() - self.rest.len();
        &self.path[..cut.saturating_sub(1)]
    }
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
    nodes: Vec<NewNode>,
    problems: &'t mut Vec<Problem>,
}

impl<O> Ctx<'_, '_, O> {
    /// Records a problem at `dir/name` and returns `keep`, the entry to leave
    /// in place.
    fn problem(
        &mut self,
        dir: &[u8],
        name: &[u8],
        problem: ApplyProblem,
        keep: Option<EntryKind>,
    ) -> Option<EntryKind> {
        self.problems.push(Problem {
            path: join(dir, name),
            problem,
        });
        keep
    }
}

/// Rebuilds the root tree `base_root` with `items`, children before
/// parents, and returns the new root, or `None` if it has no entries left.
///
/// Each directory on a changed path is a frame on an explicit stack: a frame
/// resolves its names in order until one has to become a directory rebuilt
/// first, opens a frame for it, and settles that name once the frame above
/// it closes.
fn rebuild<O: Objects>(
    ctx: &mut Ctx<'_, '_, O>,
    base_root: ObjectId,
    items: Vec<Item<'_>>,
) -> Result<Option<ObjectId>, Error> {
    let mut stack = vec![Frame::open(Some(base_root), items)];
    // What the frame closed last became, for the frame below it.
    let mut closed = None;
    while let Some(frame) = stack.last_mut() {
        if let Some(directory) = closed.take() {
            frame.settle(ctx, directory);
        }
        match frame.advance(ctx)? {
            Some((below, nested)) => stack.push(Frame::open(below, nested)),
            None => {
                if let Some(frame) = stack.pop() {
                    closed = Some(frame.close(ctx)?);
                }
            }
        }
    }
    Ok(closed.flatten())
}

/// A directory being rebuilt.
struct Frame<'a> {
    /// Its path; empty for the root.
    dir: &'a [u8],
    /// Its tree before the change, if it existed.
    base: Option<ObjectId>,
    /// The changes under each name not resolved yet, in name order.
    groups: btree_map::IntoIter<&'a [u8], Group<'a>>,
    /// The names resolved so far whose entry changes, in name order.
    edits: Vec<EntryEdit>,
    /// The name whose directory the frame above is rebuilding.
    waiting: Option<Waiting<'a>>,
}

/// A directory to rebuild before the one above it: its tree before the
/// change, if it existed, and the changes below it.
type Below<'a> = (Option<ObjectId>, Vec<Item<'a>>);

/// A name that must be, or become, a directory, waiting for it to be rebuilt.
struct Waiting<'a> {
    name: &'a [u8],
    current: Option<EntryKind>,
    direct: Option<&'a ChangeOp>,
}

/// What to do with one name of a directory.
enum Step<'a> {
    /// This is its new entry; `None` leaves it out.
    Done(Option<EntryKind>),
    /// Its directory must be rebuilt first, from the tree `below` (if there
    /// is one) with the changes `nested`.
    Descend {
        below: Option<ObjectId>,
        nested: Vec<Item<'a>>,
        waiting: Waiting<'a>,
    },
}

impl<'a> Frame<'a> {
    /// Opens the directory whose tree is `base` (`None` if it does not
    /// exist), to rebuild it with `items`, of which there is at least one.
    /// Nothing is read until a name is resolved.
    fn open(base: Option<ObjectId>, items: Vec<Item<'a>>) -> Self {
        let dir = items.first().map_or(&b""[..], Item::dir);
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
        Frame {
            dir,
            base,
            groups: groups.into_iter(),
            edits: Vec::new(),
            waiting: None,
        }
    }

    /// Resolves names in order until one must be a directory rebuilt first,
    /// and returns what to rebuild it from.
    fn advance<O: Objects>(
        &mut self,
        ctx: &mut Ctx<'_, '_, O>,
    ) -> Result<Option<Below<'a>>, Error> {
        while let Some((name, group)) = self.groups.next() {
            let current = match self.base {
                Some(base) => ctx.trees.entry(base, name)?,
                None => None,
            };
            match plan(ctx, self.dir, name, current.clone(), group) {
                Step::Done(kind) => self.record(name, current, kind),
                Step::Descend {
                    below,
                    nested,
                    waiting,
                } => {
                    self.waiting = Some(waiting);
                    return Ok(Some((below, nested)));
                }
            }
        }
        Ok(None)
    }

    /// The frame above rebuilt the directory this one waits for, as
    /// `directory` (`None` if it has no entries left).
    fn settle<O>(&mut self, ctx: &mut Ctx<'_, '_, O>, directory: Option<ObjectId>) {
        if let Some(Waiting {
            name,
            current,
            direct,
        }) = self.waiting.take()
        {
            let kind = settle(ctx, self.dir, name, current.clone(), direct, directory);
            self.record(name, current, kind);
        }
    }

    /// Records the entry the name `name` gets, `kind`, if it is not the one
    /// it has, `current`.
    fn record(&mut self, name: &[u8], current: Option<EntryKind>, kind: Option<EntryKind>) {
        if kind != current {
            self.edits.push(EntryEdit {
                name: name.to_vec(),
                kind,
            });
        }
    }

    /// Finishes the directory, and returns its new tree, or `None` if it has
    /// no entries left. Only the nodes on the way to the names that changed
    /// are read and rewritten.
    fn close<O: Objects>(self, ctx: &mut Ctx<'_, '_, O>) -> Result<Option<ObjectId>, Error> {
        if self.edits.is_empty() {
            // Nothing changed: the directory stays as it was, unless it was
            // empty, as no braid tree holds an empty directory.
            return Ok(self.base.filter(|base| *base != EMPTY_TREE));
        }
        let updated = ctx
            .trees
            .update(self.base.unwrap_or(EMPTY_TREE), &self.edits)?;
        if updated.entries == 0 {
            return Ok(None);
        }
        ctx.nodes.extend(updated.nodes);
        Ok(Some(updated.root))
    }
}

/// Decides what happens to the name `name` of the directory `dir`, given its
/// `current` entry and the changes to it and below it.
fn plan<'a, O>(
    ctx: &mut Ctx<'_, '_, O>,
    dir: &[u8],
    name: &'a [u8],
    current: Option<EntryKind>,
    group: Group<'a>,
) -> Step<'a> {
    let Group { direct, nested } = group;

    if nested.is_empty() {
        // A change to this name only. A group always holds a change.
        let Some(op) = direct else {
            return Step::Done(current);
        };
        let new = op.to_entry();
        let problem = match (&new, &current) {
            (None, None) => Some(ApplyProblem::DeleteMissing),
            (None, Some(EntryKind::Tree { .. })) => Some(ApplyProblem::IsADirectory),
            (Some(_), Some(EntryKind::Tree { tree })) if *tree != EMPTY_TREE => {
                Some(ApplyProblem::DirectoryNotEmpty)
            }
            (Some(new), Some(old)) if new == old => Some(ApplyProblem::NoOp),
            _ => None,
        };
        return Step::Done(match problem {
            Some(problem) => ctx.problem(dir, name, problem, current),
            None => new,
        });
    }

    // Changes below this name: it must be, or become, a directory. A file in
    // the way must be deleted by the same change list.
    let below = match &current {
        Some(EntryKind::Tree { tree }) => Some(*tree),
        _ => None,
    };
    if current.is_some() && below.is_none() && !matches!(direct, Some(ChangeOp::Delete)) {
        return Step::Done(ctx.problem(dir, name, ApplyProblem::NotADirectory, current));
    }
    Step::Descend {
        below,
        nested,
        waiting: Waiting {
            name,
            current,
            direct,
        },
    }
}

/// The new entry for the name `name` of the directory `dir`, now that the
/// directory below it is rebuilt as `directory` (`None` if it has no entries
/// left).
fn settle<O>(
    ctx: &mut Ctx<'_, '_, O>,
    dir: &[u8],
    name: &[u8],
    current: Option<EntryKind>,
    direct: Option<&ChangeOp>,
    directory: Option<ObjectId>,
) -> Option<EntryKind> {
    let tree = directory.map(|tree| EntryKind::Tree { tree });
    let deletes = matches!(direct, Some(ChangeOp::Delete));
    let problem = match (&current, &tree) {
        (None, _) if deletes => Some(ApplyProblem::DeleteMissing),
        (Some(EntryKind::Tree { .. }), _) if deletes => Some(ApplyProblem::IsADirectory),
        // A directory replaced by a file: nothing may be left below it.
        (_, Some(_)) if direct.is_some() && !deletes => Some(ApplyProblem::DirectoryNotEmpty),
        _ => None,
    };
    if let Some(problem) = problem {
        return ctx.problem(dir, name, problem, current);
    }
    match direct {
        // The file that replaces the directory.
        Some(op) if !deletes => op.to_entry(),
        // The rebuilt directory, possibly replacing a deleted file, or
        // nothing if it has no entries left.
        _ => tree,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::{entry_at, read_tree};
    use crate::diff::diff;
    use crate::path::MAX_PATH_DEPTH;
    use crate::testing::{Counted, Hinted, Repo, deep_path, delete, gitlink, text_map};
    use braid_format::{Entry, MemoryObjects, build_directory};

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

    #[test]
    fn paths_up_to_the_depth_limit() {
        let repo = Repo::new();
        let deepest = deep_path(MAX_PATH_DEPTH);
        let root = apply(&repo.objects, EMPTY_TREE, &[repo.set(&deepest, "x")]).unwrap();
        assert_eq!(repo.files(root), text_map(&[(&deepest, "x")]));
        assert_eq!(
            entry_at(&repo.objects, root, deepest.as_bytes()).unwrap(),
            Some(repo.file_kind("x"))
        );
        // A sibling next to it, then everything deleted again.
        let sibling = deepest.replace("/file", "/other");
        let both = apply(&repo.objects, root, &[repo.set(&sibling, "y")]).unwrap();
        let gone = apply(&repo.objects, both, &[delete(&deepest), delete(&sibling)]).unwrap();
        assert_eq!(gone, EMPTY_TREE);

        // One name more is refused before anything is walked or stored.
        let deeper = deep_path(MAX_PATH_DEPTH + 1);
        let stored = repo.objects.len();
        let result = apply(
            &repo.objects,
            EMPTY_TREE,
            &[repo.set("a", "1"), repo.set(&deeper, "x")],
        );
        assert!(
            matches!(&result, Err(Error::TooDeep { path }) if *path == deeper.as_bytes()),
            "{result:?}"
        );
        assert_eq!(
            repo.objects.len(),
            stored + 1,
            "only the blob of `a` was added"
        );
        assert!(matches!(
            entry_at(&repo.objects, root, deeper.as_bytes()),
            Err(Error::TooDeep { .. })
        ));
    }

    fn file<O: Objects>(objects: &O, path: &str, content: &str) -> Change {
        Change {
            path: path.as_bytes().to_vec(),
            op: ChangeOp::File {
                blob: objects.put(Kind::Blob, content.as_bytes()).unwrap(),
                size: content.len() as u64,
            },
        }
    }

    fn tree_at(objects: &Hinted, root: ObjectId, path: &str) -> ObjectId {
        match entry_at(objects, root, path.as_bytes()).unwrap() {
            Some(EntryKind::Tree { tree }) => tree,
            other => panic!("{path} is {other:?}"),
        }
    }

    #[test]
    fn rebuilt_directories_name_the_ones_they_replace() {
        let objects = Hinted::default();
        let base = apply(
            &objects,
            EMPTY_TREE,
            &[
                file(&objects, "a/b/c.txt", "1"),
                file(&objects, "a/d.txt", "2"),
                file(&objects, "z/keep.txt", "3"),
            ],
        )
        .unwrap();
        // Directories that did not exist have nothing to be like.
        assert!(objects.hints.borrow().is_empty());

        let new = apply(&objects, base, &[file(&objects, "a/b/c.txt", "changed")]).unwrap();
        let mut hints = objects.hints.take();
        hints.sort();
        let mut expected: Vec<(ObjectId, ObjectId)> = ["", "a", "a/b"]
            .iter()
            .map(|path| match *path {
                "" => (new, base),
                path => (tree_at(&objects, new, path), tree_at(&objects, base, path)),
            })
            .collect();
        expected.sort();
        assert_eq!(hints, expected);
    }

    #[test]
    fn split_directories_pair_their_nodes() {
        let objects = Hinted::default();
        let files: Vec<Change> = (0..3000)
            .map(|i| file(&objects, &format!("big/f{i:05}"), "x"))
            .collect();
        let base = apply(&objects, EMPTY_TREE, &files).unwrap();
        let old_big = tree_at(&objects, base, "big");
        let old_nodes: Vec<ObjectId> = build_directory(read_tree(&objects, old_big).unwrap())
            .unwrap()
            .nodes
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert!(old_nodes.len() > 3);
        objects.hints.take();

        let new = apply(&objects, base, &[file(&objects, "big/f01500", "edited")]).unwrap();
        let new_big = tree_at(&objects, new, "big");
        let hints = objects.hints.take();
        // The root, the split directory's root, and the leaf that changed,
        // each paired with its old version.
        assert!(hints.contains(&(new, base)), "{hints:?}");
        assert!(hints.contains(&(new_big, old_big)), "{hints:?}");
        let below: Vec<&(ObjectId, ObjectId)> = hints.iter().filter(|(id, _)| *id != new).collect();
        assert!(below.len() >= 2, "{hints:?}");
        for (id, similar) in below {
            assert_ne!(id, similar);
            assert!(old_nodes.contains(similar), "{similar} is not an old node");
        }
    }

    /// Adding, changing or removing one file of a wide directory reads and
    /// writes a few nodes, whatever the number of files: the nodes on the
    /// way to it, and a neighbour where a boundary moves.
    #[test]
    fn one_file_of_a_wide_directory_costs_a_few_nodes() {
        const MOST_READ: usize = 6;
        const MOST_WRITTEN: usize = 5;
        let path = |i: usize| format!("wide/f{i:05}.txt");
        let mut costs = Vec::new();
        for count in [2_000usize, 20_000] {
            let objects = Counted::default();
            let files: Vec<Change> = (0..count)
                .map(|i| file(&objects, &path(i), &i.to_string()))
                .collect();
            let base = apply(&objects, EMPTY_TREE, &files).unwrap();
            let edits = [
                // A new file at the end, as the benchmark adds them.
                file(&objects, &path(count), "new"),
                file(&objects, &path(count / 2), "changed"),
                delete(&path(count / 3)),
                file(&objects, "wide/0-first", "new"),
            ];
            for edit in edits {
                objects.take();
                let new = apply(&objects, base, std::slice::from_ref(&edit)).unwrap();
                let (reads, writes) = objects.take();
                costs.push((count, reads, writes));

                // The same tree as building it from scratch.
                let scratch = MemoryObjects::new();
                let mut all: Vec<Change> = files
                    .iter()
                    .filter(|change| change.path != edit.path)
                    .cloned()
                    .collect();
                if edit.op != ChangeOp::Delete {
                    all.push(edit.clone());
                }
                assert_eq!(new, apply(&scratch, EMPTY_TREE, &all).unwrap());
            }
        }
        let most = |count| {
            costs
                .iter()
                .filter(|(c, ..)| *c == count)
                .fold((0, 0), |(r, w), (_, reads, writes)| {
                    (r.max(*reads), w.max(*writes))
                })
        };
        let (small, large) = (most(2_000), most(20_000));
        assert!(large.0 <= MOST_READ && large.1 <= MOST_WRITTEN, "{costs:?}");
        assert!(large.0 <= small.0 + 1, "{costs:?}");
        println!("files, objects read, objects written: {costs:?}");
    }

    #[test]
    fn deep_paths_need_little_stack() {
        // The walks keep their own stacks. At the depth limit they fit in a
        // small fraction of this thread stack, where recursing would need
        // several megabytes.
        let walk = || {
            let repo = Repo::new();
            let deepest = deep_path(MAX_PATH_DEPTH);
            let root = apply(&repo.objects, EMPTY_TREE, &[repo.set(&deepest, "x")]).unwrap();
            assert_eq!(
                diff(&repo.objects, EMPTY_TREE, root).unwrap(),
                [repo.set(&deepest, "x")]
            );
            assert_eq!(
                diff(&repo.objects, root, EMPTY_TREE).unwrap(),
                [delete(&deepest)]
            );
        };
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(walk)
            .unwrap()
            .join()
            .unwrap();
    }
}
