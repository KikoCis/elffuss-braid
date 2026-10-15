// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Landing a change on a branch: fast-forward when the change was built on
//! the head, otherwise rebase it onto the head path by path.
//!
//! The rebase looks at files, as the change list does. For each path the
//! change touches, it compares the file in the change's base (B), on the head
//! (H, "ours") and in the change (C, "theirs"); a directory counts as no
//! file. If H left the file as it was in B, C's value is taken; if H already
//! has C's value, there is nothing to do; otherwise both changed it, and only
//! two regular files can still be merged, line by line.
//!
//! Braid is stricter than git about the structure around those files. These
//! are conflicts too:
//!
//! - a file against a directory: C sets a file where H has a directory, or
//!   sets something below a path that H made a file;
//! - a directory that existed in B, removed by one side while the other added
//!   a file under it;
//! - a name that C adds next to a name that H added, when the two differ only
//!   in letter case or Unicode normalisation.
//!
//! The directory rule applies in both directions, so two changes that touch
//! unrelated paths land the same whichever goes first.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use braid_format::{
    Change, ChangeOp, ChangesetParts, Entry, EntryKind, Kind, ObjectId, Objects, Signature,
};
use unicode_normalization::UnicodeNormalization;

use crate::access::{Found, Trees, lookup, read_blob, read_changes, read_changeset};
use crate::ancestry::all_ancestors;
use crate::apply::{Applied, apply_sorted, write_nodes};
use crate::commit::store_assembled;
use crate::diff::diff_trees;
use crate::error::{ApplyProblem, Error, store_error};
use crate::merge::{ConflictHunk, MergeResult, merge3};
use crate::path::{check_depth, join};

/// The extra field a rebased changeset carries: the id, in hex, of the
/// changeset it was rebased from.
pub const REBASED_FROM: &[u8] = b"rebased-from";

/// What landing a change does to the branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandOutcome {
    /// The change was built on the head: the branch moves to the change
    /// itself.
    FastForward(ObjectId),
    /// The change was rebased onto the head as a new, stored changeset; the
    /// branch moves to it.
    Rebased {
        /// The new changeset.
        changeset: ObjectId,
        /// The paths both sides changed, merged line by line or by kind.
        merged_paths: Vec<Vec<u8>>,
    },
    /// The head already has everything the change does: the change itself
    /// is on the head's first-parent chain, or, rebased, it would change no
    /// file and every parent it has besides its base is already an ancestor
    /// of the head. Nothing was stored.
    AlreadyApplied,
    /// The change conflicts with the head. Nothing was stored, and the branch
    /// does not move.
    Conflicts(Vec<Conflict>),
}

/// One conflict found while rebasing a change onto the head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// Where the conflict is.
    pub path: Vec<u8>,
    /// What kind of conflict it is.
    pub kind: ConflictKind,
    /// The entry at `path` in the base of the change.
    pub base: Option<EntryKind>,
    /// The entry at `path` on the head of the branch.
    pub ours: Option<EntryKind>,
    /// The entry at `path` in the change being landed.
    pub theirs: Option<EntryKind>,
}

/// The kinds of conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictKind {
    /// Both sides changed the content, and the changes overlap. A conflict
    /// on binary files, symlink targets or gitlinks has no hunks.
    Content {
        /// The conflicting regions of a text file.
        hunks: Vec<ConflictHunk>,
    },
    /// One side deleted the file, the other changed it.
    ModifyDelete,
    /// The sides disagree on what the path is: a regular file, an executable,
    /// a symlink or a gitlink, and the base does not settle it.
    Mode,
    /// A file on one side, a directory on the other.
    DirectoryFile,
    /// A directory that existed in the base was removed on one side, and the
    /// other side added the file at `path` under it.
    DirectoryRemoved {
        /// The removed directory.
        directory: Vec<u8>,
    },
    /// The change adds a name that differs only in letter case or Unicode
    /// normalisation from a name the head added at `existing`.
    Case {
        /// The path of the name already on the head.
        existing: Vec<u8>,
    },
}

impl ConflictKind {
    /// A short, stable name for the kind, for reports and JSON:
    /// `CONTENT`, `MODIFY_DELETE`, `MODE`, `DF`, `DIR_REMOVED` or `CASE`.
    pub fn code(&self) -> &'static str {
        match self {
            ConflictKind::Content { .. } => "CONTENT",
            ConflictKind::ModifyDelete => "MODIFY_DELETE",
            ConflictKind::Mode => "MODE",
            ConflictKind::DirectoryFile => "DF",
            ConflictKind::DirectoryRemoved { .. } => "DIR_REMOVED",
            ConflictKind::Case { .. } => "CASE",
        }
    }

    fn rank(&self) -> u8 {
        match self {
            ConflictKind::Content { .. } => 0,
            ConflictKind::ModifyDelete => 1,
            ConflictKind::Mode => 2,
            ConflictKind::DirectoryFile => 3,
            ConflictKind::DirectoryRemoved { .. } => 4,
            ConflictKind::Case { .. } => 5,
        }
    }
}

