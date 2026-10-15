// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Bringing a working copy from one tree to another, writing only what
//! differs.
//!
//! A checkout is planned first and written second. Planning reads the two
//! trees and the working copy and refuses, before anything is written:
//!
//! - a name this platform cannot represent;
//! - two names of one directory that this file system would take for the
//!   same file (letter case, Unicode normalisation): neither is written;
//! - in checked mode, anything in the way that braid did not put there: an
//!   untracked file where a new file goes, or a file or symlink where a new
//!   directory goes.
//!
//! Writing then removes before it creates. Files that go away are removed
//! first, then the directories left empty, then new content is written:
//! each regular file into a scratch file in its directory, with mode 0644 or
//! 0755, renamed over its name; each symlink created with its target stored
//! as data and renamed into place; each gitlink as an empty directory. Every
//! step goes through directory handles that never follow a symlink (see
//! [`safe`](crate::safe)), and nothing found in the repository is ever run.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;

use braid_engine::{EMPTY_TREE, diff, entry_at, read_blob, read_tree};
use braid_format::{ChangeOp, EntryKind, ObjectId, Objects};

use crate::error::Error;
use crate::fsprobe::FileSystem;
use crate::paths::{ancestors, names, os_name, platform_problem, split_last};
use crate::safe::{Found, Root, StatKind, remove_entry, write_file, write_symlink};

/// Whether a checkout may overwrite what it finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Refuse when untracked data is in the way.
    Checked,
    /// Finish a checkout that was interrupted: the paths it covers are
    /// braid's to overwrite.
    Forced,
}

/// A checkout, validated and ready to write.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    /// Files, symlinks and gitlinks to remove, with what they were.
    removes: Vec<(Vec<u8>, ChangeOp)>,
    /// Directories that the target tree does not have, deepest first.
    empty_dirs: Vec<Vec<u8>>,
    /// What to write, by path in order.
    writes: Vec<(Vec<u8>, ChangeOp)>,
}

impl Plan {
    /// How many paths the checkout writes or removes.
    pub(crate) fn len(&self) -> usize {
        self.removes.len() + self.writes.len()
    }
}

/// Plans the checkout that turns the working copy of tree `from` into tree
/// `to`.
pub(crate) fn plan<O: Objects>(
    objects: &O,
    root: &Root,
    fs: FileSystem,
    from: ObjectId,
    to: ObjectId,
    mode: Mode,
) -> Result<Plan, Error> {
    let mut plan = Plan::default();
    if from == to {
        return Ok(plan);
    }
    // The diff the other way lists the same paths with what `from` has
    // there: one walk over the difference instead of a lookup per path.
    let olds: HashMap<Vec<u8>, ChangeOp> = if from == EMPTY_TREE {
        HashMap::new()
    } else {
        diff(objects, to, from)?
            .into_iter()
            .filter(|change| !matches!(change.op, ChangeOp::Delete))
            .map(|change| (change.path, change.op))
            .collect()
    };
    for change in diff(objects, from, to)? {
        match change.op {
            ChangeOp::Delete => {
                if let Some(old) = olds.get(&change.path) {
                    plan.removes.push((change.path, old.clone()));
                }
            }
            op => plan.writes.push((change.path, op)),
        }
    }

    // Directories the removals may leave behind, deepest first.
    let mut is_dir: HashMap<&[u8], bool> = HashMap::new();
    let mut gone = BTreeSet::new();
    for (path, _) in &plan.removes {
        for dir in ancestors(path) {
            let stays = match is_dir.get(dir) {
                Some(stays) => *stays,
                None => {
                    let stays = is_dir_in(objects, to, dir)?;
                    is_dir.insert(dir, stays);
                    stays
                }
            };
            if !stays {
                gone.insert(dir.to_vec());
            }
        }
    }
    plan.empty_dirs = gone.into_iter().collect();
    plan.empty_dirs.sort_by(|a, b| {
        let depth = |p: &[u8]| p.iter().filter(|byte| **byte == b'/').count();
        depth(b).cmp(&depth(a)).then_with(|| a.cmp(b))
    });

    for (path, _) in &plan.writes {
        for name in names(path) {
            if let Some(reason) = platform_problem(name) {
                return Err(Error::Unsafe {
                    path: path.clone(),
                    reason: reason.to_owned(),
                });
            }
        }
    }
    if fs.ignores_case || fs.ignores_normalization {
        check_collisions(objects, fs, to, &plan.writes)?;
    }
    if mode == Mode::Checked {
        check_obstacles(root, fs, &plan, &olds)?;
    }
    Ok(plan)
}

