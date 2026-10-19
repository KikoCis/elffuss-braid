// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Every object a set of changesets reaches: what a client needs to hold
//! them, or what a server lacks of them.
//!
//! From each changeset the walk takes the changeset itself, its external
//! change list, the blob holding its original git commit, and every node
//! and blob of its root tree; then, as far as `history` says, its parents.
//! Changesets in `have` are not walked, and the nodes and blobs of their
//! root trees are left out of the result: whoever asks holds them. A walk
//! of the whole history also skips every ancestor of a `have` changeset,
//! so that the second parent of a merge does not lead it back down a
//! history the asker holds; finding them reads every changeset `have`
//! reaches, and nothing else.

use std::collections::{HashSet, VecDeque};

use braid_format::changeset::Changes;
use braid_format::tree::{EntryKind, Node};
use braid_format::{Changeset, Kind, ObjectId, Objects};

use crate::access::{EMPTY_TREE, read_object};
use crate::error::{Error, store_error};

/// What to walk.
#[derive(Clone, Debug, Default)]
pub struct Reach<'a> {
    /// The changesets wanted.
    pub want: &'a [ObjectId],
    /// Changesets already held with everything they reach.
    pub have: &'a [ObjectId],
    /// `None` for every ancestor, `Some(0)` for the wanted changesets
    /// alone, `Some(n)` for n generations along all parents.
    pub history: Option<u32>,
    /// When given, only the parts of trees on the way to or under one of
    /// these path prefixes (`/`-separated, no leading or trailing `/`).
    pub paths: Option<&'a [Vec<u8>]>,
    /// When given, the walk stops with [`Error::TooManyObjects`] as soon as
    /// it would list more objects than this, so that a server answering
    /// for others bounds the work and the memory of one request.
    pub max_objects: Option<usize>,
}

/// One object reached: its id, kind and size in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reached {
    /// The object's id.
    pub id: ObjectId,
    /// Its kind.
    pub kind: Kind,
    /// Its size in bytes, decoded.
    pub size: u64,
}

/// Lists every object `reach` reaches, each once: a changeset before the
/// objects it names, a tree node before what it holds. The empty tree is
/// never listed: everyone has it.
///
/// With [`Reach::max_objects`], neither the list nor the objects of the
/// `have` root trees may grow past that many, or the walk stops with
/// [`Error::TooManyObjects`].
pub fn reachable<O: Objects>(objects: &O, reach: &Reach<'_>) -> Result<Vec<Reached>, Error> {
    let mut walk = Walk {
        objects,
        paths: reach.paths,
        max: reach.max_objects,
        seen: HashSet::new(),
        out: Vec::new(),
    };
    // What the asker holds: the `have` changesets, with all their ancestors
    // when the walk could reach them, and the nodes and blobs of every
    // `have` root tree.
    let stop = match reach.history {
        None => ancestors(objects, reach.have)?,
        Some(_) => reach.have.iter().copied().collect(),
    };
    let mut known = Walk {
        objects,
        paths: None,
        max: reach.max_objects,
        seen: HashSet::new(),
        out: Vec::new(),
    };
    for have in reach.have {
        if let Some((Kind::Changeset, bytes)) = objects.get(have).map_err(store_error)? {
            let changeset =
                Changeset::decode(&bytes).map_err(|error| Error::Changeset { id: *have, error })?;
            known.tree(changeset.root, &[])?;
        }
    }
    walk.seen = known.seen;

    let mut queue: VecDeque<(ObjectId, u32)> = VecDeque::new();
    let mut queued: HashSet<ObjectId> = HashSet::new();
    for id in reach.want {
        if !stop.contains(id) && queued.insert(*id) {
            queue.push_back((*id, 0));
        }
    }
    while let Some((id, depth)) = queue.pop_front() {
        let bytes = read_object(objects, id, Kind::Changeset)?;
        let changeset =
            Changeset::decode(&bytes).map_err(|error| Error::Changeset { id, error })?;
        walk.emit(id, Kind::Changeset, bytes.len() as u64)?;
        if let Changes::External { list, .. } = &changeset.changes {
            walk.object(*list, Kind::Blob)?;
        }
        if let Some(origin) = changeset.git_origin {
            walk.object(origin, Kind::Blob)?;
        }
        walk.tree(changeset.root, &[])?;
        let deeper = reach.history.is_none_or(|limit| depth < limit);
        if deeper {
            for parent in &changeset.parents {
                if !stop.contains(parent) && queued.insert(*parent) {
                    queue.push_back((*parent, depth + 1));
                }
            }
        }
    }
    Ok(walk.out)
}

