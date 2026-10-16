// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Typed access to stored objects: blobs, trees and changesets.

use std::borrow::Cow;
use std::collections::HashMap;
use std::rc::Rc;

use braid_format::changeset::{MAX_INLINE_CHANGES_BYTES, decode_change_list};
use braid_format::{
    Change, Changes, Changeset, Entry, EntryKind, Kind, Node, NodeCache, ObjectId, Objects,
    TreeError, build_directory, find_entry, read_directory,
};

use crate::error::{Error, store_error};
use crate::path::check_depth;

/// Id of the tree with no entries: the root of an empty repository, and the
/// base of a changeset without parents. Part of format v1.
pub const EMPTY_TREE: ObjectId = ObjectId::from_bytes([
    0x31, 0x36, 0xb0, 0xf1, 0x85, 0x62, 0xee, 0x73, 0xb8, 0x40, 0xea, 0x34, 0x8c, 0x54, 0xae, 0x3c,
    0x7e, 0x97, 0xe4, 0xcc, 0x3e, 0x54, 0x7e, 0xa9, 0xdd, 0x03, 0xfa, 0x1c, 0xd8, 0xb3, 0x84, 0xcf,
]);

/// The encoding of the empty tree.
pub(crate) fn empty_tree_node() -> Vec<u8> {
    Node::Flat(Vec::new()).encode()
}

/// Reads an object that must be present and of `kind`, and checks that its
/// bytes hash to its id.
pub fn read_object<O: Objects>(objects: &O, id: ObjectId, kind: Kind) -> Result<Vec<u8>, Error> {
    match objects.get(&id).map_err(store_error)? {
        None => Err(Error::Missing { id, kind }),
        Some((found, _)) if found != kind => Err(Error::WrongKind {
            id,
            expected: kind,
            found,
        }),
        Some((_, bytes)) if ObjectId::hash(kind, &bytes) != id => Err(Error::Corrupt { id }),
        Some((_, bytes)) => Ok(bytes),
    }
}

/// The size of an object that must be stored as `kind`, from
/// [`Objects::stat`]: the content is not read, if the store can avoid it.
pub fn stat_object<O: Objects>(objects: &O, id: ObjectId, kind: Kind) -> Result<u64, Error> {
    match objects.stat(&id).map_err(store_error)? {
        None => Err(Error::Missing { id, kind }),
        Some((found, size)) if found == kind => Ok(size),
        Some((found, _)) => Err(Error::WrongKind {
            id,
            expected: kind,
            found,
        }),
    }
}

/// Reads the content of a blob.
pub fn read_blob<O: Objects>(objects: &O, id: ObjectId) -> Result<Vec<u8>, Error> {
    read_object(objects, id, Kind::Blob)
}

/// Reads every entry of a directory, flat or split, in name order.
///
/// A split directory is checked to be exactly the canonical split of its
/// entries. The empty tree is known without reading it.
pub fn read_tree<O: Objects>(objects: &O, id: ObjectId) -> Result<Vec<Entry>, Error> {
    if id == EMPTY_TREE {
        return Ok(Vec::new());
    }
    let mut failure = None;
    read_directory(id, fetch_node(objects, &mut failure))
        .map_err(|error| tree_error(id, failure, error))
}

/// Fetches the nodes of trees for the readers of the format crate, which
/// see a node they cannot fetch as missing: `failure` keeps why. The empty
/// tree is known without reading it.
fn fetch_node<'a, O: Objects>(
    objects: &'a O,
    failure: &'a mut Option<Error>,
) -> impl FnMut(&ObjectId) -> Option<Vec<u8>> + 'a {
    move |node| {
        if *node == EMPTY_TREE {
            return Some(empty_tree_node());
        }
        match objects.get(node) {
            Ok(Some((Kind::Tree, bytes))) => Some(bytes),
            Ok(Some((found, _))) => {
                failure.get_or_insert(Error::WrongKind {
                    id: *node,
                    expected: Kind::Tree,
                    found,
                });
                None
            }
            Ok(None) => None,
            Err(error) => {
                failure.get_or_insert(store_error(error));
                None
            }
        }
    }
}