/// Whether `dir` is a directory of the tree `root`.
fn is_dir_in<O: Objects>(objects: &O, root: ObjectId, dir: &[u8]) -> Result<bool, Error> {
    Ok(matches!(
        entry_at(objects, root, dir)?,
        Some(EntryKind::Tree { .. })
    ))
}

/// Refuses two names of one directory of `to` that fold to the same name,
/// in every directory a write goes through.
fn check_collisions<O: Objects>(
    objects: &O,
    fs: FileSystem,
    to: ObjectId,
    writes: &[(Vec<u8>, ChangeOp)],
) -> Result<(), Error> {
    let mut checked: HashSet<Vec<u8>> = HashSet::new();
    for (path, _) in writes {
        let dirs = std::iter::once(&path[..0]).chain(ancestors(path));
        for dir in dirs {
            if !checked.insert(dir.to_vec()) {
                continue;
            }
            let tree = if dir.is_empty() {
                to
            } else {
                match entry_at(objects, to, dir)? {
                    Some(EntryKind::Tree { tree }) => tree,
                    _ => continue,
                }
            };
            let mut seen: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
            for entry in read_tree(objects, tree)? {
                let folded = fs.fold(&entry.name).into_owned();
                if let Some(first) = seen.insert(folded, entry.name.clone()) {
                    let at = |name: &[u8]| crate::paths::join(dir, name);
                    return Err(Error::Collision {
                        first: at(&first),
                        second: at(&entry.name),
                    });
                }
            }
        }
    }
    Ok(())
}

/// What the working copy has at a directory on the way to a write.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DirState {
    Present,
    Missing,
}

/// Refuses writes that would overwrite or go through something the old
/// tree did not have there.
fn check_obstacles(
    root: &Root,
    fs: FileSystem,
    plan: &Plan,
    olds: &HashMap<Vec<u8>, ChangeOp>,
) -> Result<(), Error> {
    let fold_path = |path: &[u8]| -> Vec<u8> {
        let mut out = Vec::with_capacity(path.len());
        for name in names(path) {
            if !out.is_empty() {
                out.push(b'/');
            }
            out.extend_from_slice(&fs.fold(name));
        }
        out
    };
    // What the plan removes, as this file system finds it.
    let removed: HashSet<Vec<u8>> = plan
        .removes
        .iter()
        .map(|(path, _)| fold_path(path))
        .collect();
    let emptied: HashSet<Vec<u8>> = plan.empty_dirs.iter().map(|path| fold_path(path)).collect();
    let mut dirs: HashMap<Vec<u8>, DirState> = HashMap::new();
    dirs.insert(Vec::new(), DirState::Present);

    for (path, op) in &plan.writes {
        let mut parent_state = DirState::Present;
        for dir in ancestors(path) {
            if let Some(state) = dirs.get(dir) {
                parent_state = *state;
                continue;
            }
            let state = if parent_state == DirState::Missing {
                DirState::Missing
            } else {
                match root.stat(dir)? {
                    Found::Here(stat) if stat.kind == StatKind::Dir => DirState::Present,
                    Found::Here(_) if removed.contains(&fold_path(dir)) => DirState::Missing,
                    Found::Here(_) => {
                        return Err(Error::Unsafe {
                            path: dir.to_vec(),
                            reason: "a file or symlink that braid does not track is where a directory goes"
                                .to_owned(),
                        });
                    }
                    Found::Missing | Found::Blocked(_) => DirState::Missing,
                }
            };
            dirs.insert(dir.to_vec(), state);
            parent_state = state;
        }
        if parent_state == DirState::Missing || olds.contains_key(path) {
            continue;
        }
        let Found::Here(stat) = root.stat(path)? else {
            continue;
        };
        let folded = fold_path(path);
        if removed.contains(&folded) {
            // A file or symlink the checkout removes first.
            continue;
        }
        let is_dir = stat.kind == StatKind::Dir;
        if is_dir && matches!(op, ChangeOp::Gitlink { .. }) {
            continue;
        }
        if is_dir && emptied.contains(&folded) {
            // A directory that this file replaces: it must hold nothing
            // but what the checkout removes.
            if let Some(untracked) = untracked_inside(root, path, &removed, &fold_path)? {
                return Err(Error::Unsafe {
                    path: untracked,
                    reason: format!(
                        "a file replaces the directory {:?}, and this untracked file in it would be lost",
                        crate::paths::show(path)
                    ),
                });
            }
            continue;
        }
        return Err(Error::Unsafe {
            path: path.clone(),
            reason: "an untracked file or directory is in the way; move it or remove it".to_owned(),
        });
    }
    Ok(())
}

