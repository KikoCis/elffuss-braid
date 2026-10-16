// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Blame: the changeset that last changed each line of a file, along first
//! parents.
//!
//! The content of a file at a changeset `C` is the one the last changeset
//! on `C`'s first-parent chain whose change list sets the file gave it. Its
//! blame is derived data of that changeset and the path, computed from the
//! blame at the previous changeset on the chain that names the path: when
//! the path was a file there, a line diff between the two contents carries
//! the lines that were kept over with their changesets, and the lines added
//! or changed are the new changeset's. A file that a changeset creates,
//! where there was nothing, a directory or a gitlink, is all its own:
//! renames are not followed.
//!
//! The line diff, [`line_diff`], is imara-diff's Myers algorithm with its
//! indentation heuristic, which is what git's blame uses by default, so the
//! two agree line for line except in two ways. Git follows a file across
//! whole-file renames, and braid does not. And the two diffs can differ:
//! when a change has several shortest line diffs, they may keep different
//! copies of lines that repeat, such as closing braces and blank lines; and
//! neither always finds a shortest diff, so on some changes one inserts and
//! deletes a few more lines than the other. Blame then gives those lines to
//! different changesets.
//!
//! The derived function `blame`, version 1, of a changeset and a path (the
//! argument of the key) is:
//!
//! ```text
//! uv   k, k × [32] changesets, in the order their first lines come
//! uv   r, r × (uv changeset index, uv lines)    runs of lines, in order
//! ```
//!
//! Runs are not empty, two runs in a row never have the same changeset, and
//! every changeset listed has lines. A line ends after a `\n`, and a last
//! line without one is a line too, as in three-way merges.

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use braid_format::codec::{Reader, put_id, put_uv};
use braid_format::{ChangeOp, DecodeError, EntryKind, ObjectId, Objects};
use imara_diff::sources::byte_lines;
use imara_diff::{Algorithm, Diff, InternedInput};

use crate::access::{entry_at, read_blob, read_changeset};
use crate::bloom::PathHash;
use crate::derived::{DerivedIndex, DerivedKey};
use crate::error::Error;
use crate::history::History;

/// The derived function of blame records.
const BLAME: &str = "blame";

/// The version of its encoding.
const BLAME_VERSION: u64 = 1;

/// Content this long or longer is not diffed: a new version of it is all new
/// lines.
const MAX_DIFF_BYTES: usize = i32::MAX as usize;

/// Who last changed each line of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blame {
    /// The changeset the file's content comes from: the last one, on the
    /// first-parent chain of the changeset blamed, whose change list sets the
    /// file.
    pub changeset: ObjectId,
    /// The file's content.
    pub content: Vec<u8>,
    /// For each line of the content, in order, the changeset that last
    /// changed it.
    pub origins: Vec<ObjectId>,
}

impl Blame {
    /// Each line, with its line ending if it has one, and the changeset that
    /// last changed it.
    pub fn lines(&self) -> impl Iterator<Item = (&[u8], ObjectId)> + '_ {
        byte_lines(&self.content).zip(self.origins.iter().copied())
    }
}