/// `have` and every ancestor of it, along all parents, among the
/// changesets stored: an id that is not a stored changeset is kept, and not
/// walked.
fn ancestors<O: Objects>(objects: &O, have: &[ObjectId]) -> Result<HashSet<ObjectId>, Error> {
    let mut found: HashSet<ObjectId> = HashSet::new();
    let mut pending: Vec<ObjectId> = have.to_vec();
    while let Some(id) = pending.pop() {
        if !found.insert(id) {
            continue;
        }
        if let Some((Kind::Changeset, bytes)) = objects.get(&id).map_err(store_error)? {
            let changeset =
                Changeset::decode(&bytes).map_err(|error| Error::Changeset { id, error })?;
            pending.extend(
                changeset
                    .parents
                    .iter()
                    .filter(|parent| !found.contains(parent)),
            );
        }
    }
    Ok(found)
}

struct Walk<'a, O> {
    objects: &'a O,
    paths: Option<&'a [Vec<u8>]>,
    /// The most objects `out` may hold.
    max: Option<usize>,
    seen: HashSet<ObjectId>,
    out: Vec<Reached>,
}

impl<O: Objects> Walk<'_, O> {
    fn emit(&mut self, id: ObjectId, kind: Kind, size: u64) -> Result<(), Error> {
        if self.seen.insert(id) {
            if let Some(limit) = self.max
                && self.out.len() >= limit
            {
                return Err(Error::TooManyObjects { limit });
            }
            self.out.push(Reached { id, kind, size });
        }
        Ok(())
    }

    /// A whole object whose size is not known yet.
    fn object(&mut self, id: ObjectId, kind: Kind) -> Result<(), Error> {
        if self.seen.contains(&id) {
            return Ok(());
        }
        match self.objects.stat(&id).map_err(store_error)? {
            Some((found, size)) if found == kind => self.emit(id, kind, size),
            Some((found, _)) => Err(Error::WrongKind {
                id,
                expected: kind,
                found,
            }),
            None => Err(Error::Missing { id, kind }),
        }
    }

    /// Whether anything at `path` (a directory or a file) can be wanted.
    fn wanted(&self, path: &[u8]) -> bool {
        let Some(prefixes) = self.paths else {
            return true;
        };
        prefixes.iter().any(|prefix| {
            // On the way to the prefix, or under it.
            let on_the_way = prefix.starts_with(path)
                && (prefix.len() == path.len() || path.is_empty() || prefix[path.len()] == b'/');
            let under = path.starts_with(prefix)
                && (path.len() == prefix.len() || prefix.is_empty() || path[prefix.len()] == b'/');
            on_the_way || under
        })
    }

    /// A tree node and what it holds; `dir` is the directory's path.
    fn tree(&mut self, node: ObjectId, dir: &[u8]) -> Result<(), Error> {
        if node == EMPTY_TREE || self.seen.contains(&node) {
            return Ok(());
        }
        let bytes = read_object(self.objects, node, Kind::Tree)?;
        let decoded = Node::decode(&bytes).map_err(|error| Error::Tree { id: node, error })?;
        self.emit(node, Kind::Tree, bytes.len() as u64)?;
        match decoded {
            Node::Flat(entries) | Node::Leaf(entries) => {
                for entry in entries {
                    let path = if dir.is_empty() {
                        entry.name.clone()
                    } else {
                        [dir, b"/", &entry.name].concat()
                    };
                    if !self.wanted(&path) {
                        continue;
                    }
                    match entry.kind {
                        EntryKind::File { blob, size }
                        | EntryKind::Executable { blob, size }
                        | EntryKind::Symlink { blob, size } => self.emit(blob, Kind::Blob, size)?,
                        EntryKind::Tree { tree } => self.tree(tree, &path)?,
                        EntryKind::Gitlink { .. } => {}
                    }
                }
            }
            Node::Internal { children, .. } => {
                for child in children {
                    self.tree(child.node, dir)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Repo;
    use braid_format::MemoryObjects;

    fn ids(reached: &[Reached]) -> HashSet<ObjectId> {
        reached.iter().map(|r| r.id).collect()
    }

    /// Copies what `reached` lists into a fresh store.
    fn copy(from: &MemoryObjects, reached: &[Reached]) -> MemoryObjects {
        let to = MemoryObjects::new();
        for r in reached {
            let (kind, bytes) = from.get(&r.id).unwrap().unwrap();
            assert_eq!(kind, r.kind);
            assert_eq!(bytes.len() as u64, r.size, "{:?}", r.kind);
            to.put(kind, &bytes).unwrap();
        }
        to
    }

    #[test]
    fn everything_a_history_needs_and_nothing_twice() {
        let repo = Repo::new();
        let one = repo.commit(
            &[],
            vec![repo.set("a.txt", "1\n"), repo.set("src/lib.rs", "x\n")],
        );
        let two = repo.commit(&[one], vec![repo.set("a.txt", "2\n")]);
        let three = repo.commit(&[two], vec![repo.set("src/main.rs", "y\n")]);
        let all = reachable(
            &repo.objects,
            &Reach {
                want: &[three],
                ..Reach::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), ids(&all).len(), "listed once");
        assert_eq!(all[0].id, three, "a changeset first");
        // A store holding only what was listed reads every changeset and tree.
        let copied = copy(&repo.objects, &all);
        for changeset in [one, two, three] {
            let read = crate::read_changeset(&copied, changeset).unwrap();
            crate::read_tree(&copied, read.root).unwrap();
        }
        // The blob "x" is shared by all three trees and listed once.
        let x = repo.blob("x\n");
        assert_eq!(all.iter().filter(|r| r.id == x).count(), 1);
    }

    #[test]
    fn have_and_history_cut_the_walk() {
        let repo = Repo::new();
        let one = repo.commit(
            &[],
            vec![repo.set("a.txt", "1\n"), repo.set("b.txt", "b\n")],
        );
        let two = repo.commit(&[one], vec![repo.set("a.txt", "2\n")]);
        let three = repo.commit(&[two], vec![repo.set("a.txt", "3\n")]);
        // Holding two: only three and what it adds.
        let new = reachable(
            &repo.objects,
            &Reach {
                want: &[three],
                have: &[two],
                ..Reach::default()
            },
        )
        .unwrap();
        let new = ids(&new);
        assert!(new.contains(&three));
        assert!(!new.contains(&two) && !new.contains(&one));
        assert!(new.contains(&repo.blob("3\n")));
        assert!(!new.contains(&repo.blob("b\n")), "b.txt is in two's tree");
        // history 0: the wanted changeset alone.
        let only = reachable(
            &repo.objects,
            &Reach {
                want: &[three],
                history: Some(0),
                ..Reach::default()
            },
        )
        .unwrap();
        let only = ids(&only);
        assert!(only.contains(&three) && !only.contains(&two));
        // history 1: one generation more.
        let one_more = reachable(
            &repo.objects,
            &Reach {
                want: &[three],
                history: Some(1),
                ..Reach::default()
            },
        )
        .unwrap();
        let one_more = ids(&one_more);
        assert!(one_more.contains(&two) && !one_more.contains(&one));
    }

    #[test]
    fn a_merge_does_not_lead_back_into_what_is_held() {
        let repo = Repo::new();
        let root = repo.commit(&[], vec![repo.set("a.txt", "0\n")]);
        let side = repo.commit(&[root], vec![repo.set("side.txt", "s\n")]);
        let held = repo.commit(&[root], vec![repo.set("a.txt", "1\n")]);
        // A merge of the side branch on top of what the asker holds.
        let merge = repo.commit(&[held, side], vec![repo.set("side.txt", "s\n")]);
        let new = reachable(
            &repo.objects,
            &Reach {
                want: &[merge],
                have: &[held],
                ..Reach::default()
            },
        )
        .unwrap();
        let new = ids(&new);
        assert!(new.contains(&merge) && new.contains(&side));
        assert!(!new.contains(&root), "an ancestor of what is held");
        assert!(!new.contains(&held));
        // Unknown ids in `have` are ignored.
        let unknown = ObjectId::hash(Kind::Changeset, b"unknown");
        let all = reachable(
            &repo.objects,
            &Reach {
                want: &[merge],
                have: &[unknown],
                ..Reach::default()
            },
        )
        .unwrap();
        assert!(ids(&all).contains(&root));
    }

    #[test]
    fn paths_keep_the_way_and_what_is_under() {
        let repo = Repo::new();
        let head = repo.commit(
            &[],
            vec![
                repo.set("README", "r\n"),
                repo.set("src/net/tcp.rs", "tcp\n"),
                repo.set("src/net/udp.rs", "udp\n"),
                repo.set("src/fs/io.rs", "io\n"),
                repo.set("srcx/other.rs", "other\n"),
            ],
        );
        let paths = vec![b"src/net".to_vec()];
        let some = reachable(
            &repo.objects,
            &Reach {
                want: &[head],
                history: Some(0),
                paths: Some(&paths),
                ..Reach::default()
            },
        )
        .unwrap();
        let some = ids(&some);
        assert!(some.contains(&repo.blob("tcp\n")));
        assert!(some.contains(&repo.blob("udp\n")));
        for left_out in ["r\n", "io\n", "other\n"] {
            assert!(!some.contains(&repo.blob(left_out)), "{left_out}");
        }
    }

    #[test]
    fn a_walk_stops_at_its_limit() {
        let repo = Repo::new();
        let one = repo.commit(&[], vec![repo.set("a.txt", "1\n")]);
        let two = repo.commit(&[one], vec![repo.set("b.txt", "2\n")]);
        let walk = |max_objects, have: &[ObjectId]| {
            reachable(
                &repo.objects,
                &Reach {
                    want: &[two],
                    have,
                    max_objects,
                    ..Reach::default()
                },
            )
        };
        let all = walk(None, &[]).unwrap();
        assert_eq!(walk(Some(all.len()), &[]).unwrap(), all);
        assert!(matches!(
            walk(Some(all.len() - 1), &[]),
            Err(Error::TooManyObjects { limit }) if limit == all.len() - 1
        ));
        // The objects of what the asker holds count too: one's tree has a
        // node and a blob.
        assert!(matches!(
            walk(Some(1), &[one]),
            Err(Error::TooManyObjects { limit: 1 })
        ));
    }

    #[test]
    fn a_missing_object_is_an_error() {
        let repo = Repo::new();
        let head = repo.commit(&[], vec![repo.set("a.txt", "1\n")]);
        let (_, bytes) = repo.objects.get(&head).unwrap().unwrap();
        let partial = MemoryObjects::new();
        partial.put(Kind::Changeset, &bytes).unwrap();
        assert!(matches!(
            reachable(
                &partial,
                &Reach {
                    want: &[head],
                    ..Reach::default()
                }
            ),
            Err(Error::Missing { .. })
        ));
    }
}