/// Lands `change` on a branch whose head is `head` (`None` for a branch
/// without changesets).
///
/// The first parent of `change` is its base. The base must be on the head's
/// first-parent chain, or else the branch is empty and the change has no
/// parents; otherwise this fails with [`Error::BaseNotOnBranch`].
///
/// If `change` is already on the head's first-parent chain, it has landed
/// before, and the outcome is [`AlreadyApplied`](LandOutcome::AlreadyApplied).
/// If its base is the head, the branch fast-forwards to `change`, whatever it
/// contains. Otherwise the change is rebased onto the head: the new changeset
/// has the parents `[head] + change.parents[1..]`, the merged tree, the change
/// list against the head, the author, message and extra fields of `change`
/// (with [`REBASED_FROM`] naming it), and `committer`. When the merged tree is
/// the head's own, the outcome is `AlreadyApplied` only if every parent of
/// `change` after the first is already an ancestor of the head; otherwise the
/// new changeset is made anyway, with an empty change list, so that a merge
/// keeps its ancestry. Blobs, directories and the changeset are stored only
/// when the outcome is [`Rebased`](LandOutcome::Rebased).
///
/// This decides and writes objects, but does not move the branch: the caller
/// runs it inside its serialized section for the branch and publishes the
/// outcome. The cost follows the size of the change, except for two walks of
/// history: the one that checks the base, which follows its distance from the
/// head, and, for a merge that changes no file, the one that looks for its
/// other parents among the head's ancestors.
pub fn land<O: Objects>(
    objects: &O,
    head: Option<ObjectId>,
    change: ObjectId,
    committer: Signature,
) -> Result<LandOutcome, Error> {
    let theirs = read_changeset(objects, change)?;
    let (head_id, base_id) = match (head, theirs.parents.first().copied()) {
        (Some(head), _) if head == change => return Ok(LandOutcome::AlreadyApplied),
        (None, None) => return Ok(LandOutcome::FastForward(change)),
        (Some(head), Some(base)) if head == base => return Ok(LandOutcome::FastForward(change)),
        (Some(head), Some(base)) => match locate(objects, head, base, change)? {
            OnChain::Base => (head, base),
            // Landed already, by a fast-forward: nothing to do again.
            OnChain::Change => return Ok(LandOutcome::AlreadyApplied),
            OnChain::Neither => {
                return Err(Error::BaseNotOnBranch {
                    base: Some(base),
                    head: Some(head),
                });
            }
        },
        (head, base) => return Err(Error::BaseNotOnBranch { base, head }),
    };
    let ours = read_changeset(objects, head_id)?;
    let base = read_changeset(objects, base_id)?;

    let changes = read_changes(objects, change, &theirs)?;
    // Every walk below follows these paths: refuse deep ones first.
    for item in changes.iter() {
        check_depth(&item.path)?;
    }
    let mut rebase = Rebase::new(objects, base.root, ours.root, theirs.root);
    for item in changes.iter() {
        rebase.path(item)?;
    }
    let applied = rebase.structure()?;
    if !rebase.conflicts.is_empty() {
        return Ok(LandOutcome::Conflicts(rebase.into_conflicts()));
    }
    if applied.root == ours.root && all_ancestors(objects, &theirs.parents[1..], head_id)? {
        return Ok(LandOutcome::AlreadyApplied);
    }

    for (_, bytes) in &rebase.blobs {
        objects.put(Kind::Blob, bytes).map_err(store_error)?;
    }
    write_nodes(objects, &applied.nodes)?;
    let mut extras: Vec<(Vec<u8>, Vec<u8>)> = theirs
        .extras
        .into_iter()
        .filter(|(key, _)| key.as_slice() != REBASED_FROM)
        .collect();
    extras.push((REBASED_FROM.to_vec(), change.to_hex().into_bytes()));
    let parents = std::iter::once(head_id)
        .chain(theirs.parents.iter().skip(1).copied())
        .collect();
    let assembled = ChangesetParts {
        parents,
        root: applied.root,
        changes: rebase.resolved,
        author: theirs.author,
        committer,
        message: theirs.message,
        extras,
        git_origin: None,
    }
    .assemble()
    .map_err(Error::Assemble)?;
    store_assembled(objects, &assembled)?;
    Ok(LandOutcome::Rebased {
        changeset: assembled.id,
        merged_paths: rebase.merged,
    })
}

/// What the walk down the head's first-parent chain meets first.
enum OnChain {
    /// The change's base.
    Base,
    /// The change itself, which is above its base.
    Change,
    /// Neither, down to the first changeset.
    Neither,
}

/// Walks the first parents of `head` until it meets `base` or `change`. The
/// cost follows the distance from the head to the base.
fn locate<O: Objects>(
    objects: &O,
    head: ObjectId,
    base: ObjectId,
    change: ObjectId,
) -> Result<OnChain, Error> {
    let mut current = head;
    loop {
        if current == change {
            return Ok(OnChain::Change);
        }
        if current == base {
            return Ok(OnChain::Base);
        }
        match read_changeset(objects, current)?.parents.first() {
            Some(parent) => current = *parent,
            None => return Ok(OnChain::Neither),
        }
    }
}

/// A regular file: its blob, size and executable bit.
#[derive(Clone, Copy)]
struct Regular {
    blob: ObjectId,
    size: u64,
    executable: bool,
}

impl Regular {
    fn of(kind: &EntryKind) -> Option<Self> {
        match kind {
            EntryKind::File { blob, size } => Some(Regular {
                blob: *blob,
                size: *size,
                executable: false,
            }),
            EntryKind::Executable { blob, size } => Some(Regular {
                blob: *blob,
                size: *size,
                executable: true,
            }),
            _ => None,
        }
    }
}

/// The file part of an entry: `None` for a directory.
fn file(entry: Option<&EntryKind>) -> Option<EntryKind> {
    entry
        .filter(|kind| !matches!(kind, EntryKind::Tree { .. }))
        .cloned()
}

/// A name as file systems that ignore case and normalisation compare it:
/// Unicode NFC, lowercased. A name that is not UTF-8 is lowercased as ASCII.
fn fold(name: &[u8]) -> Vec<u8> {
    match core::str::from_utf8(name) {
        Ok(text) => text
            .nfc()
            .flat_map(char::to_lowercase)
            .nfc()
            .collect::<String>()
            .into_bytes(),
        Err(_) => name.to_ascii_lowercase(),
    }
}

/// The names of one directory, by folded name.
type FoldedNames = HashMap<Vec<u8>, Vec<Vec<u8>>>;