impl<O: Objects, I: DerivedIndex> History<'_, O, I> {
    /// The blame of `path` at `changeset`: for each line of the file there,
    /// the changeset on the first-parent chain that last changed it. `None`
    /// when the path holds no file there: nothing, a directory or a gitlink.
    /// A symlink's target is blamed as a file.
    ///
    /// Blame records are computed for the changesets that set the file and
    /// lack one, back to the newest that has one or to where the file was
    /// created; finding those changesets costs a walk down the chain with
    /// changed-path filters.
    pub fn blame(&mut self, changeset: ObjectId, path: &[u8]) -> Result<Option<Blame>, Error> {
        let root = read_changeset(self.objects, changeset)?.root;
        let blob = match entry_at(self.objects, root, path)? {
            Some(
                EntryKind::File { blob, .. }
                | EntryKind::Executable { blob, .. }
                | EntryKind::Symlink { blob, .. },
            ) => blob,
            _ => return Ok(None),
        };
        let hash = PathHash::of(path);
        // The change list of some changeset on the chain must have set the
        // file, unless derived data says otherwise.
        let last = match self.last_change(Some(changeset), path, hash)? {
            Some((last, op)) if file_blob(&op) == Some(blob) => last,
            _ => return Err(Error::DerivedData { id: changeset }),
        };
        let (origins, content) = self.origins(last, blob, path, hash)?;
        Ok(Some(Blame {
            changeset: last,
            content,
            origins,
        }))
    }

    /// The origins of the lines of `path` at `last`, which set it to `blob`,
    /// and its content.
    fn origins(
        &mut self,
        last: ObjectId,
        blob: ObjectId,
        path: &[u8],
        hash: PathHash,
    ) -> Result<(Vec<ObjectId>, Vec<u8>), Error> {
        // The changesets that set the file without a blame record, newest
        // first, down to one with a record or to the one that created it.
        let mut chain: Vec<(ObjectId, ObjectId)> = Vec::new();
        let mut base: Option<(Vec<ObjectId>, Vec<u8>)> = None;
        let mut current = (last, blob);
        loop {
            if let Some(runs) = self.load_blame(current.0, path)? {
                let content = read_blob(self.objects, current.1)?;
                // A record that does not fit the content is ignored, and
                // made again.
                if runs.lines() == count_lines(&content) {
                    base = Some((runs.expand(), content));
                    break;
                }
            }
            chain.push(current);
            let parent = self.changed_paths(current.0)?.first_parent;
            match self.last_change(parent, path, hash)? {
                Some((id, op)) => match file_blob(&op) {
                    Some(blob) => current = (id, blob),
                    None => break,
                },
                None => break,
            }
        }

        let (mut origins, mut content) = match base {
            Some((origins, content)) => (origins, Some(content)),
            None => (Vec::new(), None),
        };
        for (id, blob) in chain.into_iter().rev() {
            let new = read_blob(self.objects, blob)?;
            origins = match &content {
                Some(old) => carry(old, &new, &origins, id),
                None => vec![id; count_lines(&new)],
            };
            self.save_blame(id, path, &origins)?;
            content = Some(new);
        }
        // The loop above ran at least once, or the base was found.
        Ok((origins, content.unwrap_or_default()))
    }

    /// The blame record of `path` at `id`, if there is a valid one.
    fn load_blame(&mut self, id: ObjectId, path: &[u8]) -> Result<Option<Rc<Runs>>, Error> {
        let memo = (id, path.to_vec());
        if let Some(runs) = self.blames.get(&memo) {
            return Ok(Some(Rc::clone(runs)));
        }
        let key = DerivedKey::new(BLAME, BLAME_VERSION, &[id], path);
        let Some(runs) = self.load(&key, Runs::decode)? else {
            return Ok(None);
        };
        let runs = Rc::new(runs);
        self.blames.insert(memo, Rc::clone(&runs));
        Ok(Some(runs))
    }

    /// Stores the blame record of `path` at `id`.
    fn save_blame(&mut self, id: ObjectId, path: &[u8], origins: &[ObjectId]) -> Result<(), Error> {
        let runs = Runs::of(origins);
        let key = DerivedKey::new(BLAME, BLAME_VERSION, &[id], path);
        self.save(key, &runs.encode())?;
        self.blames.insert((id, path.to_vec()), Rc::new(runs));
        Ok(())
    }
}

/// The blob a change sets, for the changes that set a file.
fn file_blob(op: &ChangeOp) -> Option<ObjectId> {
    match op {
        ChangeOp::File { blob, .. }
        | ChangeOp::Executable { blob, .. }
        | ChangeOp::Symlink { blob, .. } => Some(*blob),
        ChangeOp::Gitlink { .. } | ChangeOp::Delete => None,
    }
}

/// The number of lines of `content`.
fn count_lines(content: &[u8]) -> usize {
    byte_lines(content).count()
}

/// One change of a line diff: lines of the old version, replaced by lines of
/// the new one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineChange {
    /// The lines of the old version it replaces, counted from 0, end
    /// excluded; empty for an insertion, which goes before line `old.start`.
    pub old: Range<usize>,
    /// The lines of the new version it puts there; empty for a deletion.
    pub new: Range<usize>,
}