/// The error for the directory `id`, which a reader of the format crate
/// failed on: why a node could not be fetched, if that was it.
fn tree_error(id: ObjectId, failure: Option<Error>, error: TreeError) -> Error {
    match (failure, error) {
        (Some(failure), _) => failure,
        (None, TreeError::MissingNode(node)) => Error::Missing {
            id: node,
            kind: Kind::Tree,
        },
        (None, error) => Error::Tree { id, error },
    }
}

/// Stores a directory: builds its canonical nodes from `entries`, in any
/// order, stores every node, and returns the directory's id.
pub fn write_tree<O: Objects>(objects: &O, entries: Vec<Entry>) -> Result<ObjectId, Error> {
    let directory = build_directory(entries).map_err(Error::Entries)?;
    for (_, bytes) in &directory.nodes {
        objects.put(Kind::Tree, bytes).map_err(store_error)?;
    }
    Ok(directory.root())
}

/// Reads and decodes a changeset. Its change list may be external: see
/// [`read_changes`].
pub fn read_changeset<O: Objects>(objects: &O, id: ObjectId) -> Result<Changeset, Error> {
    let bytes = read_object(objects, id, Kind::Changeset)?;
    Changeset::decode(&bytes).map_err(|error| Error::Changeset { id, error })
}

/// The change list of `changeset`, whose id is `id`: borrowed when it is
/// inline, fetched and decoded when it is in its own blob.
///
/// An external list must hold as many changes as the changeset declares, and
/// must be too long to have been stored inline.
pub fn read_changes<'c, O: Objects>(
    objects: &O,
    id: ObjectId,
    changeset: &'c Changeset,
) -> Result<Cow<'c, [Change]>, Error> {
    match &changeset.changes {
        Changes::Inline(changes) => Ok(Cow::Borrowed(changes)),
        Changes::External { list, count } => {
            let bytes = read_blob(objects, *list)?;
            if bytes.len() <= MAX_INLINE_CHANGES_BYTES {
                return Err(Error::ExternalListFitsInline {
                    changeset: id,
                    len: bytes.len(),
                });
            }
            let changes =
                decode_change_list(&bytes).map_err(|error| Error::Changeset { id, error })?;
            if changes.len() as u64 != *count {
                return Err(Error::ChangeCount {
                    changeset: id,
                    declared: *count,
                    found: changes.len(),
                });
            }
            Ok(Cow::Owned(changes))
        }
    }
}

/// The entry at `path` in the tree `root`, or `None` if there is none (or a
/// prefix of `path` is not a directory). A path deeper than
/// [`MAX_PATH_DEPTH`](crate::MAX_PATH_DEPTH) is an error.
pub fn entry_at<O: Objects>(
    objects: &O,
    root: ObjectId,
    path: &[u8],
) -> Result<Option<EntryKind>, Error> {
    check_depth(path)?;
    Ok(Trees::new(objects).find(root, path)?.into_entry())
}

/// What is at a path of a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// This entry.
    Entry(EntryKind),
    /// Nothing, and every prefix of the path that exists is a directory.
    Absent,
    /// Nothing, because the prefix of this many bytes is not a directory.
    Blocked(usize),
}

impl Found {
    pub(crate) fn into_entry(self) -> Option<EntryKind> {
        match self {
            Found::Entry(kind) => Some(kind),
            Found::Absent | Found::Blocked(_) => None,
        }
    }

    pub(crate) fn entry(&self) -> Option<&EntryKind> {
        match self {
            Found::Entry(kind) => Some(kind),
            Found::Absent | Found::Blocked(_) => None,
        }
    }
}

/// The entry named `name` in a directory's sorted entries.
pub(crate) fn lookup<'e>(entries: &'e [Entry], name: &[u8]) -> Option<&'e EntryKind> {
    entries
        .binary_search_by(|entry| entry.name.as_slice().cmp(name))
        .ok()
        .map(|index| &entries[index].kind)
}

