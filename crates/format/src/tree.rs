// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Trees: one directory, stored as a single flat node or, when it is large, as
//! a balanced structure of nodes whose boundaries depend only on entry names.
//!
//! A directory with up to [`MAX_FLAT_ENTRIES`] entries is always one flat node.
//! A larger one is split into leaves and internal nodes: walking the sorted
//! entries, a node ends after an entry whose name hashes below a threshold
//! (once the node holds at least [`MIN_NODE_ENTRIES`]), or when it reaches
//! [`MAX_NODE_ENTRIES`]. The same set of entries therefore always gives the
//! same nodes and the same root id, whatever order it was edited in, and an
//! edit only touches the nodes on its path.

use core::fmt;

use crate::codec::{DecodeError, Reader, put_bytes, put_id, put_uv};
use crate::id::{Kind, ObjectId};
use crate::name::{NameError, forbidden_as_symlink, validate_name};

/// Format byte of every tree node.
pub const FORMAT: u8 = 1;

/// A directory with at most this many entries is one flat node.
pub const MAX_FLAT_ENTRIES: usize = 1024;

/// A node of a split directory only ends on a name boundary once it holds
/// at least this many entries or children.
pub const MIN_NODE_ENTRIES: usize = 32;

/// A node of a split directory never holds more than this many entries or
/// children.
pub const MAX_NODE_ENTRIES: usize = 512;

const SPLIT_CONTEXT: &str = "elffuss-braid 2026 tree-split v1";

/// One name in 128 ends a node: 2³² / 128.
const SPLIT_THRESHOLD: u32 = 1 << 25;

const NODE_FLAT: u8 = 0;
const NODE_LEAF: u8 = 1;
const NODE_INTERNAL: u8 = 2;

/// A git object id, kept for submodules (gitlinks).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GitId {
    /// A SHA-1 id.
    Sha1([u8; 20]),
    /// A SHA-256 id.
    Sha256([u8; 32]),
}

impl GitId {
    /// The raw id bytes.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            GitId::Sha1(bytes) => bytes,
            GitId::Sha256(bytes) => bytes,
        }
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match self {
            GitId::Sha1(bytes) => {
                out.push(1);
                out.extend_from_slice(bytes);
            }
            GitId::Sha256(bytes) => {
                out.push(2);
                out.extend_from_slice(bytes);
            }
        }
    }

    pub(crate) fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let at = reader.position();
        match reader.u8()? {
            1 => Ok(GitId::Sha1(reader.array()?)),
            2 => Ok(GitId::Sha256(reader.array()?)),
            _ => Err(DecodeError::Invalid {
                at,
                what: "git id algorithm",
            }),
        }
    }
}

/// What a directory entry is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A regular file.
    File { blob: ObjectId, size: u64 },
    /// An executable file.
    Executable { blob: ObjectId, size: u64 },
    /// A symlink; the blob holds its target.
    Symlink { blob: ObjectId, size: u64 },
    /// A subdirectory.
    Tree { tree: ObjectId },
    /// A git submodule, pinned to a commit of another repository.
    Gitlink { commit: GitId },
}

impl EntryKind {
    /// The one-byte code of this kind in the encoding.
    pub const fn code(&self) -> u8 {
        match self {
            EntryKind::File { .. } => 0,
            EntryKind::Executable { .. } => 1,
            EntryKind::Symlink { .. } => 2,
            EntryKind::Tree { .. } => 3,
            EntryKind::Gitlink { .. } => 4,
        }
    }
}

/// One entry of a directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    /// The entry's name, a single path component.
    pub name: Vec<u8>,
    /// What the entry is.
    pub kind: EntryKind,
}

impl Entry {
    fn encode(&self, out: &mut Vec<u8>) {
        put_bytes(out, &self.name);
        out.push(self.kind.code());
        match &self.kind {
            EntryKind::File { blob, size }
            | EntryKind::Executable { blob, size }
            | EntryKind::Symlink { blob, size } => {
                put_id(out, blob);
                put_uv(out, *size);
            }
            EntryKind::Tree { tree } => put_id(out, tree),
            EntryKind::Gitlink { commit } => commit.encode(out),
        }
    }

    fn decode(reader: &mut Reader<'_>) -> Result<Self, TreeError> {
        let name = reader.bytes()?.to_vec();
        let at = reader.position();
        let kind = match reader.u8()? {
            0 => EntryKind::File {
                blob: reader.id()?,
                size: reader.uv()?,
            },
            1 => EntryKind::Executable {
                blob: reader.id()?,
                size: reader.uv()?,
            },
            2 => EntryKind::Symlink {
                blob: reader.id()?,
                size: reader.uv()?,
            },
            3 => EntryKind::Tree { tree: reader.id()? },
            4 => EntryKind::Gitlink {
                commit: GitId::decode(reader)?,
            },
            _ => {
                return Err(TreeError::Decode(DecodeError::Invalid {
                    at,
                    what: "entry kind",
                }));
            }
        };
        let entry = Entry { name, kind };
        validate_entry(&entry)?;
        Ok(entry)
    }
}