/// The line diff blame uses between two versions of a file: its changes, in
/// order. The lines between changes are kept: equal, in the same order.
///
/// It is imara-diff's Myers algorithm with its indentation heuristic, which
/// is what git's blame uses by default. Like git's, it does not always find
/// the shortest diff: it gives up on it where the search would cost too
/// much, and its preprocessing, which sets aside lines that cannot be kept,
/// can miss it too. So a diff may insert, and delete, a few more lines than
/// the fewest possible, and not on the same changes as git's. Content of
/// [`i32::MAX`] bytes or more is not diffed: all of it is replaced.
pub fn line_diff(old: &[u8], new: &[u8]) -> Vec<LineChange> {
    if old.len() >= MAX_DIFF_BYTES || new.len() >= MAX_DIFF_BYTES {
        let (old_lines, new_lines) = (count_lines(old), count_lines(new));
        if old_lines == 0 && new_lines == 0 {
            return Vec::new();
        }
        return vec![LineChange {
            old: 0..old_lines,
            new: 0..new_lines,
        }];
    }
    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Myers, &input);
    diff.postprocess_lines(&input);
    diff.hunks()
        .map(|hunk| LineChange {
            old: hunk.before.start as usize..hunk.before.end as usize,
            new: hunk.after.start as usize..hunk.after.end as usize,
        })
        .collect()
}

/// The origins of the lines of `new`, a later version of `old` whose lines
/// come from `old_origins`, when `id` made it: kept lines keep their origin,
/// the others are `id`'s.
fn carry(old: &[u8], new: &[u8], old_origins: &[ObjectId], id: ObjectId) -> Vec<ObjectId> {
    let mut origins = Vec::with_capacity(old_origins.len());
    // The next line of `old` that no change has reached yet.
    let mut kept = 0;
    for change in line_diff(old, new) {
        origins.extend_from_slice(&old_origins[kept..change.old.start]);
        origins.extend(std::iter::repeat_n(id, change.new.len()));
        kept = change.old.end;
    }
    origins.extend_from_slice(&old_origins[kept..]);
    debug_assert_eq!(origins.len(), count_lines(new));
    origins
}

/// A blame record: the origins of the lines of a file, as runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Runs {
    /// The changesets, in the order their first lines come.
    changesets: Vec<ObjectId>,
    /// Runs of lines: an index into `changesets`, and how many lines.
    runs: Vec<(usize, u64)>,
}

impl Runs {
    /// The runs of these origins, line by line.
    fn of(origins: &[ObjectId]) -> Runs {
        let mut changesets = Vec::new();
        let mut index: HashMap<ObjectId, usize> = HashMap::new();
        let mut runs: Vec<(usize, u64)> = Vec::new();
        for origin in origins {
            let at = *index.entry(*origin).or_insert_with(|| {
                changesets.push(*origin);
                changesets.len() - 1
            });
            match runs.last_mut() {
                Some((last, lines)) if *last == at => *lines += 1,
                _ => runs.push((at, 1)),
            }
        }
        Runs { changesets, runs }
    }

    /// The number of lines.
    fn lines(&self) -> usize {
        self.runs.iter().map(|(_, lines)| *lines as usize).sum()
    }

    /// The origin of each line.
    fn expand(&self) -> Vec<ObjectId> {
        let mut origins = Vec::with_capacity(self.lines());
        for (at, lines) in &self.runs {
            origins.extend(std::iter::repeat_n(self.changesets[*at], *lines as usize));
        }
        origins
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_uv(&mut out, self.changesets.len() as u64);
        for changeset in &self.changesets {
            put_id(&mut out, changeset);
        }
        put_uv(&mut out, self.runs.len() as u64);
        for (at, lines) in &self.runs {
            put_uv(&mut out, *at as u64);
            put_uv(&mut out, *lines);
        }
        out
    }