/// The state of one rebase. [`merge`](crate::merge) runs one too.
pub(crate) struct Rebase<'o, O> {
    trees: Trees<'o, O>,
    /// Root trees of the base, the head and the change.
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
    /// The change list against the head, in path order.
    pub(crate) resolved: Vec<Change>,
    /// Paths merged from both sides.
    pub(crate) merged: Vec<Vec<u8>>,
    /// Merged contents to store.
    pub(crate) blobs: Vec<(ObjectId, Vec<u8>)>,
    /// Paths the change adds that had no file in the base or on the head.
    added: Vec<Vec<u8>>,
    /// Paths the change deletes.
    deleted: Vec<Vec<u8>>,
    pub(crate) conflicts: Vec<Conflict>,
    /// For each directory of the head, its names by folded name.
    folded: HashMap<ObjectId, Rc<FoldedNames>>,
    /// Names the change adds that were checked for collisions already.
    case_checked: HashSet<Vec<u8>>,
}

impl<'o, O: Objects> Rebase<'o, O> {
    pub(crate) fn new(objects: &'o O, base: ObjectId, ours: ObjectId, theirs: ObjectId) -> Self {
        Rebase {
            trees: Trees::new(objects),
            base,
            ours,
            theirs,
            resolved: Vec::new(),
            merged: Vec::new(),
            blobs: Vec::new(),
            added: Vec::new(),
            deleted: Vec::new(),
            conflicts: Vec::new(),
            folded: HashMap::new(),
            case_checked: HashSet::new(),
        }
    }

    /// Records a conflict at `path`, with what each side has there.
    fn conflict(&mut self, path: &[u8], kind: ConflictKind) -> Result<(), Error> {
        let base = self.trees.find(self.base, path)?.into_entry();
        let ours = self.trees.find(self.ours, path)?.into_entry();
        let theirs = self.trees.find(self.theirs, path)?.into_entry();
        self.conflicts.push(Conflict {
            path: path.to_vec(),
            kind,
            base,
            ours,
            theirs,
        });
        Ok(())
    }

    /// Resolves one path of the change list.
    pub(crate) fn path(&mut self, change: &Change) -> Result<(), Error> {
        let path = change.path.as_slice();
        if matches!(change.op, ChangeOp::Delete) {
            self.deleted.push(path.to_vec());
        }
        let in_base = self.trees.find(self.base, path)?;
        let in_ours = self.trees.find(self.ours, path)?;
        let base_file = file(in_base.entry());
        let ours_file = file(in_ours.entry());
        let theirs_file = change.op.to_entry();

        if theirs_file == ours_file {
            // The head already has the change's value.
            return Ok(());
        }
        if base_file == ours_file {
            // The head left this file alone: take the change's value.
            if base_file.is_none() {
                self.added.push(path.to_vec());
            }
            self.resolved.push(change.clone());
            return Ok(());
        }
        // Both sides changed this file, differently.
        match (ours_file, theirs_file) {
            (Some(ours), Some(theirs)) => self.merge(path, base_file, ours, theirs),
            (None, Some(_)) => match in_ours {
                // The head replaced the file with a directory.
                Found::Entry(EntryKind::Tree { .. }) => {
                    self.conflict(path, ConflictKind::DirectoryFile)
                }
                // The head made a directory above it a file.
                Found::Blocked(at) => self.conflict(&path[..at], ConflictKind::DirectoryFile),
                _ => self.conflict(path, ConflictKind::ModifyDelete),
            },
            (Some(_), None) | (None, None) => self.conflict(path, ConflictKind::ModifyDelete),
        }
    }

    /// Both sides changed the file at `path` to different entries.
    fn merge(
        &mut self,
        path: &[u8],
        base: Option<EntryKind>,
        ours: EntryKind,
        theirs: EntryKind,
    ) -> Result<(), Error> {
        let (Some(o), Some(t)) = (Regular::of(&ours), Regular::of(&theirs)) else {
            // Symlinks and gitlinks have no lines to merge.
            let kind = if ours.code() == theirs.code() {
                ConflictKind::Content { hunks: Vec::new() }
            } else {
                ConflictKind::Mode
            };
            return self.conflict(path, kind);
        };
        let b = base.as_ref().and_then(Regular::of);

        // The executable bit merges on its own: a side that kept the base's
        // bit yields to the other.
        let executable = match b {
            _ if o.executable == t.executable => Some(o.executable),
            Some(b) if b.executable == o.executable => Some(t.executable),
            Some(b) if b.executable == t.executable => Some(o.executable),
            _ => None,
        };

        let mut content = None;
        let mut hunks = None;
        if o.blob == t.blob {
            content = Some((o.blob, o.size, None));
        } else {
            let objects = self.trees.objects();
            let base_bytes = match b {
                Some(b) => read_blob(objects, b.blob)?,
                // Added on both sides, or a regular file on both sides only now.
                None => Vec::new(),
            };
            let ours_bytes = read_blob(objects, o.blob)?;
            let theirs_bytes = read_blob(objects, t.blob)?;
            match merge3(&base_bytes, &ours_bytes, &theirs_bytes) {
                MergeResult::Clean(bytes) => {
                    let blob = ObjectId::hash(Kind::Blob, &bytes);
                    content = Some((blob, bytes.len() as u64, Some(bytes)));
                }
                MergeResult::Conflict { hunks: found } => hunks = Some(found),
            }
        }

        if executable.is_none() {
            self.conflict(path, ConflictKind::Mode)?;
        }
        if let Some(hunks) = hunks {
            self.conflict(path, ConflictKind::Content { hunks })?;
        }
        let (Some(executable), Some((blob, size, bytes))) = (executable, content) else {
            return Ok(());
        };
        let op = if executable {
            ChangeOp::Executable { blob, size }
        } else {
            ChangeOp::File { blob, size }
        };
        if op.to_entry().as_ref() == Some(&ours) {
            // The merge gives what the head has.
            return Ok(());
        }
        if let Some(bytes) = bytes {
            self.blobs.push((blob, bytes));
        }
        self.resolved.push(Change {
            path: path.to_vec(),
            op,
        });
        self.merged.push(path.to_vec());
        Ok(())
    }