/// Smallest possible encoded entry: a 1-byte name, a kind and a SHA-1 gitlink.
const MIN_ENTRY_BYTES: usize = 1 + 1 + 1 + 1 + 20;

/// Smallest possible encoded child reference.
const MIN_CHILD_BYTES: usize = 1 + 1 + 32 + 1;

/// A reference from an internal node to the node below it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Child {
    /// Name of the last entry under this child.
    pub last_name: Vec<u8>,
    /// Id of the child node.
    pub node: ObjectId,
    /// Number of entries under this child.
    pub entry_count: u64,
}

/// One encoded tree object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// A whole directory of at most [`MAX_FLAT_ENTRIES`] entries.
    Flat(Vec<Entry>),
    /// A run of entries of a split directory.
    Leaf(Vec<Entry>),
    /// A run of children of a split directory.
    Internal {
        /// 1 for the nodes just above the leaves.
        level: u8,
        /// The nodes below, in name order.
        children: Vec<Child>,
    },
}

/// Why a tree is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// The bytes are not a canonical encoding.
    Decode(DecodeError),
    /// An entry name is not valid.
    Name { name: Vec<u8>, error: NameError },
    /// A name that git or braid reads is a symlink.
    SymlinkName(Vec<u8>),
    /// Names are not strictly ascending; `index` is the first entry out of order.
    Unsorted { index: usize },
    /// A node holds more entries or children than allowed, or none.
    BadNodeSize { count: usize },
    /// A child reference covers no entries.
    EmptyChild,
    /// A node referenced by a split directory is missing.
    MissingNode(ObjectId),
    /// The nodes are well-formed but not the canonical split of their entries.
    NotCanonical,
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TreeError::Decode(err) => write!(f, "{err}"),
            TreeError::Name { name, error } => {
                write!(f, "entry {:?}: {error}", String::from_utf8_lossy(name))
            }
            TreeError::SymlinkName(name) => write!(
                f,
                "entry {:?} must not be a symlink",
                String::from_utf8_lossy(name)
            ),
            TreeError::Unsorted { index } => {
                write!(f, "entry {index} is out of order or a duplicate")
            }
            TreeError::BadNodeSize { count } => write!(f, "node holds {count} items"),
            TreeError::EmptyChild => f.write_str("child reference covers no entries"),
            TreeError::MissingNode(id) => write!(f, "tree node {id} is missing"),
            TreeError::NotCanonical => {
                f.write_str("nodes are not the canonical split of the directory")
            }
        }
    }
}

impl std::error::Error for TreeError {}

impl From<DecodeError> for TreeError {
    fn from(err: DecodeError) -> Self {
        TreeError::Decode(err)
    }
}

/// Checks one entry on its own: its name, and the symlink restriction.
pub fn validate_entry(entry: &Entry) -> Result<(), TreeError> {
    validate_name(&entry.name).map_err(|error| TreeError::Name {
        name: entry.name.clone(),
        error,
    })?;
    if matches!(entry.kind, EntryKind::Symlink { .. }) && forbidden_as_symlink(&entry.name) {
        return Err(TreeError::SymlinkName(entry.name.clone()));
    }
    Ok(())
}

fn check_ascending<'a>(names: impl Iterator<Item = &'a [u8]>) -> Result<(), TreeError> {
    let mut previous: Option<&[u8]> = None;
    for (index, name) in names.enumerate() {
        if previous.is_some_and(|p| p >= name) {
            return Err(TreeError::Unsorted { index });
        }
        previous = Some(name);
    }
    Ok(())
}

