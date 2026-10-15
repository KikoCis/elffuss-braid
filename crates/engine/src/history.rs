// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! History queries answered with derived data: the history of paths, here,
//! ancestry in [`ancestry`](crate::ancestry), and blame in
//! [`blame`](crate::blame).
//!
//! A [`History`] reads objects, finds derived records through a
//! [`DerivedIndex`], computes the values that are missing, stores them as
//! records and remembers every value it used. It records the new records in
//! the index by batches, and at the end when [`History::flush`] is called; a
//! value computed and never recorded is only computed again later.
//!
//! # Changed paths
//!
//! The derived function `changed-paths`, version 1, of a changeset is its
//! first parent and a Bloom filter (see `bloom.rs`) of the paths its change
//! list names and of their parent directories:
//!
//! ```text
//! u8   1 if there is a first parent, else 0
//! [32] the first parent, if there is one
//! uv   the number of entries, then the filter's bytes
//! ```
//!
//! It depends on the changeset alone, so the walk down a first-parent chain
//! reads one small record per changeset, and reads a change list only when
//! the filter says that the changeset may touch the path.

use core::fmt;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use braid_format::codec::{Reader, put_id};
use braid_format::{Change, ChangeOp, DecodeError, Kind, ObjectId, Objects};

use crate::access::{read_changes, read_changeset};
use crate::ancestry::GraphNode;
use crate::blame::Runs;
use crate::bloom::{Filter, PathHash};
use crate::derived::{DerivedIndex, DerivedKey, encode_record, record_value};
use crate::error::{Error, store_error};

/// The derived function of changed-path records.
const CHANGED_PATHS: &str = "changed-paths";

/// The version of its encoding.
const CHANGED_PATHS_VERSION: u64 = 1;

/// New records are recorded in the index once this many wait.
const RECORD_BATCH: usize = 4096;

/// History queries over the objects of `O`, with the derived data found
/// through `I`, computed when missing.
///
/// A `History` keeps every derived value it reads or computes in memory, so
/// a query never reads the same value twice; drop it to free them. Call
/// [`flush`](Self::flush) before dropping it to record the new values in the
/// index.
pub struct History<'o, O, I> {
    pub(crate) objects: &'o O,
    index: I,
    /// Commit graph data, by changeset.
    pub(crate) nodes: HashMap<ObjectId, Rc<GraphNode>>,
    /// Changed-path records, by changeset.
    paths: HashMap<ObjectId, Rc<ChangedPaths>>,
    /// Blame records, by changeset and path.
    pub(crate) blames: HashMap<(ObjectId, Vec<u8>), Rc<Runs>>,
    /// Records stored and not recorded in the index yet.
    pending: Vec<(DerivedKey, ObjectId)>,
    /// Records this history computed and stored.
    computed: u64,
}

impl<O, I> fmt::Debug for History<'_, O, I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("History")
            .field("nodes", &self.nodes.len())
            .field("paths", &self.paths.len())
            .field("blames", &self.blames.len())
            .field("pending", &self.pending.len())
            .field("computed", &self.computed)
            .finish_non_exhaustive()
    }
}

/// What [`History::derive`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeriveReport {
    /// Changesets reachable from the heads.
    pub changesets: u64,
    /// Records computed and stored; the others were there already.
    pub computed: u64,
}

impl<'o, O: Objects, I: DerivedIndex> History<'o, O, I> {
    /// A history over `objects`, finding and recording derived records
    /// through `index`.
    pub fn new(objects: &'o O, index: I) -> Self {
        History {
            objects,
            index,
            nodes: HashMap::new(),
            paths: HashMap::new(),
            blames: HashMap::new(),
            pending: Vec::new(),
            computed: 0,
        }
    }