/// Directories read during one operation, so that each one, or each node of
/// a split one, is fetched and checked once.
pub(crate) struct Trees<'o, O> {
    objects: &'o O,
    /// Directories read whole, by id.
    directories: HashMap<ObjectId, Rc<[Entry]>>,
    /// Nodes read one path at a time.
    nodes: NodeCache,
}

impl<'o, O: Objects> Trees<'o, O> {
    pub(crate) fn new(objects: &'o O) -> Self {
        Trees {
            objects,
            directories: HashMap::new(),
            nodes: NodeCache::new(),
        }
    }

    pub(crate) fn objects(&self) -> &'o O {
        self.objects
    }

    /// The entries of a directory, in name order.
    pub(crate) fn read(&mut self, id: ObjectId) -> Result<Rc<[Entry]>, Error> {
        if let Some(entries) = self.directories.get(&id) {
            return Ok(Rc::clone(entries));
        }
        let entries: Rc<[Entry]> = read_tree(self.objects, id)?.into();
        self.directories.insert(id, Rc::clone(&entries));
        Ok(entries)
    }

    /// The entries of a directory that may not exist.
    pub(crate) fn read_or_empty(&mut self, id: Option<ObjectId>) -> Result<Rc<[Entry]>, Error> {
        match id {
            Some(id) => self.read(id),
            None => Ok(Vec::new().into()),
        }
    }

    /// The entry named `name` in the directory `dir`. Only the nodes on the
    /// way to the name are read, unless the directory was read whole already.
    pub(crate) fn entry(&mut self, dir: ObjectId, name: &[u8]) -> Result<Option<EntryKind>, Error> {
        if let Some(entries) = self.directories.get(&dir) {
            return Ok(lookup(entries, name).cloned());
        }
        let mut failure = None;
        find_entry(
            &mut self.nodes,
            dir,
            name,
            fetch_node(self.objects, &mut failure),
        )
        .map_err(|error| tree_error(dir, failure, error))
    }

    /// What is at `path` in the tree `root`.
    pub(crate) fn find(&mut self, root: ObjectId, path: &[u8]) -> Result<Found, Error> {
        let mut dir = root;
        let mut start = 0;
        loop {
            let slash = path[start..].iter().position(|byte| *byte == b'/');
            let end = slash.map_or(path.len(), |slash| start + slash);
            match (self.entry(dir, &path[start..end])?, slash) {
                (None, _) => return Ok(Found::Absent),
                (Some(kind), None) => return Ok(Found::Entry(kind)),
                (Some(EntryKind::Tree { tree }), Some(_)) => {
                    dir = tree;
                    start = end + 1;
                }
                (Some(_), Some(_)) => return Ok(Found::Blocked(end)),
            }
        }
    }

    /// Length of the deepest proper prefix of `path` that is a directory in
    /// the tree `root`; 0 when that is the root itself.
    pub(crate) fn deepest_directory(
        &mut self,
        root: ObjectId,
        path: &[u8],
    ) -> Result<usize, Error> {
        let mut dir = root;
        let mut deepest = 0;
        let mut start = 0;
        while let Some(slash) = path[start..].iter().position(|byte| *byte == b'/') {
            let end = start + slash;
            match self.entry(dir, &path[start..end])? {
                Some(EntryKind::Tree { tree }) => {
                    dir = tree;
                    deepest = end;
                    start = end + 1;
                }
                _ => break,
            }
        }
        Ok(deepest)
    }

    /// Length of the shortest proper prefix of `path` that does not exist in
    /// the tree `root`, if there is one and every prefix above it is a
    /// directory.
    pub(crate) fn first_missing_directory(
        &mut self,
        root: ObjectId,
        path: &[u8],
    ) -> Result<Option<usize>, Error> {
        let mut dir = root;
        let mut start = 0;
        while let Some(slash) = path[start..].iter().position(|byte| *byte == b'/') {
            let end = start + slash;
            match self.entry(dir, &path[start..end])? {
                Some(EntryKind::Tree { tree }) => {
                    dir = tree;
                    start = end + 1;
                }
                Some(_) => return Ok(None),
                None => return Ok(Some(end)),
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Repo;
    use braid_format::MemoryObjects;

    #[test]
    fn empty_tree_id_is_the_format_constant() {
        assert_eq!(build_directory(Vec::new()).unwrap().root(), EMPTY_TREE);
        assert_eq!(ObjectId::hash(Kind::Tree, &empty_tree_node()), EMPTY_TREE);
        assert_eq!(
            EMPTY_TREE.to_hex(),
            "3136b0f18562ee73b840ea348c54ae3c7e97e4cc3e547ea9dd03fa1cd8b384cf"
        );
        // Known without being stored.
        assert_eq!(read_tree(&MemoryObjects::new(), EMPTY_TREE).unwrap(), []);
    }

    #[test]
    fn typed_reads_check_kind_and_presence() {
        let objects = MemoryObjects::new();
        let blob = objects.put(Kind::Blob, b"hello\n").unwrap();
        assert_eq!(read_blob(&objects, blob).unwrap(), b"hello\n");
        assert!(matches!(
            read_tree(&objects, blob),
            Err(Error::WrongKind {
                expected: Kind::Tree,
                found: Kind::Blob,
                ..
            })
        ));
        assert!(matches!(
            read_changeset(&objects, blob),
            Err(Error::WrongKind { .. })
        ));
        let absent = ObjectId::hash(Kind::Blob, b"absent");
        assert!(matches!(
            read_blob(&objects, absent),
            Err(Error::Missing {
                kind: Kind::Blob,
                ..
            })
        ));
        assert!(matches!(
            read_tree(&objects, absent),
            Err(Error::Missing {
                kind: Kind::Tree,
                ..
            })
        ));
        // A changeset whose bytes do not decode.
        let junk = objects.put(Kind::Changeset, b"junk").unwrap();
        assert!(matches!(
            read_changeset(&objects, junk),
            Err(Error::Changeset { .. })
        ));
    }

    #[test]
    fn split_directories_are_written_and_read_back() {
        let repo = Repo::new();
        let entries: Vec<Entry> = (0..3000)
            .map(|i| Entry {
                name: format!("f{i:05}").into_bytes(),
                kind: repo.file_kind("x"),
            })
            .collect();
        let id = write_tree(&repo.objects, entries.clone()).unwrap();
        assert_eq!(read_tree(&repo.objects, id).unwrap(), entries);
        assert!(
            repo.objects.len() > 3,
            "a split directory has several nodes"
        );
        let duplicate = vec![entries[0].clone(), entries[0].clone()];
        assert!(matches!(
            write_tree(&repo.objects, duplicate),
            Err(Error::Entries(TreeError::Unsorted { .. }))
        ));
    }

    #[test]
    fn find_reports_entries_absences_and_blockers() {
        let repo = Repo::new();
        let root = repo.tree(&[("a/b/c.txt", "1"), ("f", "2")]);
        let mut trees = Trees::new(&repo.objects);
        assert!(matches!(
            trees.find(root, b"a/b/c.txt").unwrap(),
            Found::Entry(EntryKind::File { .. })
        ));
        assert!(matches!(
            trees.find(root, b"a/b").unwrap(),
            Found::Entry(EntryKind::Tree { .. })
        ));
        assert_eq!(trees.find(root, b"a/x/y").unwrap(), Found::Absent);
        assert_eq!(trees.find(root, b"f/g/h").unwrap(), Found::Blocked(1));
        assert_eq!(trees.deepest_directory(root, b"a/b/new/x").unwrap(), 3);
        assert_eq!(trees.deepest_directory(root, b"x/y").unwrap(), 0);
        assert_eq!(
            trees.first_missing_directory(root, b"a/z/q").unwrap(),
            Some(3)
        );
        assert_eq!(
            trees.first_missing_directory(root, b"a/b/c.txt").unwrap(),
            None
        );
        assert_eq!(trees.first_missing_directory(root, b"f/g").unwrap(), None);
        assert_eq!(
            entry_at(&repo.objects, root, b"f").unwrap(),
            Some(repo.file_kind("2"))
        );
        assert_eq!(entry_at(&repo.objects, root, b"f/g").unwrap(), None);
    }
}