impl Node {
    /// The canonical encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![FORMAT];
        match self {
            Node::Flat(entries) | Node::Leaf(entries) => {
                out.push(if matches!(self, Node::Flat(_)) {
                    NODE_FLAT
                } else {
                    NODE_LEAF
                });
                put_uv(&mut out, entries.len() as u64);
                for entry in entries {
                    entry.encode(&mut out);
                }
            }
            Node::Internal { level, children } => {
                out.push(NODE_INTERNAL);
                out.push(*level);
                put_uv(&mut out, children.len() as u64);
                for child in children {
                    put_bytes(&mut out, &child.last_name);
                    put_id(&mut out, &child.node);
                    put_uv(&mut out, child.entry_count);
                }
            }
        }
        out
    }

    /// The id of this node.
    pub fn id(&self) -> ObjectId {
        ObjectId::hash(Kind::Tree, &self.encode())
    }

    /// Decodes one node and checks everything that can be checked without
    /// the other nodes of its directory.
    pub fn decode(bytes: &[u8]) -> Result<Self, TreeError> {
        let mut reader = Reader::new(bytes);
        let at = reader.position();
        if reader.u8()? != FORMAT {
            return Err(DecodeError::Invalid {
                at,
                what: "tree format",
            }
            .into());
        }
        let at = reader.position();
        let node = match reader.u8()? {
            node_type @ (NODE_FLAT | NODE_LEAF) => {
                let count = reader.count(MIN_ENTRY_BYTES)?;
                let limit = if node_type == NODE_FLAT {
                    MAX_FLAT_ENTRIES
                } else {
                    MAX_NODE_ENTRIES
                };
                if count > limit || (node_type == NODE_LEAF && count == 0) {
                    return Err(TreeError::BadNodeSize { count });
                }
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    entries.push(Entry::decode(&mut reader)?);
                }
                check_ascending(entries.iter().map(|e| e.name.as_slice()))?;
                if node_type == NODE_FLAT {
                    Node::Flat(entries)
                } else {
                    Node::Leaf(entries)
                }
            }
            NODE_INTERNAL => {
                let level_at = reader.position();
                let level = reader.u8()?;
                if level == 0 {
                    return Err(DecodeError::Invalid {
                        at: level_at,
                        what: "internal node level",
                    }
                    .into());
                }
                let count = reader.count(MIN_CHILD_BYTES)?;
                if count == 0 || count > MAX_NODE_ENTRIES {
                    return Err(TreeError::BadNodeSize { count });
                }
                let mut children = Vec::with_capacity(count);
                for _ in 0..count {
                    let last_name = reader.bytes()?.to_vec();
                    validate_name(&last_name).map_err(|error| TreeError::Name {
                        name: last_name.clone(),
                        error,
                    })?;
                    let node = reader.id()?;
                    let entry_count = reader.uv()?;
                    if entry_count == 0 {
                        return Err(TreeError::EmptyChild);
                    }
                    children.push(Child {
                        last_name,
                        node,
                        entry_count,
                    });
                }
                check_ascending(children.iter().map(|c| c.last_name.as_slice()))?;
                Node::Internal { level, children }
            }
            _ => {
                return Err(DecodeError::Invalid {
                    at,
                    what: "tree node type",
                }
                .into());
            }
        };
        reader.finish()?;
        Ok(node)
    }
}

/// The encoded nodes of one directory, children before parents. The last
/// node is the root, whose id is the directory's id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directory {
    /// `(id, encoding)` of every node.
    pub nodes: Vec<(ObjectId, Vec<u8>)>,
}

impl Directory {
    /// The id of the directory: the id of its root node.
    pub fn root(&self) -> ObjectId {
        self.nodes
            .last()
            .map(|(id, _)| *id)
            .expect("a directory always has a root node")
    }
}

/// Whether a node ends after an item named `name` that makes its size `count`.
fn ends_node(level: u8, name: &[u8], count: usize) -> bool {
    if count >= MAX_NODE_ENTRIES {
        return true;
    }
    if count < MIN_NODE_ENTRIES {
        return false;
    }
    let mut hasher = blake3::Hasher::new_derive_key(SPLIT_CONTEXT);
    hasher.update(&[level]);
    hasher.update(name);
    let digest = hasher.finalize();
    let head = digest.as_bytes();
    u32::from_le_bytes([head[0], head[1], head[2], head[3]]) < SPLIT_THRESHOLD
}

fn push_node(nodes: &mut Vec<(ObjectId, Vec<u8>)>, node: &Node) -> ObjectId {
    let bytes = node.encode();
    let id = ObjectId::hash(Kind::Tree, &bytes);
    nodes.push((id, bytes));
    id
}