/// The first entry under the directory `dir` of the working copy that is
/// not a directory and that the checkout does not remove, if there is one.
fn untracked_inside(
    root: &Root,
    dir: &[u8],
    removed: &HashSet<Vec<u8>>,
    fold_path: &dyn Fn(&[u8]) -> Vec<u8>,
) -> Result<Option<Vec<u8>>, Error> {
    let mut pending = vec![dir.to_vec()];
    while let Some(current) = pending.pop() {
        let Found::Here(handle) = root.open_dir(&current)? else {
            continue;
        };
        let entries = handle
            .entries()
            .map_err(|err| root.error("read", &current, err))?;
        for entry in entries {
            let entry = entry.map_err(|err| root.error("read", &current, err))?;
            let name = entry.file_name();
            let Some(bytes) = crate::paths::name_bytes(&name) else {
                return Ok(Some(current));
            };
            let path = crate::paths::join(&current, bytes);
            let meta = handle
                .symlink_metadata(&name)
                .map_err(|err| root.error("read", &path, err))?;
            if meta.is_dir() {
                pending.push(path);
            } else if !removed.contains(&fold_path(&path)) {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

/// Writes a planned checkout. Returns how many paths it wrote or removed.
pub(crate) fn apply<O: Objects>(objects: &O, root: &Root, plan: &Plan) -> Result<usize, Error> {
    for (path, old) in &plan.removes {
        let (parent, name) = split_last(path);
        let Found::Here(dir) = root.open_dir(parent)? else {
            continue;
        };
        let os = os_name(name).ok_or_else(|| unrepresentable(path))?;
        let removed = match old {
            // A submodule's directory goes only if it is empty.
            ChangeOp::Gitlink { .. } => match dir.remove_dir(os) {
                Err(err) if is_not_empty(&err) || err.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
            _ => remove_entry(&dir, os),
        };
        removed.map_err(|err| root.error("remove", path, err))?;
    }
    for path in &plan.empty_dirs {
        let (parent, name) = split_last(path);
        let Found::Here(dir) = root.open_dir(parent)? else {
            continue;
        };
        let os = os_name(name).ok_or_else(|| unrepresentable(path))?;
        match dir.remove_dir(os) {
            Ok(()) => {}
            // Untracked files keep it, or it is gone already, or it is not a
            // directory any more.
            Err(err)
                if is_not_empty(&err)
                    || err.kind() == io::ErrorKind::NotFound
                    || err.kind() == io::ErrorKind::NotADirectory => {}
            Err(err) => return Err(root.error("remove", path, err)),
        }
    }

    // Writes in path order, so a directory's files go out together.
    let mut by_dir: BTreeMap<&[u8], Vec<Write<'_>>> = BTreeMap::new();
    for (path, op) in &plan.writes {
        let (parent, name) = split_last(path);
        by_dir.entry(parent).or_default().push((path, name, op));
    }
    for (parent, files) in by_dir {
        let dir = root.create_dirs(parent)?;
        for (path, name, op) in files {
            let os = os_name(name).ok_or_else(|| unrepresentable(path))?;
            let written = match op {
                ChangeOp::File { blob, .. } | ChangeOp::Executable { blob, .. } => {
                    let content = read_blob(objects, *blob)?;
                    let executable = matches!(op, ChangeOp::Executable { .. });
                    write_file(&dir, os, &content, executable)
                }
                ChangeOp::Symlink { blob, .. } => {
                    let target = read_blob(objects, *blob)?;
                    write_symlink(&dir, os, &target)
                }
                ChangeOp::Gitlink { .. } => match dir.symlink_metadata(os) {
                    Ok(meta) if meta.is_dir() => Ok(()),
                    _ => remove_entry(&dir, os).and_then(|()| dir.create_dir(os)),
                },
                ChangeOp::Delete => Ok(()),
            };
            written.map_err(|err| match err.kind() {
                _ if is_not_empty(&err) => Error::Unsafe {
                    path: path.to_vec(),
                    reason: "a directory that still holds untracked files is where a file goes"
                        .to_owned(),
                },
                _ => root.error("write", path, err),
            })?;
        }
    }
    Ok(plan.len())
}

/// One write of a directory: the full path, the name in the directory, and
/// what to write.
type Write<'p> = (&'p [u8], &'p [u8], &'p ChangeOp);

fn is_not_empty(err: &io::Error) -> bool {
    // Some systems report a directory that is not empty as "exists".
    err.kind() == io::ErrorKind::DirectoryNotEmpty || err.kind() == io::ErrorKind::AlreadyExists
}

fn unrepresentable(path: &[u8]) -> Error {
    Error::Unsafe {
        path: path.to_vec(),
        reason: "this platform cannot represent the name".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_engine::write_tree;
    use braid_format::{Entry, Kind, MemoryObjects};

    fn file(objects: &MemoryObjects, name: &str, content: &str) -> Entry {
        Entry {
            name: name.as_bytes().to_vec(),
            kind: EntryKind::File {
                blob: objects.put(Kind::Blob, content.as_bytes()).unwrap(),
                size: content.len() as u64,
            },
        }
    }

    fn planned(
        objects: &MemoryObjects,
        fs: FileSystem,
        to: ObjectId,
    ) -> (tempfile::TempDir, Result<Plan, Error>) {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let plan = plan(objects, &root, fs, EMPTY_TREE, to, Mode::Checked);
        (dir, plan)
    }

    #[test]
    fn case_twins_collide_where_case_is_ignored() {
        let objects = MemoryObjects::new();
        let tree = write_tree(
            &objects,
            vec![file(&objects, "README", "a"), file(&objects, "readme", "b")],
        )
        .unwrap();
        let (_dir, plan) = planned(&objects, FileSystem::FORGIVING, tree);
        assert!(
            matches!(&plan, Err(Error::Collision { first, second })
                if first == b"README" && second == b"readme"),
            "{plan:?}"
        );
        // Where names are exact, both are fine.
        let (_dir, plan) = planned(&objects, FileSystem::default(), tree);
        assert_eq!(plan.unwrap().len(), 2);
    }

    #[test]
    fn normalisation_twins_collide_in_subdirectories() {
        let objects = MemoryObjects::new();
        let inner = write_tree(
            &objects,
            vec![
                file(&objects, "caf\u{e9}", "a"),
                file(&objects, "cafe\u{301}", "b"),
            ],
        )
        .unwrap();
        let tree = write_tree(
            &objects,
            vec![Entry {
                name: b"docs".to_vec(),
                kind: EntryKind::Tree { tree: inner },
            }],
        )
        .unwrap();
        let only_normalisation = FileSystem {
            ignores_case: false,
            ignores_normalization: true,
        };
        let (_dir, plan) = planned(&objects, only_normalisation, tree);
        assert!(matches!(plan, Err(Error::Collision { .. })), "{plan:?}");
    }

    #[test]
    fn a_symlink_and_a_directory_that_fold_together_are_refused() {
        // `dir` is a symlink and `DIR/x` a file: on a file system that
        // ignores case, writing `DIR/x` would go through the symlink.
        let objects = MemoryObjects::new();
        let x = file(&objects, "x", "pwned");
        let inner = write_tree(&objects, vec![x]).unwrap();
        let tree = write_tree(
            &objects,
            vec![
                Entry {
                    name: b"DIR".to_vec(),
                    kind: EntryKind::Tree { tree: inner },
                },
                Entry {
                    name: b"dir".to_vec(),
                    kind: EntryKind::Symlink {
                        blob: objects.put(Kind::Blob, b"/tmp").unwrap(),
                        size: 4,
                    },
                },
            ],
        )
        .unwrap();
        let (_dir, plan) = planned(&objects, FileSystem::FORGIVING, tree);
        assert!(matches!(plan, Err(Error::Collision { .. })), "{plan:?}");
    }
}