    /// Decodes a record, accepting only the encoding [`Runs::of`] makes.
    fn decode(value: &[u8]) -> Option<Runs> {
        let read = || -> Result<Runs, DecodeError> {
            let mut reader = Reader::new(value);
            let count = reader.count(ObjectId::LEN)?;
            let mut changesets = Vec::with_capacity(count);
            for _ in 0..count {
                changesets.push(reader.id()?);
            }
            let run_count = reader.count(2)?;
            let mut runs = Vec::with_capacity(run_count);
            for _ in 0..run_count {
                let at = reader.position();
                let index = usize::try_from(reader.uv()?).ok();
                let lines = reader.uv()?;
                let invalid = DecodeError::Invalid { at, what: "run" };
                let Some(index) = index.filter(|index| *index < count) else {
                    return Err(invalid);
                };
                // Changesets come in the order of their first runs, and two
                // runs in a row have different changesets.
                let seen = runs.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
                let repeats = runs.last().is_some_and(|(last, _)| *last == index);
                if lines == 0 || index > seen || repeats {
                    return Err(invalid);
                }
                runs.push((index, lines));
            }
            reader.finish()?;
            let used = runs.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
            if used != count {
                return Err(DecodeError::Invalid {
                    at: 0,
                    what: "unused changeset",
                });
            }
            Ok(Runs { changesets, runs })
        };
        read().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derived::MemoryIndex;
    use crate::testing::{Repo, delete, gitlink};
    use braid_format::Kind;

    fn id(n: u8) -> ObjectId {
        ObjectId::hash(Kind::Changeset, &[n])
    }

    #[test]
    fn runs_round_trip_and_are_canonical() {
        let origins = [id(1), id(1), id(2), id(1), id(3), id(3)];
        let runs = Runs::of(&origins);
        assert_eq!(runs.changesets, [id(1), id(2), id(3)]);
        assert_eq!(runs.runs, [(0, 2), (1, 1), (0, 1), (2, 2)]);
        assert_eq!(runs.lines(), 6);
        assert_eq!(runs.expand(), origins);
        assert_eq!(Runs::decode(&runs.encode()), Some(runs.clone()));
        assert_eq!(Runs::of(&[]).encode(), [0, 0]);
        assert_eq!(Runs::decode(&[0, 0]), Some(Runs::of(&[])));

        let encode = |changesets: &[ObjectId], runs: &[(u64, u64)]| {
            let mut out = Vec::new();
            put_uv(&mut out, changesets.len() as u64);
            for changeset in changesets {
                put_id(&mut out, changeset);
            }
            put_uv(&mut out, runs.len() as u64);
            for (at, lines) in runs {
                put_uv(&mut out, *at);
                put_uv(&mut out, *lines);
            }
            out
        };
        assert!(Runs::decode(&encode(&[id(1), id(2)], &[(0, 1), (1, 2)])).is_some());
        for (changesets, runs) in [
            // An empty run, two runs in a row from one changeset, an index
            // out of range, changesets out of order, one never used.
            (&[id(1)][..], &[(0, 0)][..]),
            (&[id(1)], &[(0, 1), (0, 1)]),
            (&[id(1)], &[(1, 1)]),
            (&[id(1), id(2)], &[(1, 1), (0, 1)]),
            (&[id(1), id(2)], &[(0, 1)]),
        ] {
            assert_eq!(Runs::decode(&encode(changesets, runs)), None, "{runs:?}");
        }
    }

    #[test]
    fn line_diffs_list_changes_between_kept_lines() {
        let change = |old: Range<usize>, new: Range<usize>| LineChange { old, new };
        assert_eq!(
            line_diff(b"a\nb\nc\n", b"a\nB\nc\nd\n"),
            [change(1..2, 1..2), change(3..3, 3..4)]
        );
        assert_eq!(line_diff(b"x\ny\n", b"x\ny\n"), []);
        assert_eq!(line_diff(b"", b""), []);
        assert_eq!(line_diff(b"", b"one\n"), [change(0..0, 0..1)]);
        assert_eq!(line_diff(b"a\nb\n", b"b\n"), [change(0..1, 0..0)]);
    }

    #[test]
    fn kept_lines_keep_their_origin() {
        let old = b"a\nb\nc\nd\n";
        let new = b"a\nB\nc\nd\ne\n";
        let origins = [id(1), id(2), id(3), id(4)];
        assert_eq!(
            carry(old, new, &origins, id(9)),
            [id(1), id(9), id(3), id(4), id(9)]
        );
        assert_eq!(carry(b"", b"x\ny", &[], id(9)), [id(9), id(9)]);
        assert_eq!(carry(b"x\n", b"", &[id(1)], id(9)), []);
        // A last line that gains its newline is a changed line.
        assert_eq!(
            carry(b"a\nb", b"a\nb\n", &[id(1), id(2)], id(9)),
            [id(1), id(9)]
        );
    }

    fn blame<O: Objects, I: DerivedIndex>(
        history: &mut History<'_, O, I>,
        at: ObjectId,
        path: &str,
    ) -> Vec<(String, ObjectId)> {
        let blame = history.blame(at, path.as_bytes()).unwrap().unwrap();
        blame
            .lines()
            .map(|(line, origin)| (String::from_utf8(line.to_vec()).unwrap(), origin))
            .collect()
    }

    fn origins<O: Objects, I: DerivedIndex>(
        history: &mut History<'_, O, I>,
        at: ObjectId,
    ) -> Vec<ObjectId> {
        history.blame(at, b"f").unwrap().unwrap().origins
    }

    #[test]
    fn lines_go_to_the_changeset_that_last_changed_them() {
        let repo = Repo::new();
        let one = repo.commit(&[], vec![repo.set("f", "a\nb\nc\n"), repo.set("g", "1\n")]);
        let two = repo.commit(&[one], vec![repo.set("f", "a\nB\nc\n")]);
        let other = repo.commit(&[two], vec![repo.set("g", "2\n")]);
        // A side branch adds a line; the merge brings it in, so the line is
        // the merge's along first parents.
        let side = repo.commit(&[two], vec![repo.set("f", "a\nB\nc\nside\n")]);
        let merge = repo.commit(&[other, side], vec![repo.set("f", "a\nB\nc\nside\n")]);
        let exec = repo.commit(&[merge], vec![repo.exec("f", "a\nB\nc\nside\n")]);

        let index = MemoryIndex::new();
        let mut history = History::new(&repo.objects, &index);
        let line = |text: &str, origin| (text.to_owned(), origin);
        assert_eq!(
            blame(&mut history, exec, "f"),
            [
                line("a\n", one),
                line("B\n", two),
                line("c\n", one),
                line("side\n", merge)
            ]
        );
        // Blamed at a changeset that did not touch the file.
        assert_eq!(
            blame(&mut history, other, "f"),
            [line("a\n", one), line("B\n", two), line("c\n", one)]
        );
        assert_eq!(blame(&mut history, other, "g"), [line("2\n", other)]);
        let whole = history.blame(exec, b"f").unwrap().unwrap();
        assert_eq!(whole.changeset, exec);
        assert_eq!(whole.content, b"a\nB\nc\nside\n");
        // Records for the changesets that set f, once each.
        let computed = history.computed();
        history.blame(exec, b"f").unwrap();
        assert_eq!(history.computed(), computed);

        // Nothing to blame: a missing path, a directory.
        assert_eq!(history.blame(exec, b"nothing").unwrap(), None);
        let dir = repo.commit(&[exec], vec![repo.set("d/x", "1\n")]);
        assert_eq!(history.blame(dir, b"d").unwrap(), None);
    }

    #[test]
    fn a_file_created_again_starts_over() {
        let repo = Repo::new();
        let one = repo.commit(&[], vec![repo.set("f", "a\nb\n")]);
        let gone = repo.commit(&[one], vec![delete("f")]);
        let back = repo.commit(&[gone], vec![repo.set("f", "a\nb\nc\n")]);
        let link = repo.commit(&[back], vec![gitlink("f", 7)]);
        let file = repo.commit(&[link], vec![repo.set("f", "a\nb\nc\n")]);
        let index = MemoryIndex::new();
        let mut history = History::new(&repo.objects, &index);
        assert_eq!(origins(&mut history, back), [back, back, back]);
        assert_eq!(origins(&mut history, file), [file, file, file]);
        assert_eq!(history.blame(link, b"f").unwrap(), None);
        assert_eq!(history.blame(gone, b"f").unwrap(), None);
    }

    #[test]
    fn records_made_in_any_order_are_the_same() {
        let repo = Repo::new();
        let mut at = repo.commit(&[], vec![repo.set("f", "1\n2\n3\n")]);
        let mut chain = vec![at];
        for (i, content) in ["1\n2\n3\n4\n", "0\n1\n3\n4\n", "0\n1\n3\n4\n5\n", "x\n"]
            .iter()
            .enumerate()
        {
            let other = repo.set(&format!("other{i}"), "x");
            at = repo.commit(&[at], vec![repo.set("f", content), other]);
            chain.push(at);
        }
        let forward = MemoryIndex::new();
        let mut history = History::new(&repo.objects, &forward);
        for at in &chain {
            history.blame(*at, b"f").unwrap();
        }
        history.flush().unwrap();
        let backward = MemoryIndex::new();
        let mut history = History::new(&repo.objects, &backward);
        for at in chain.iter().rev() {
            history.blame(*at, b"f").unwrap();
        }
        history.flush().unwrap();
        assert_eq!(forward.entries(), backward.entries());
    }
}