    /// The objects this history reads.
    pub fn objects(&self) -> &'o O {
        self.objects
    }

    /// How many derived records this history computed and stored.
    pub fn computed(&self) -> u64 {
        self.computed
    }

    /// Records in the index every record stored since the last flush.
    pub fn flush(&mut self) -> Result<(), Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.index.record(&self.pending).map_err(store_error)?;
        self.pending.clear();
        Ok(())
    }

    /// The value of `key`, from its record, if the index knows the record
    /// and `decode` accepts its value. Anything else is a miss.
    pub(crate) fn load<T>(
        &mut self,
        key: &DerivedKey,
        decode: impl FnOnce(&[u8]) -> Option<T>,
    ) -> Result<Option<T>, Error> {
        let Some(record) = self.index.find(key).map_err(store_error)? else {
            return Ok(None);
        };
        let payload = match self.objects.get(&record).map_err(store_error)? {
            Some((Kind::Derived, payload)) if ObjectId::hash(Kind::Derived, &payload) == record => {
                payload
            }
            _ => return Ok(None),
        };
        Ok(record_value(key, &payload).and_then(decode))
    }

    /// Stores `value` as the record of `key`.
    pub(crate) fn save(&mut self, key: DerivedKey, value: &[u8]) -> Result<(), Error> {
        let record = self
            .objects
            .put(Kind::Derived, &encode_record(&key, value))
            .map_err(store_error)?;
        self.pending.push((key, record));
        self.computed += 1;
        if self.pending.len() >= RECORD_BATCH {
            self.flush()?;
        }
        Ok(())
    }

    /// The changed-path record of a changeset.
    pub(crate) fn changed_paths(&mut self, id: ObjectId) -> Result<Rc<ChangedPaths>, Error> {
        if let Some(record) = self.paths.get(&id) {
            return Ok(Rc::clone(record));
        }
        let key = DerivedKey::new(CHANGED_PATHS, CHANGED_PATHS_VERSION, &[id], b"");
        let record = match self.load(&key, ChangedPaths::decode)? {
            Some(record) => record,
            None => {
                let changeset = read_changeset(self.objects, id)?;
                let changes = read_changes(self.objects, id, &changeset)?;
                let record = ChangedPaths {
                    first_parent: changeset.parents.first().copied(),
                    filter: Filter::build(changes.iter().map(|change| change.path.as_slice())),
                };
                self.save(key, &record.encode())?;
                record
            }
        };
        let record = Rc::new(record);
        self.paths.insert(id, Rc::clone(&record));
        Ok(record)
    }

    /// The changesets on the first-parent chain of `head`, newest first,
    /// whose change list touches one of `paths`: changes the path itself or
    /// anything below it. At most `limit` of them.
    ///
    /// A path is given from the repository's root, without a leading or
    /// trailing `/`; the empty path matches every changeset that changes
    /// something. Like `git log --first-parent -- <path>`, a changeset counts
    /// when it differs from its first parent there, and renames are not
    /// followed.
    ///
    /// The walk reads one changed-path record per changeset, and the change
    /// list of a changeset only when its filter may hold one of the paths: the
    /// cost follows the length of the chain walked, in filter checks, plus the
    /// changesets that touch the paths.
    pub fn path_log(
        &mut self,
        head: ObjectId,
        paths: &[&[u8]],
        limit: usize,
    ) -> Result<Vec<ObjectId>, Error> {
        let wanted: Vec<(&[u8], PathHash)> = paths
            .iter()
            .map(|path| (*path, PathHash::of(path)))
            .collect();
        let mut found = Vec::new();
        let mut next = Some(head);
        while let Some(id) = next {
            if found.len() >= limit {
                break;
            }
            let record = self.changed_paths(id)?;
            let maybe = wanted.iter().any(|(path, hash)| {
                if path.is_empty() {
                    !record.filter.is_empty()
                } else {
                    record.filter.may_contain(*hash)
                }
            });
            if maybe {
                let changeset = read_changeset(self.objects, id)?;
                let changes = read_changes(self.objects, id, &changeset)?;
                if wanted.iter().any(|(path, _)| touches(&changes, path)) {
                    found.push(id);
                }
            }
            next = record.first_parent;
        }
        Ok(found)
    }

    /// The first changeset, from `from` down its first-parent chain, whose
    /// change list names exactly `path`, with what it does there.
    pub(crate) fn last_change(
        &mut self,
        from: Option<ObjectId>,
        path: &[u8],
        hash: PathHash,
    ) -> Result<Option<(ObjectId, ChangeOp)>, Error> {
        let mut next = from;
        while let Some(id) = next {
            let record = self.changed_paths(id)?;
            if record.filter.may_contain(hash) {
                let changeset = read_changeset(self.objects, id)?;
                let changes = read_changes(self.objects, id, &changeset)?;
                if let Ok(at) = changes.binary_search_by(|change| change.path.as_slice().cmp(path))
                {
                    return Ok(Some((id, changes[at].op.clone())));
                }
            }
            next = record.first_parent;
        }
        Ok(None)
    }

    /// Computes the derived data of every changeset reachable from `heads`
    /// that is missing: commit graph data and changed-path records. Blame
    /// records, per file, are computed when asked for. Records everything
    /// in the index before returning.
    pub fn derive(&mut self, heads: &[ObjectId]) -> Result<DeriveReport, Error> {
        let before = self.computed;
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut stack = heads.to_vec();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let node = self.node(id)?;
            self.changed_paths(id)?;
            stack.extend(node.parents.iter().filter(|parent| !seen.contains(*parent)));
        }
        self.flush()?;
        Ok(DeriveReport {
            changesets: seen.len() as u64,
            computed: self.computed - before,
        })
    }
}