    /// Checks the structure around the resolved changes, applies them to the
    /// head without storing anything, and turns structural problems into
    /// conflicts.
    pub(crate) fn structure(&mut self) -> Result<Applied, Error> {
        self.removed_under_added()?;
        self.added_under_removed()?;
        let sets: Vec<Vec<u8>> = self
            .resolved
            .iter()
            .filter(|change| !matches!(change.op, ChangeOp::Delete))
            .map(|change| change.path.clone())
            .collect();
        for path in sets {
            self.check_case(&path)?;
        }

        let resolved = std::mem::take(&mut self.resolved);
        let sorted: Vec<&Change> = resolved.iter().collect();
        let mut problems = Vec::new();
        let applied = apply_sorted(&mut self.trees, self.ours, &sorted, &mut problems)?;
        self.resolved = resolved;
        for problem in problems {
            match problem.problem {
                ApplyProblem::NotADirectory | ApplyProblem::DirectoryNotEmpty => {
                    self.conflict(&problem.path, ConflictKind::DirectoryFile)?;
                }
                // The resolved list only deletes files the head has and only
                // sets what the head does not have, so nothing else can fail
                // unless the change itself is not canonical.
                _ => return Err(problem.into_error()),
            }
        }
        Ok(applied)
    }

    /// The change adds a file under a directory that existed in the base and
    /// that the head removed.
    fn removed_under_added(&mut self) -> Result<(), Error> {
        for path in std::mem::take(&mut self.added) {
            let deepest = self.trees.deepest_directory(self.base, &path)?;
            if deepest == 0 {
                continue;
            }
            let directory = &path[..deepest];
            if self.trees.find(self.ours, directory)? == Found::Absent {
                self.conflict(
                    &path,
                    ConflictKind::DirectoryRemoved {
                        directory: directory.to_vec(),
                    },
                )?;
            }
        }
        Ok(())
    }