/// Builds the canonical nodes of a directory from its entries, in any order.
pub fn build_directory(mut entries: Vec<Entry>) -> Result<Directory, TreeError> {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    check_ascending(entries.iter().map(|e| e.name.as_slice()))?;
    for entry in &entries {
        validate_entry(entry)?;
    }

    let mut nodes = Vec::new();
    if entries.len() <= MAX_FLAT_ENTRIES {
        push_node(&mut nodes, &Node::Flat(entries));
        return Ok(Directory { nodes });
    }

    // Leaves.
    let total = entries.len();
    let mut children = Vec::new();
    let mut run = Vec::new();
    for (i, entry) in entries.into_iter().enumerate() {
        let ends = ends_node(0, &entry.name, run.len() + 1);
        run.push(entry);
        if ends || i + 1 == total {
            let last_name = run
                .last()
                .map(|e: &Entry| e.name.clone())
                .unwrap_or_default();
            let entry_count = run.len() as u64;
            let node = push_node(&mut nodes, &Node::Leaf(core::mem::take(&mut run)));
            children.push(Child {
                last_name,
                node,
                entry_count,
            });
        }
    }

    // Internal levels, until one node is left.
    let mut level = 1u8;
    while children.len() > 1 {
        let total = children.len();
        let mut parents = Vec::new();
        let mut run: Vec<Child> = Vec::new();
        for (i, child) in children.into_iter().enumerate() {
            let ends = ends_node(level, &child.last_name, run.len() + 1);
            run.push(child);
            if ends || i + 1 == total {
                let last_name = run.last().map(|c| c.last_name.clone()).unwrap_or_default();
                let entry_count = run.iter().map(|c| c.entry_count).sum();
                let node = push_node(
                    &mut nodes,
                    &Node::Internal {
                        level,
                        children: core::mem::take(&mut run),
                    },
                );
                parents.push(Child {
                    last_name,
                    node,
                    entry_count,
                });
            }
        }
        children = parents;
        level += 1;
    }
    Ok(Directory { nodes })
}

/// Reads every entry of the directory rooted at `root`, fetching nodes through
/// `fetch`, and checks that the nodes are exactly the canonical split of those
/// entries.
pub fn read_directory(
    root: ObjectId,
    mut fetch: impl FnMut(&ObjectId) -> Option<Vec<u8>>,
) -> Result<Vec<Entry>, TreeError> {
    let mut entries = Vec::new();
    collect(root, &mut fetch, &mut entries, None)?;
    if build_directory(entries.clone())?.root() != root {
        return Err(TreeError::NotCanonical);
    }
    Ok(entries)
}