/// Whether a sorted change list touches `path`: names it, or a path below
/// it. The empty path is touched by any change.
pub(crate) fn touches(changes: &[Change], path: &[u8]) -> bool {
    if path.is_empty() {
        return !changes.is_empty();
    }
    if changes
        .binary_search_by(|change| change.path.as_slice().cmp(path))
        .is_ok()
    {
        return true;
    }
    // Paths below `path` sort together, from `path/` on.
    let mut below = path.to_vec();
    below.push(b'/');
    let at = changes.partition_point(|change| change.path.as_slice() < below.as_slice());
    changes
        .get(at)
        .is_some_and(|change| change.path.starts_with(&below))
}

/// The changed-path record of a changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangedPaths {
    /// The first parent.
    pub(crate) first_parent: Option<ObjectId>,
    /// The filter of the paths the change list names, and their directories.
    pub(crate) filter: Filter,
}

impl ChangedPaths {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match &self.first_parent {
            Some(parent) => {
                out.push(1);
                put_id(&mut out, parent);
            }
            None => out.push(0),
        }
        self.filter.encode(&mut out);
        out
    }

    fn decode(value: &[u8]) -> Option<ChangedPaths> {
        let read = || -> Result<ChangedPaths, DecodeError> {
            let mut reader = Reader::new(value);
            let at = reader.position();
            let first_parent = match reader.u8()? {
                0 => None,
                1 => Some(reader.id()?),
                _ => {
                    return Err(DecodeError::Invalid {
                        at,
                        what: "first parent flag",
                    });
                }
            };
            let filter = Filter::decode(&mut reader)?;
            reader.finish()?;
            Ok(ChangedPaths {
                first_parent,
                filter,
            })
        };
        read().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derived::MemoryIndex;
    use crate::testing::{Repo, delete};

    fn log(repo: &Repo, head: ObjectId, path: &str) -> Vec<ObjectId> {
        let index = MemoryIndex::new();
        let mut history = History::new(&repo.objects, &index);
        history
            .path_log(head, &[path.as_bytes()], usize::MAX)
            .unwrap()
    }

    #[test]
    fn touching_is_by_path_or_below_it() {
        let repo = Repo::new();
        let changes = vec![
            repo.set("a", "1"),
            repo.set("a-b/c", "2"),
            repo.set("a.txt", "3"),
            repo.set("a/x/y", "4"),
            repo.set("b", "5"),
        ];
        for (path, expected) in [
            ("a", true),
            ("a/x", true),
            ("a/x/y", true),
            ("a/y", false),
            ("a-b", true),
            ("a-", false),
            ("b", true),
            ("c", false),
            ("", true),
        ] {
            assert_eq!(touches(&changes, path.as_bytes()), expected, "{path}");
        }
        assert!(!touches(&[], b""));
        assert!(!touches(&[repo.set("ab", "1")], b"a"));
    }

    #[test]
    fn records_round_trip() {
        let parent = ObjectId::hash(Kind::Changeset, b"p");
        for record in [
            ChangedPaths {
                first_parent: Some(parent),
                filter: Filter::build([&b"a/b"[..]]),
            },
            ChangedPaths {
                first_parent: None,
                filter: Filter::Empty,
            },
        ] {
            assert_eq!(ChangedPaths::decode(&record.encode()), Some(record));
        }
        assert_eq!(ChangedPaths::decode(&[2, 0]), None);
        assert_eq!(ChangedPaths::decode(&[0, 0, 0]), None);
    }

    #[test]
    fn the_log_of_a_path_follows_first_parents() {
        let repo = Repo::new();
        let root = repo.commit(&[], vec![repo.set("a/f", "1"), repo.set("b", "1")]);
        let edit = repo.commit(&[root], vec![repo.set("a/f", "2")]);
        let other = repo.commit(&[edit], vec![repo.set("b", "2")]);
        // A side branch changes a/g; the merge brings it to the first
        // parent's history, so the merge touches a.
        let side = repo.commit(&[root], vec![repo.set("a/g", "1")]);
        let merge = repo.commit(&[other, side], vec![repo.set("a/g", "1")]);
        let gone = repo.commit(&[merge], vec![delete("a/f")]);
        let empty = repo.commit(&[gone], Vec::new());
        let back = repo.commit(&[empty], vec![repo.set("a/f", "3")]);

        assert_eq!(log(&repo, back, "a/f"), [back, gone, edit, root]);
        assert_eq!(log(&repo, back, "a/g"), [merge]);
        assert_eq!(log(&repo, back, "a"), [back, gone, merge, edit, root]);
        assert_eq!(log(&repo, back, "b"), [other, root]);
        assert_eq!(log(&repo, back, "a/f/x"), []);
        assert_eq!(log(&repo, back, "c"), []);
        assert_eq!(log(&repo, back, ""), [back, gone, merge, other, edit, root]);
        // The side branch is not on the first-parent chain.
        assert_eq!(log(&repo, side, "a/g"), [side]);

        let index = MemoryIndex::new();
        let mut history = History::new(&repo.objects, &index);
        assert_eq!(
            history.path_log(back, &[b"b", b"a/g"], usize::MAX).unwrap(),
            [merge, other, root]
        );
        assert_eq!(history.path_log(back, &[b"a"], 2).unwrap(), [back, gone]);
        assert_eq!(history.path_log(back, &[b"a"], 0).unwrap(), []);
        assert_eq!(
            history
                .last_change(Some(back), b"a/f", PathHash::of(b"a/f"))
                .unwrap()
                .map(|(id, _)| id),
            Some(back)
        );
        assert_eq!(
            history
                .last_change(Some(empty), b"a/f", PathHash::of(b"a/f"))
                .unwrap(),
            Some((gone, ChangeOp::Delete))
        );
    }

    #[test]
    fn records_are_stored_once_and_reused() {
        let repo = Repo::new();
        let root = repo.commit(&[], vec![repo.set("f", "1")]);
        let head = repo.commit(&[root], vec![repo.set("f", "2")]);
        let index = MemoryIndex::new();
        let mut first = History::new(&repo.objects, &index);
        assert_eq!(first.path_log(head, &[b"f"], 10).unwrap(), [head, root]);
        assert_eq!(first.computed(), 2);
        // Not in the index until flushed.
        assert!(index.is_empty());
        first.flush().unwrap();
        assert_eq!(index.len(), 2);

        let mut second = History::new(&repo.objects, &index);
        assert_eq!(second.path_log(head, &[b"f"], 10).unwrap(), [head, root]);
        assert_eq!(second.computed(), 0);

        // A record that is gone, or another value under the key, is computed
        // again.
        let key = DerivedKey::new(CHANGED_PATHS, CHANGED_PATHS_VERSION, &[head], b"");
        let elsewhere = repo.objects.put(Kind::Derived, b"not a record").unwrap();
        index
            .record(&[(key, ObjectId::hash(Kind::Derived, b"gone"))])
            .unwrap();
        let mut third = History::new(&repo.objects, &index);
        assert_eq!(third.path_log(head, &[b"f"], 10).unwrap(), [head, root]);
        assert_eq!(third.computed(), 1);
        index.record(&[(key, elsewhere)]).unwrap();
        let mut fourth = History::new(&repo.objects, &index);
        assert_eq!(fourth.path_log(head, &[b"f"], 10).unwrap(), [head, root]);
        assert_eq!(fourth.computed(), 1);
    }
}