    /// The change removes a directory that existed in the base, and the head
    /// added files under it.
    fn added_under_removed(&mut self) -> Result<(), Error> {
        let mut removed = BTreeSet::new();
        for path in std::mem::take(&mut self.deleted) {
            if let Some(len) = self.trees.first_missing_directory(self.theirs, &path)? {
                removed.insert(path[..len].to_vec());
            }
        }
        for directory in removed {
            let before = self.trees.find(self.base, &directory)?;
            let after = self.trees.find(self.ours, &directory)?;
            let (
                Found::Entry(EntryKind::Tree { tree: before }),
                Found::Entry(EntryKind::Tree { tree: after }),
            ) = (before, after)
            else {
                continue;
            };
            for item in diff_trees(&mut self.trees, &directory, Some(before), Some(after))? {
                if item.old.is_none() && item.new.is_some() {
                    self.conflict(
                        &item.path,
                        ConflictKind::DirectoryRemoved {
                            directory: directory.clone(),
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    /// The change sets `path`. Find the first name on it that the head does
    /// not have; if the head added a name there that folds to the same name,
    /// the two would collide on a file system that ignores case or
    /// normalisation.
    fn check_case(&mut self, path: &[u8]) -> Result<(), Error> {
        let mut dir = self.ours;
        let mut start = 0;
        loop {
            let slash = path[start..].iter().position(|byte| *byte == b'/');
            let end = slash.map_or(path.len(), |slash| start + slash);
            let name = &path[start..end];
            let entries = self.trees.read(dir)?;
            match (lookup(&entries, name), slash) {
                (Some(EntryKind::Tree { tree }), Some(_)) => {
                    dir = *tree;
                    start = end + 1;
                }
                // The head has this name already.
                (Some(_), _) => return Ok(()),
                (None, _) => {
                    if !self.case_checked.insert(path[..end].to_vec()) {
                        // Another path of the change goes through this name.
                        return Ok(());
                    }
                    let parent = &path[..start.saturating_sub(1)];
                    let index = self.folded_names(dir, &entries);
                    let others = index.get(&fold(name)).cloned().unwrap_or_default();
                    for other in others {
                        // Only a name the head brought counts: one already in
                        // the base, or one the change has too, makes the
                        // collision the change's own.
                        let existing = join(parent, &other);
                        let in_base = self.trees.find(self.base, &existing)?;
                        let in_change = self.trees.find(self.theirs, &existing)?;
                        if in_base.entry().is_none() && in_change.entry().is_none() {
                            self.conflict(&path[..end], ConflictKind::Case { existing })?;
                        }
                    }
                    return Ok(());
                }
            }
        }
    }

    /// The names of a directory of the head, by folded name.
    fn folded_names(&mut self, dir: ObjectId, entries: &[Entry]) -> Rc<FoldedNames> {
        Rc::clone(self.folded.entry(dir).or_insert_with(|| {
            let mut index = FoldedNames::new();
            for entry in entries {
                index
                    .entry(fold(&entry.name))
                    .or_default()
                    .push(entry.name.clone());
            }
            Rc::new(index)
        }))
    }

    /// The conflicts, sorted by path and kind, without repeats.
    pub(crate) fn into_conflicts(self) -> Vec<Conflict> {
        let mut conflicts = self.conflicts;
        conflicts.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.kind.rank().cmp(&b.kind.rank()))
        });
        conflicts.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
        conflicts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::{admit, store_assembled};
    use crate::diff::diff;
    use crate::merge::HunkSide;
    use crate::path::MAX_PATH_DEPTH;
    use crate::testing::{Repo, deep_path, delete, gitlink, signature, text_map};
    use braid_format::Changes;

    struct Setup {
        repo: Repo,
        base: ObjectId,
        head: ObjectId,
        change: ObjectId,
    }

    /// A base with `files`, a head that applies `ours` to it, and a change
    /// that applies `theirs` to it.
    fn setup(
        files: &[(&str, &str)],
        ours: impl FnOnce(&Repo) -> Vec<Change>,
        theirs: impl FnOnce(&Repo) -> Vec<Change>,
    ) -> Setup {
        let repo = Repo::new();
        let start: Vec<Change> = files.iter().map(|(p, c)| repo.set(p, c)).collect();
        let base = repo.commit(&[], start);
        let head = repo.commit(&[base], ours(&repo));
        let change = repo.commit(&[base], theirs(&repo));
        Setup {
            repo,
            base,
            head,
            change,
        }
    }

    fn land_it(setup: &Setup) -> LandOutcome {
        land(
            &setup.repo.objects,
            Some(setup.head),
            setup.change,
            signature("lander"),
        )
        .unwrap()
    }

    fn rebased(setup: &Setup) -> (ObjectId, Vec<Vec<u8>>) {
        match land_it(setup) {
            LandOutcome::Rebased {
                changeset,
                merged_paths,
            } => {
                // Whatever lands is admissible, and its list is the diff.
                let objects = &setup.repo.objects;
                admit(objects, changeset, |id| *id == setup.head).unwrap();
                let new = read_changeset(objects, changeset).unwrap();
                let list = read_changes(objects, changeset, &new).unwrap().into_owned();
                let head_root = setup.repo.root(setup.head);
                assert_eq!(list, diff(objects, head_root, new.root).unwrap());
                (changeset, merged_paths)
            }
            other => panic!("expected a rebase, got {other:?}"),
        }
    }

    fn conflicts(setup: &Setup) -> Vec<(String, &'static str)> {
        match land_it(setup) {
            LandOutcome::Conflicts(conflicts) => conflicts
                .iter()
                .map(|c| (String::from_utf8(c.path.clone()).unwrap(), c.kind.code()))
                .collect(),
            other => panic!("expected conflicts, got {other:?}"),
        }
    }

    fn pairs(expected: &[(&str, &'static str)]) -> Vec<(String, &'static str)> {
        expected
            .iter()
            .map(|(path, code)| (path.to_string(), *code))
            .collect()
    }

    #[test]
    fn fast_forward_when_the_base_is_the_head() {
        let s = setup(&[("a", "1")], |r| vec![r.set("a", "2")], |_| Vec::new());
        let on_head = s.repo.commit(&[s.head], vec![s.repo.set("b", "3")]);
        assert_eq!(
            land(&s.repo.objects, Some(s.head), on_head, signature("x")).unwrap(),
            LandOutcome::FastForward(on_head)
        );
        // Even a change that changes nothing.
        assert_eq!(
            land(&s.repo.objects, Some(s.base), s.change, signature("x")).unwrap(),
            LandOutcome::FastForward(s.change)
        );
        // The first changeset of an empty branch.
        let root = s.repo.commit(&[], vec![s.repo.set("r", "1")]);
        assert_eq!(
            land(&s.repo.objects, None, root, signature("x")).unwrap(),
            LandOutcome::FastForward(root)
        );
    }

    #[test]
    fn landing_again_is_already_applied() {
        let s = setup(&[("a", "1")], |r| vec![r.set("a", "2")], |_| Vec::new());
        let on_head = s.repo.commit(&[s.head], vec![s.repo.set("b", "3")]);
        let objects = &s.repo.objects;
        // The change is the head.
        assert_eq!(
            land(objects, Some(on_head), on_head, signature("x")).unwrap(),
            LandOutcome::AlreadyApplied
        );
        // The change is further down the first-parent chain.
        let later = s.repo.commit(&[on_head], vec![s.repo.set("b", "4")]);
        assert_eq!(
            land(objects, Some(later), on_head, signature("x")).unwrap(),
            LandOutcome::AlreadyApplied
        );
        // A first changeset that is the head.
        let root = s.repo.commit(&[], vec![s.repo.set("r", "1")]);
        assert_eq!(
            land(objects, Some(root), root, signature("x")).unwrap(),
            LandOutcome::AlreadyApplied
        );
    }

    #[test]
    fn merges_that_change_no_file_keep_their_ancestry() {
        let repo = Repo::new();
        let base = repo.commit(&[], vec![repo.set("a", "1")]);
        let head = repo.commit(&[base], vec![repo.set("h", "1")]);
        let side = repo.commit(&[base], vec![repo.set("s", "1")]);
        // Merges `side` but keeps none of its files: no change against `base`.
        let merge = commit_with_extras(&repo, vec![base, side], Vec::new(), Vec::new());
        let objects = &repo.objects;

        let outcome = land(objects, Some(head), merge, signature("x")).unwrap();
        let LandOutcome::Rebased {
            changeset,
            merged_paths,
        } = outcome
        else {
            panic!("expected a rebase, got {outcome:?}");
        };
        assert!(merged_paths.is_empty());
        let new = read_changeset(objects, changeset).unwrap();
        assert_eq!(new.parents, [head, side]);
        assert_eq!(new.root, repo.root(head));
        assert_eq!(new.changes, Changes::Inline(Vec::new()));
        admit(objects, changeset, |id| [head, side].contains(id)).unwrap();

        // Landed again on the new head, the merge has nothing left to add.
        assert_eq!(
            land(objects, Some(changeset), merge, signature("x")).unwrap(),
            LandOutcome::AlreadyApplied
        );
        // Nor has another merge of the same side, once `side` is in history.
        let again = commit_with_extras(
            &repo,
            vec![base, side],
            Vec::new(),
            vec![(b"note".to_vec(), b"again".to_vec())],
        );
        assert_eq!(
            land(objects, Some(changeset), again, signature("x")).unwrap(),
            LandOutcome::AlreadyApplied
        );
        // A merge built on the head fast-forwards, changes or not.
        let on_head = commit_with_extras(&repo, vec![head, side], Vec::new(), Vec::new());
        assert_eq!(
            land(objects, Some(head), on_head, signature("x")).unwrap(),
            LandOutcome::FastForward(on_head)
        );
    }

    #[test]
    fn landing_respects_the_depth_limit() {
        let deepest = deep_path(MAX_PATH_DEPTH);
        let s = setup(
            &[("a", "1")],
            |r| vec![r.set("a", "2")],
            |r| vec![r.set(&deepest, "c")],
        );
        let (id, _) = rebased(&s);
        assert_eq!(
            s.repo.files(s.repo.root(id)),
            text_map(&[("a", "2"), (&deepest, "c")])
        );

        // A change one name deeper, made by hand since `commit` refuses it.
        let forged = ChangesetParts {
            parents: vec![s.base],
            root: s.repo.root(s.base),
            changes: vec![s.repo.set(&deep_path(MAX_PATH_DEPTH + 1), "c")],
            author: signature("a"),
            committer: signature("a"),
            message: Vec::new(),
            extras: Vec::new(),
            git_origin: None,
        }
        .assemble()
        .unwrap();
        store_assembled(&s.repo.objects, &forged).unwrap();
        let result = land(&s.repo.objects, Some(s.head), forged.id, signature("x"));
        assert!(matches!(result, Err(Error::TooDeep { .. })), "{result:?}");
    }

    #[test]
    fn base_must_be_on_the_branch() {
        let s = setup(
            &[("a", "1")],
            |r| vec![r.set("a", "2")],
            |r| vec![r.set("b", "1")],
        );
        let objects = &s.repo.objects;
        // A change built on a changeset that is not on the branch.
        let on_side = s.repo.commit(&[s.change], vec![s.repo.set("c", "1")]);
        assert!(matches!(
            land(objects, Some(s.head), on_side, signature("x")),
            Err(Error::BaseNotOnBranch { .. })
        ));
        // A change with parents on an empty branch, and a root change on a
        // branch with a head.
        assert!(matches!(
            land(objects, None, s.change, signature("x")),
            Err(Error::BaseNotOnBranch { head: None, .. })
        ));
        let root = s.repo.commit(&[], vec![s.repo.set("r", "1")]);
        assert!(matches!(
            land(objects, Some(s.head), root, signature("x")),
            Err(Error::BaseNotOnBranch { base: None, .. })
        ));
        // A base further down the first-parent chain is fine.
        let later = s.repo.commit(&[s.head], vec![s.repo.set("d", "1")]);
        assert!(matches!(
            land(objects, Some(later), s.change, signature("x")).unwrap(),
            LandOutcome::Rebased { .. }
        ));
    }

    #[test]
    fn disjoint_changes_rebase_cleanly() {
        let s = setup(
            &[("a", "1"), ("d/b", "2")],
            |r| vec![r.set("a", "head"), r.set("d/new", "h")],
            |r| vec![r.set("d/b", "change"), r.set("e/f", "c")],
        );
        let (id, merged) = rebased(&s);
        assert!(merged.is_empty());
        assert_eq!(
            s.repo.files(s.repo.root(id)),
            text_map(&[
                ("a", "head"),
                ("d/b", "change"),
                ("d/new", "h"),
                ("e/f", "c")
            ])
        );
        let new = read_changeset(&s.repo.objects, id).unwrap();
        let original = read_changeset(&s.repo.objects, s.change).unwrap();
        assert_eq!(new.parents, [s.head]);
        assert_eq!(new.author, original.author);
        assert_eq!(new.message, original.message);
        assert_eq!(new.committer, signature("lander"));
        assert_eq!(
            new.extras,
            [(REBASED_FROM.to_vec(), s.change.to_hex().into_bytes())]
        );
        assert_eq!(new.git_origin, None);
    }

    #[test]
    fn rebasing_keeps_extra_parents_and_replaces_rebased_from() {
        let repo = Repo::new();
        let base = repo.commit(&[], vec![repo.set("a", "1")]);
        let head = repo.commit(&[base], vec![repo.set("h", "1")]);
        let other = repo.commit(&[base], vec![repo.set("o", "1")]);
        let merge = commit_with_extras(
            &repo,
            vec![base, other],
            vec![repo.set("m", "1")],
            vec![
                (b"keep".to_vec(), b"me".to_vec()),
                (REBASED_FROM.to_vec(), b"old".to_vec()),
            ],
        );
        let outcome = land(&repo.objects, Some(head), merge, signature("x")).unwrap();
        let LandOutcome::Rebased { changeset, .. } = outcome else {
            panic!("expected a rebase, got {outcome:?}");
        };
        let new = read_changeset(&repo.objects, changeset).unwrap();
        assert_eq!(new.parents, [head, other]);
        assert_eq!(
            new.extras,
            [
                (b"keep".to_vec(), b"me".to_vec()),
                (REBASED_FROM.to_vec(), merge.to_hex().into_bytes()),
            ]
        );
    }

    fn commit_with_extras(
        repo: &Repo,
        parents: Vec<ObjectId>,
        changes: Vec<Change>,
        extras: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> ObjectId {
        crate::commit::commit(
            &repo.objects,
            parents,
            changes,
            signature("author"),
            signature("author"),
            b"merge\n".to_vec(),
            extras,
        )
        .unwrap()
    }

    #[test]
    fn both_sides_editing_a_file_merge_line_by_line() {
        let s = setup(
            &[("f", "1\n2\n3\n4\n5\n")],
            |r| vec![r.set("f", "ONE\n2\n3\n4\n5\n")],
            |r| vec![r.set("f", "1\n2\n3\n4\nFIVE\n")],
        );
        let (id, merged) = rebased(&s);
        assert_eq!(merged, [b"f".to_vec()]);
        assert_eq!(
            s.repo.files(s.repo.root(id)),
            text_map(&[("f", "ONE\n2\n3\n4\nFIVE\n")])
        );
    }

    #[test]
    fn executable_bit_and_content_merge_separately() {
        let s = setup(
            &[("run", "1\n2\n3\n")],
            |r| vec![r.exec("run", "1\n2\n3\n")],
            |r| vec![r.set("run", "1\n2\nthree\n")],
        );
        let (id, merged) = rebased(&s);
        assert_eq!(merged, [b"run".to_vec()]);
        assert_eq!(
            s.repo.files(s.repo.root(id)),
            text_map(&[("run", "exec:1\n2\nthree\n")])
        );
    }

    #[test]
    fn already_applied() {
        let s = setup(
            &[("a", "1"), ("b", "2")],
            |r| vec![r.set("a", "new"), delete("b"), r.set("c", "3")],
            |r| vec![r.set("a", "new"), delete("b")],
        );
        assert_eq!(land_it(&s), LandOutcome::AlreadyApplied);
        // A merge that gives what the head has is nothing to do either.
        let s = setup(
            &[("f", "1\n2\n3\n4\n5\n")],
            |r| vec![r.set("f", "ONE\n2\n3\n4\nFIVE\n")],
            |r| vec![r.set("f", "ONE\n2\n3\n4\n5\n")],
        );
        assert_eq!(land_it(&s), LandOutcome::AlreadyApplied);
    }

    #[test]
    fn content_conflict() {
        let s = setup(
            &[("f", "1\n2\n3\n")],
            |r| vec![r.set("f", "1\nours\n3\n")],
            |r| vec![r.set("f", "1\ntheirs\n3\n")],
        );
        let LandOutcome::Conflicts(found) = land_it(&s) else {
            panic!("expected conflicts");
        };
        assert_eq!(found.len(), 1);
        let conflict = &found[0];
        assert_eq!(conflict.path, b"f");
        assert_eq!(conflict.base, Some(s.repo.file_kind("1\n2\n3\n")));
        assert_eq!(conflict.ours, Some(s.repo.file_kind("1\nours\n3\n")));
        assert_eq!(conflict.theirs, Some(s.repo.file_kind("1\ntheirs\n3\n")));
        let ConflictKind::Content { hunks } = &conflict.kind else {
            panic!("expected a content conflict");
        };
        let side = |lines, text: &str| HunkSide {
            lines,
            text: text.as_bytes().to_vec(),
        };
        assert_eq!(
            hunks,
            &[ConflictHunk {
                base: side(1..2, "2\n"),
                ours: side(1..2, "ours\n"),
                theirs: side(1..2, "theirs\n"),
            }]
        );

        // Both sides add the same path with different content.
        let s = setup(
            &[],
            |r| vec![r.set("n", "a\n")],
            |r| vec![r.set("n", "b\n")],
        );
        assert_eq!(conflicts(&s), pairs(&[("n", "CONTENT")]));
        // Two symlink targets, and two gitlink commits.
        let s = setup(
            &[("x", "1")],
            |r| vec![r.symlink("l", "one"), gitlink("g", 1)],
            |r| vec![r.symlink("l", "two"), gitlink("g", 2)],
        );
        assert_eq!(conflicts(&s), pairs(&[("g", "CONTENT"), ("l", "CONTENT")]));
        // Binary content.
        let s = setup(
            &[("bin", "\0base")],
            |r| vec![r.set("bin", "\0ours")],
            |r| vec![r.set("bin", "\0theirs")],
        );
        let LandOutcome::Conflicts(found) = land_it(&s) else {
            panic!("expected conflicts");
        };
        assert_eq!(found[0].kind, ConflictKind::Content { hunks: Vec::new() });
    }

    #[test]
    fn modify_delete_conflict() {
        let s = setup(
            &[("f", "1"), ("g", "1")],
            |r| vec![r.set("f", "2"), delete("g")],
            |r| vec![delete("f"), r.set("g", "2")],
        );
        assert_eq!(
            conflicts(&s),
            pairs(&[("f", "MODIFY_DELETE"), ("g", "MODIFY_DELETE")])
        );
    }

    #[test]
    fn mode_conflict() {
        // Both changed the kind, differently.
        let s = setup(
            &[("f", "1")],
            |r| vec![r.symlink("f", "target")],
            |r| vec![r.exec("f", "1")],
        );
        assert_eq!(conflicts(&s), pairs(&[("f", "MODE")]));
        // Added on both sides, executable on one only: the base cannot settle
        // the bit.
        let s = setup(
            &[],
            |r| vec![r.exec("n", "same")],
            |r| vec![r.set("n", "same")],
        );
        assert_eq!(conflicts(&s), pairs(&[("n", "MODE")]));
    }

    #[test]
    fn directory_file_conflicts() {
        // The change sets a file where the head now has a directory.
        let s = setup(
            &[("a", "1")],
            |r| vec![r.set("x/y", "h")],
            |r| vec![r.set("x", "c")],
        );
        assert_eq!(conflicts(&s), pairs(&[("x", "DF")]));
        // The change sets something below a path the head made a file.
        let s = setup(
            &[("d/a", "1")],
            |r| vec![delete("d/a"), r.set("d", "file")],
            |r| vec![r.set("d/b", "c")],
        );
        assert_eq!(conflicts(&s), pairs(&[("d", "DF")]));
        // The change edits a file below a path the head made a file.
        let s = setup(
            &[("d/a", "1")],
            |r| vec![delete("d/a"), r.set("d", "file")],
            |r| vec![r.set("d/a", "2")],
        );
        assert_eq!(conflicts(&s), pairs(&[("d", "DF")]));
        // The change edits a file the head replaced with a directory.
        let s = setup(
            &[("f", "1")],
            |r| vec![delete("f"), r.set("f/inner", "h")],
            |r| vec![r.set("f", "2")],
        );
        assert_eq!(conflicts(&s), pairs(&[("f", "DF")]));
        // The change replaces a directory with a file, and the head added a
        // file to that directory.
        let s = setup(
            &[("p/x", "1")],
            |r| vec![r.set("p/new", "h")],
            |r| vec![delete("p/x"), r.set("p", "file")],
        );
        assert_eq!(conflicts(&s), pairs(&[("p", "DF")]));
    }

    #[test]
    fn replacing_a_directory_the_head_also_emptied() {
        // The head deleted one file of `p`; the change deleted both and put a
        // file in its place. Nothing is left in the directory: no conflict.
        let s = setup(
            &[("p/x", "1"), ("p/y", "2")],
            |_| vec![delete("p/y")],
            |r| vec![delete("p/x"), delete("p/y"), r.set("p", "file")],
        );
        let (id, _) = rebased(&s);
        assert_eq!(s.repo.files(s.repo.root(id)), text_map(&[("p", "file")]));
        // The head replaced a file with a directory, and the change deleted
        // the file: both removed it, the directory stays.
        let s = setup(
            &[("f", "1"), ("g", "1")],
            |r| vec![delete("f"), r.set("f/inner", "h")],
            |r| vec![delete("f"), r.set("g", "2")],
        );
        let (id, _) = rebased(&s);
        assert_eq!(
            s.repo.files(s.repo.root(id)),
            text_map(&[("f/inner", "h"), ("g", "2")])
        );
    }

    #[test]
    fn directory_removed_conflicts() {
        // The head removed `d`; the change adds a file in it.
        let s = setup(
            &[("d/a", "1"), ("k", "1")],
            |_| vec![delete("d/a")],
            |r| vec![r.set("d/b", "2")],
        );
        let LandOutcome::Conflicts(found) = land_it(&s) else {
            panic!("expected conflicts");
        };
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, b"d/b");
        assert_eq!(
            found[0].kind,
            ConflictKind::DirectoryRemoved {
                directory: b"d".to_vec()
            }
        );
        // Also when the new file is deeper, in a directory the change creates.
        let s = setup(
            &[("d/e/a", "1"), ("k", "1")],
            |_| vec![delete("d/e/a")],
            |r| vec![r.set("d/e/new/f", "2")],
        );
        assert_eq!(conflicts(&s), pairs(&[("d/e/new/f", "DIR_REMOVED")]));
        // The other way: the change removed `d`, the head added a file in it.
        let s = setup(
            &[("d/a", "1"), ("k", "1")],
            |r| vec![r.set("d/b", "2")],
            |_| vec![delete("d/a")],
        );
        let LandOutcome::Conflicts(found) = land_it(&s) else {
            panic!("expected conflicts");
        };
        assert_eq!(found[0].path, b"d/b");
        assert_eq!(found[0].ours, Some(s.repo.file_kind("2")));
        assert_eq!(found[0].theirs, None);
        // A directory that keeps other files is not removed.
        let s = setup(
            &[("d/a", "1"), ("d/z", "1")],
            |_| vec![delete("d/a")],
            |r| vec![r.set("d/b", "2")],
        );
        let (id, _) = rebased(&s);
        assert_eq!(
            s.repo.files(s.repo.root(id)),
            text_map(&[("d/b", "2"), ("d/z", "1")])
        );
    }

    #[test]
    fn case_conflicts() {
        let s = setup(
            &[("x", "1")],
            |r| vec![r.set("README", "h")],
            |r| vec![r.set("readme", "c")],
        );
        let LandOutcome::Conflicts(found) = land_it(&s) else {
            panic!("expected conflicts");
        };
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, b"readme");
        assert_eq!(
            found[0].kind,
            ConflictKind::Case {
                existing: b"README".to_vec()
            }
        );
        // Unicode normalisation: precomposed and decomposed forms.
        let s = setup(
            &[("x", "1")],
            |r| vec![r.set("docs/caf\u{e9}.md", "h")],
            |r| vec![r.set("docs/cafe\u{301}.md", "c")],
        );
        assert_eq!(conflicts(&s), pairs(&[("docs/cafe\u{301}.md", "CASE")]));
        // A directory name that collides, reported once.
        let s = setup(
            &[("x", "1")],
            |r| vec![r.set("Docs/a", "h")],
            |r| vec![r.set("docs/b", "c"), r.set("docs/c", "c")],
        );
        assert_eq!(conflicts(&s), pairs(&[("docs", "CASE")]));
        // A collision with a name that was already in the base is the
        // change's own business.
        let s = setup(
            &[("README", "1")],
            |r| vec![r.set("x", "h")],
            |r| vec![r.set("readme", "c")],
        );
        rebased(&s);
        // Renaming by case alone is fine too.
        let s = setup(
            &[("README", "1")],
            |r| vec![r.set("x", "h")],
            |r| vec![delete("README"), r.set("Readme", "1")],
        );
        rebased(&s);
        // Two names on the head fold to the name the change adds: each
        // collision is reported once, however many files go through it.
        let s = setup(
            &[("x", "1")],
            |r| vec![r.set("Docs/a", "h"), r.set("DOCS/b", "h")],
            |r| vec![r.set("docs/c", "c"), r.set("docs/d", "c")],
        );
        let LandOutcome::Conflicts(found) = land_it(&s) else {
            panic!("expected conflicts");
        };
        let existing: Vec<&ConflictKind> = found.iter().map(|c| &c.kind).collect();
        assert_eq!(
            existing,
            [
                &ConflictKind::Case {
                    existing: b"DOCS".to_vec()
                },
                &ConflictKind::Case {
                    existing: b"Docs".to_vec()
                },
            ]
        );
        // Both sides create the directory `cafe\u{301}`, and the change also
        // adds `caf\u{e9}` next to it. That collision is the change's own,
        // whichever side lands first.
        let dir = |r: &Repo| vec![r.set("cafe\u{301}/z", "h")];
        let own = |r: &Repo| vec![r.set("cafe\u{301}/b", "c"), r.set("caf\u{e9}", "c")];
        rebased(&setup(&[("x", "1")], dir, own));
        rebased(&setup(&[("x", "1")], own, dir));
    }

    #[test]
    fn several_conflicts_are_all_reported_in_order() {
        let s = setup(
            &[("a", "1\n"), ("b", "1"), ("c", "1")],
            |r| vec![r.set("a", "ours\n"), delete("b"), r.set("Z", "h")],
            |r| {
                vec![
                    r.set("a", "theirs\n"),
                    r.set("b", "2"),
                    r.set("z", "c"),
                    r.set("c", "2"),
                ]
            },
        );
        let stored = s.repo.objects.len();
        assert_eq!(
            conflicts(&s),
            pairs(&[("a", "CONTENT"), ("b", "MODIFY_DELETE"), ("z", "CASE")])
        );
        // A landing with conflicts stores nothing.
        assert_eq!(s.repo.objects.len(), stored);
    }
}