fn collect(
    id: ObjectId,
    fetch: &mut impl FnMut(&ObjectId) -> Option<Vec<u8>>,
    entries: &mut Vec<Entry>,
    expected_level: Option<u8>,
) -> Result<(), TreeError> {
    let bytes = fetch(&id).ok_or(TreeError::MissingNode(id))?;
    if ObjectId::hash(Kind::Tree, &bytes) != id {
        return Err(TreeError::NotCanonical);
    }
    match (Node::decode(&bytes)?, expected_level) {
        (Node::Flat(found), None) => entries.extend(found),
        (Node::Leaf(found), Some(0)) => entries.extend(found),
        (Node::Internal { level, children }, expected) if expected.is_none_or(|e| e == level) => {
            for child in children {
                collect(child.node, fetch, entries, Some(level - 1))?;
            }
        }
        _ => return Err(TreeError::NotCanonical),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(text: &str) -> ObjectId {
        ObjectId::hash(Kind::Blob, text.as_bytes())
    }

    fn file(name: &str, text: &str) -> Entry {
        Entry {
            name: name.as_bytes().to_vec(),
            kind: EntryKind::File {
                blob: blob(text),
                size: text.len() as u64,
            },
        }
    }

    fn many(n: usize, content: &str) -> Vec<Entry> {
        (0..n).map(|i| file(&format!("f{i:05}"), content)).collect()
    }

    #[test]
    fn empty_tree() {
        let dir = build_directory(Vec::new()).unwrap();
        assert_eq!(dir.nodes.len(), 1);
        assert_eq!(dir.nodes[0].1, vec![FORMAT, NODE_FLAT, 0]);
        assert_eq!(
            dir.root().to_hex(),
            "3136b0f18562ee73b840ea348c54ae3c7e97e4cc3e547ea9dd03fa1cd8b384cf",
            "the empty tree id is part of format v1"
        );
    }

    #[test]
    fn small_tree_golden() {
        let entries = vec![
            file("README.md", "hello\n"),
            Entry {
                name: b"run.sh".to_vec(),
                kind: EntryKind::Executable {
                    blob: blob("#!/bin/sh\n"),
                    size: 10,
                },
            },
            Entry {
                name: b"latest".to_vec(),
                kind: EntryKind::Symlink {
                    blob: blob("README.md"),
                    size: 9,
                },
            },
            Entry {
                name: b"src".to_vec(),
                kind: EntryKind::Tree {
                    tree: build_directory(Vec::new()).unwrap().root(),
                },
            },
            Entry {
                name: b"vendor".to_vec(),
                kind: EntryKind::Gitlink {
                    commit: GitId::Sha1([0x11; 20]),
                },
            },
        ];
        let dir = build_directory(entries).unwrap();
        assert_eq!(dir.nodes.len(), 1);
        assert_eq!(
            dir.root().to_hex(),
            "a6c6d54bb7663d576e8f7a702e0670927e22098d6c15c10475e06b51dc3d8c1b"
        );
    }

    #[test]
    fn flat_up_to_the_limit_then_split() {
        let flat = build_directory(many(MAX_FLAT_ENTRIES, "x")).unwrap();
        assert_eq!(flat.nodes.len(), 1);
        assert!(matches!(
            Node::decode(&flat.nodes[0].1).unwrap(),
            Node::Flat(_)
        ));

        let split = build_directory(many(MAX_FLAT_ENTRIES + 1, "x")).unwrap();
        assert!(split.nodes.len() >= 3);
        assert!(matches!(
            Node::decode(&split.nodes.last().unwrap().1).unwrap(),
            Node::Internal { .. }
        ));
    }

    #[test]
    fn large_tree_golden() {
        let dir = build_directory(many(5000, "x")).unwrap();
        assert_eq!(
            dir.root().to_hex(),
            "2c88a412664c1fd862c3487e1313a045a20fe4f7bdeaf1bb71e08564edc7dcce"
        );
        assert_eq!(dir.nodes.len(), LARGE_TREE_NODES);
    }

    const LARGE_TREE_NODES: usize = 30;

    #[test]
    fn read_back_and_reject_non_canonical_splits() {
        let entries = many(3000, "x");
        let dir = build_directory(entries.clone()).unwrap();
        let store: std::collections::HashMap<ObjectId, Vec<u8>> =
            dir.nodes.iter().cloned().collect();
        let read = read_directory(dir.root(), |id| store.get(id).cloned()).unwrap();
        assert_eq!(read, entries);

        // The same entries packed into one oversized flat node are refused.
        let fake = Node::Flat(entries.clone());
        let bytes = fake.encode();
        assert!(matches!(
            Node::decode(&bytes),
            Err(TreeError::BadNodeSize { .. })
        ));

        // Two leaves split at the wrong place, under a hand-made root.
        let left = Node::Leaf(entries[..100].to_vec());
        let right = Node::Leaf(entries[100..500].to_vec());
        let root = Node::Internal {
            level: 1,
            children: vec![
                Child {
                    last_name: entries[99].name.clone(),
                    node: left.id(),
                    entry_count: 100,
                },
                Child {
                    last_name: entries[499].name.clone(),
                    node: right.id(),
                    entry_count: 400,
                },
            ],
        };
        let store: std::collections::HashMap<ObjectId, Vec<u8>> = [&left, &right, &root]
            .iter()
            .map(|n| (n.id(), n.encode()))
            .collect();
        assert_eq!(
            read_directory(root.id(), |id| store.get(id).cloned()),
            Err(TreeError::NotCanonical)
        );
    }

    #[test]
    fn decode_rejects_bad_input() {
        let unsorted = Node::Flat(vec![file("b", "1"), file("a", "2")]).encode();
        assert_eq!(
            Node::decode(&unsorted),
            Err(TreeError::Unsorted { index: 1 })
        );
        let duplicate = Node::Flat(vec![file("a", "1"), file("a", "2")]).encode();
        assert_eq!(
            Node::decode(&duplicate),
            Err(TreeError::Unsorted { index: 1 })
        );
        let reserved = Node::Flat(vec![file(".GIT", "1")]).encode();
        assert!(matches!(
            Node::decode(&reserved),
            Err(TreeError::Name { .. })
        ));
        let symlink = Node::Flat(vec![Entry {
            name: b".gitmodules".to_vec(),
            kind: EntryKind::Symlink {
                blob: blob("x"),
                size: 1,
            },
        }])
        .encode();
        assert!(matches!(
            Node::decode(&symlink),
            Err(TreeError::SymlinkName(_))
        ));
        let mut trailing = Node::Flat(vec![file("a", "1")]).encode();
        trailing.push(0);
        assert!(matches!(
            Node::decode(&trailing),
            Err(TreeError::Decode(DecodeError::TrailingBytes { .. }))
        ));
        assert!(Node::decode(&[]).is_err());
        assert!(Node::decode(&[2, 0, 0]).is_err());
        assert!(Node::decode(&[FORMAT, 9]).is_err());
        assert!(Node::decode(&[FORMAT, NODE_LEAF, 0]).is_err());
        assert!(Node::decode(&[FORMAT, NODE_INTERNAL, 0, 0]).is_err());
    }
}
