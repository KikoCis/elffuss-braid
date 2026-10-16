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
//!
//! [`build_directory`] makes every node of a directory from all of its
//! entries, and [`read_directory`] reads them all back. [`find_entry`] and
//! [`update_directory`] work one path at a time instead: they read only the
//! nodes on the way to the names they look up or edit, so what they cost
//! follows the number of names and the depth of the directory, not its size.

use core::cmp::Ordering;
use core::fmt;
use std::collections::HashMap;
use std::rc::Rc;

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
    check_entry(&entry.name, &entry.kind)
}

fn check_entry(name: &[u8], kind: &EntryKind) -> Result<(), TreeError> {
    validate_name(name).map_err(|error| TreeError::Name {
        name: name.to_vec(),
        error,
    })?;
    if matches!(kind, EntryKind::Symlink { .. }) && forbidden_as_symlink(name) {
        return Err(TreeError::SymlinkName(name.to_vec()));
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

/// Whether a node ends after an item that makes it hold `count` items.
/// `hit`, whether the item's name hashes below the threshold, is only asked
/// when it matters.
fn ends(count: usize, hit: impl FnOnce() -> bool) -> bool {
    count >= MAX_NODE_ENTRIES || (count >= MIN_NODE_ENTRIES && hit())
}

/// The splitting rule, with one hasher kept for every name it looks at.
struct Splitter {
    hasher: blake3::Hasher,
}

impl Splitter {
    fn new() -> Self {
        Splitter {
            hasher: blake3::Hasher::new_derive_key(SPLIT_CONTEXT),
        }
    }

    /// Whether `H(level, name)` falls below the threshold.
    fn hit(&mut self, level: u8, name: &[u8]) -> bool {
        self.hasher.reset();
        self.hasher.update(&[level]);
        self.hasher.update(name);
        let digest = self.hasher.finalize();
        let head = digest.as_bytes();
        u32::from_le_bytes([head[0], head[1], head[2], head[3]]) < SPLIT_THRESHOLD
    }

    /// Whether a node of `level` ends after an item named `name` that makes
    /// it hold `count` items.
    fn ends(&mut self, level: u8, name: &[u8], count: usize) -> bool {
        ends(count, || self.hit(level, name))
    }
}

/// What the nodes of one level of a split directory hold: entries in the
/// leaves, references to the nodes below in internal nodes.
trait Item: Clone + PartialEq {
    /// The name that orders items and that the splitting rule hashes.
    fn name(&self) -> &[u8];
    /// How many entries of the directory it stands for.
    fn entries(&self) -> u64;
    /// The node of `level` that holds `items`.
    fn node(level: u8, items: Vec<Self>) -> Node;
    /// The items of `node`, if it holds this kind of item.
    fn items(node: &Node) -> Option<&[Self]>;
}

impl Item for Entry {
    fn name(&self) -> &[u8] {
        &self.name
    }

    fn entries(&self) -> u64 {
        1
    }

    fn node(_: u8, items: Vec<Self>) -> Node {
        Node::Leaf(items)
    }

    fn items(node: &Node) -> Option<&[Self]> {
        match node {
            Node::Leaf(entries) => Some(entries),
            _ => None,
        }
    }
}

impl Item for Child {
    fn name(&self) -> &[u8] {
        &self.last_name
    }

    fn entries(&self) -> u64 {
        self.entry_count
    }

    fn node(level: u8, items: Vec<Self>) -> Node {
        Node::Internal {
            level,
            children: items,
        }
    }

    fn items(node: &Node) -> Option<&[Self]> {
        match node {
            Node::Internal { children, .. } => Some(children),
            _ => None,
        }
    }
}

/// A node cut by a [`Chunker`]: the reference a parent holds to it, and its
/// encoding.
struct Made {
    child: Child,
    bytes: Vec<u8>,
}

/// Cuts the items of one level, given in name order, into nodes.
struct Chunker<I> {
    level: u8,
    /// The items of the node being filled.
    run: Vec<I>,
    made: Vec<Made>,
}

impl<I: Item> Chunker<I> {
    fn new(level: u8) -> Self {
        Chunker {
            level,
            run: Vec::new(),
            made: Vec::new(),
        }
    }

    /// How many items the node being filled holds with one more.
    fn next_count(&self) -> usize {
        self.run.len() + 1
    }

    /// Whether the last item ended a node, so that the next one starts a
    /// new one.
    fn at_boundary(&self) -> bool {
        self.run.is_empty()
    }

    /// Adds the next item, and ends its node after it if `cut`.
    fn push(&mut self, item: I, cut: bool) {
        self.run.push(item);
        if cut {
            self.cut();
        }
    }

    /// Adds the next item, and ends its node where the splitting rule says.
    fn add(&mut self, item: I, splitter: &mut Splitter) {
        let cut = splitter.ends(self.level, item.name(), self.next_count());
        self.push(item, cut);
    }

    /// Ends the node being filled, if it holds anything.
    fn cut(&mut self) {
        let Some(last) = self.run.last() else {
            return;
        };
        let last_name = last.name().to_vec();
        // Saturating: only references read from a corrupt directory could
        // add up past the largest count.
        let entry_count = self
            .run
            .iter()
            .fold(0u64, |sum, item| sum.saturating_add(item.entries()));
        let node = I::node(self.level, core::mem::take(&mut self.run));
        let bytes = node.encode();
        self.made.push(Made {
            child: Child {
                last_name,
                node: ObjectId::hash(Kind::Tree, &bytes),
                entry_count,
            },
            bytes,
        });
    }

    /// Ends the last node, and returns every node cut, in order.
    fn finish(mut self) -> Vec<Made> {
        self.cut();
        self.made
    }
}

/// One node of a directory built from scratch.
struct Built {
    id: ObjectId,
    bytes: Vec<u8>,
    /// Whether it holds entries: a flat node or a leaf.
    holds_entries: bool,
}

/// Builds the canonical nodes of a directory from its entries, in any order.
pub fn build_directory(mut entries: Vec<Entry>) -> Result<Directory, TreeError> {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    check_ascending(entries.iter().map(|e| e.name.as_slice()))?;
    for entry in &entries {
        validate_entry(entry)?;
    }
    let nodes = build_nodes(entries)
        .into_iter()
        .map(|node| (node.id, node.bytes))
        .collect();
    Ok(Directory { nodes })
}

/// The canonical nodes of sorted, valid entries, children before parents.
fn build_nodes(entries: Vec<Entry>) -> Vec<Built> {
    if entries.len() <= MAX_FLAT_ENTRIES {
        let bytes = Node::Flat(entries).encode();
        return vec![Built {
            id: ObjectId::hash(Kind::Tree, &bytes),
            bytes,
            holds_entries: true,
        }];
    }
    let mut splitter = Splitter::new();
    let mut nodes = Vec::new();
    let mut leaves = Chunker::new(0);
    for entry in entries {
        leaves.add(entry, &mut splitter);
    }
    let mut children = keep(&mut nodes, leaves.finish(), true);
    // Internal levels, until one node is left.
    let mut level = 1u8;
    while children.len() > 1 {
        let mut chunker = Chunker::new(level);
        for child in children {
            chunker.add(child, &mut splitter);
        }
        children = keep(&mut nodes, chunker.finish(), false);
        level += 1;
    }
    nodes
}

/// Moves the nodes of one level into `nodes`, and returns the references to
/// them, for the level above.
fn keep(nodes: &mut Vec<Built>, made: Vec<Made>, holds_entries: bool) -> Vec<Child> {
    made.into_iter()
        .map(|made| {
            nodes.push(Built {
                id: made.child.node,
                bytes: made.bytes,
                holds_entries,
            });
            made.child
        })
        .collect()
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

// ---------------------------------------------------------------------------
// One path at a time

/// One edit of a directory: the entry a name gets, or its removal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryEdit {
    /// The name.
    pub name: Vec<u8>,
    /// What the name becomes; `None` removes it. Removing a name the
    /// directory does not have changes nothing.
    pub kind: Option<EntryKind>,
}

/// Checks edits on their own: names strictly ascending, and every entry they
/// set valid.
pub fn check_edits(edits: &[EntryEdit]) -> Result<(), TreeError> {
    check_ascending(edits.iter().map(|edit| edit.name.as_slice()))?;
    for edit in edits {
        if let Some(kind) = &edit.kind {
            check_entry(&edit.name, kind)?;
        }
    }
    Ok(())
}

/// A node that an update of a directory made, and that the directory did not
/// have before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewNode {
    /// Its id.
    pub id: ObjectId,
    /// Its encoding.
    pub bytes: Vec<u8>,
    /// The node of the directory before the update that it most likely
    /// replaces: a hint for a store that may keep it as a difference from
    /// that node, as [`Objects::put_similar`](crate::Objects::put_similar)
    /// takes.
    pub similar: Option<ObjectId>,
}

/// A directory after an update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Updated {
    /// Its id: the id of its root node.
    pub root: ObjectId,
    /// How many entries it holds.
    pub entries: u64,
    /// Its nodes that the directory before the update did not have, children
    /// before parents. With the nodes it kept, they are exactly the nodes
    /// that [`build_directory`] makes of its entries.
    pub nodes: Vec<NewNode>,
}

/// Nodes of directories, each fetched, checked against its id and decoded
/// once, for the readers that go one path at a time.
#[derive(Debug, Default)]
pub struct NodeCache {
    nodes: HashMap<ObjectId, Rc<Node>>,
}

impl NodeCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many nodes it holds: each was fetched once.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether it holds no node.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node `id`. The first time, it is fetched through `fetch`, checked
    /// against its id and decoded.
    pub fn get(
        &mut self,
        id: ObjectId,
        fetch: &mut impl FnMut(&ObjectId) -> Option<Vec<u8>>,
    ) -> Result<Rc<Node>, TreeError> {
        if let Some(node) = self.nodes.get(&id) {
            return Ok(Rc::clone(node));
        }
        let bytes = fetch(&id).ok_or(TreeError::MissingNode(id))?;
        if ObjectId::hash(Kind::Tree, &bytes) != id {
            return Err(TreeError::NotCanonical);
        }
        let node = Rc::new(Node::decode(&bytes)?);
        self.nodes.insert(id, Rc::clone(&node));
        Ok(node)
    }
}

/// The entry named `name` in the directory whose root node is `root`, or
/// `None` if it has none.
///
/// A split directory is searched from the root down: each internal node leads
/// to the child whose last name is the first at or after `name`, so only the
/// nodes on that path are read. Each of them is checked against its id and
/// against the reference that leads to it: its level, its last name and its
/// number of entries. Whether the directory as a whole is the canonical split
/// of its entries is what [`read_directory`] checks, by reading all of it.
pub fn find_entry(
    cache: &mut NodeCache,
    root: ObjectId,
    name: &[u8],
    mut fetch: impl FnMut(&ObjectId) -> Option<Vec<u8>>,
) -> Result<Option<EntryKind>, TreeError> {
    let mut node = cache.get(root, &mut fetch)?;
    match &*node {
        Node::Flat(entries) => return Ok(search(entries, name)),
        Node::Leaf(_) => return Err(TreeError::NotCanonical),
        Node::Internal { children, .. } => {
            split_total(children)?;
        }
    }
    loop {
        let (level, children) = match &*node {
            Node::Leaf(entries) => return Ok(search(entries, name)),
            Node::Internal { level, children } => (*level, children),
            Node::Flat(_) => return Err(TreeError::NotCanonical),
        };
        let index = children.partition_point(|child| child.last_name.as_slice() < name);
        let Some(child) = children.get(index) else {
            // After the last name of the directory.
            return Ok(None);
        };
        let below = cache.get(child.node, &mut fetch)?;
        check_child(&below, level - 1, child)?;
        node = below;
    }
}

/// Applies `edits`, sorted by name, to the directory whose root node is
/// `root`, and returns the directory they make: its root, its number of
/// entries, and the nodes to store. The result is exactly what
/// [`build_directory`] makes of the edited entries, node for node.
///
/// A flat directory is built again from its entries. A split one is updated
/// in place, one run of leaves at a time. A run starts at the leaf that holds
/// the first name left to edit, or would hold it: every boundary before it is
/// unchanged, since where a node ends depends only on the names up to there. The run
/// is cut into new leaves by the splitting rule, edits included, and it goes
/// on into the following leaves until a new leaf ends exactly where an old
/// one ended: from there the rule sees the same names from the same start,
/// so every following leaf is unchanged, up to the next name to edit. The
/// new leaves replace the old ones of the run in the level above, which is
/// updated the same way, up to the root. Only the nodes of the runs, and the
/// nodes on the paths that lead to them, are read. A directory that grows
/// past [`MAX_FLAT_ENTRIES`] entries is split, and one that shrinks to that
/// many or fewer is read whole and made flat again.
///
/// The edits are checked before any node is read: an invalid one is an
/// error. Every node read is checked against its id and the reference that
/// leads to it, and the old boundaries inside the runs are checked against
/// the splitting rule; the rest of the directory is assumed canonical, as
/// every directory this crate builds is.
pub fn update_directory(
    cache: &mut NodeCache,
    root: ObjectId,
    edits: &[EntryEdit],
    mut fetch: impl FnMut(&ObjectId) -> Option<Vec<u8>>,
) -> Result<Updated, TreeError> {
    check_edits(edits)?;
    let top = cache.get(root, &mut fetch)?;
    match &*top {
        Node::Flat(entries) => {
            // An empty node is nothing for a store to start from.
            let similar = (!entries.is_empty()).then_some(root);
            Ok(rebuilt(
                root,
                similar,
                merge_items(entries.iter().cloned(), entry_changes(edits)),
            ))
        }
        Node::Leaf(_) => Err(TreeError::NotCanonical),
        Node::Internal { level, children } => {
            let entry_count = split_total(children)?;
            let last_name = children
                .last()
                .map(|child| child.last_name.clone())
                .unwrap_or_default();
            Split {
                cache,
                fetch: &mut fetch,
                root: Child {
                    last_name,
                    node: root,
                    entry_count,
                },
                height: *level,
                splitter: Splitter::new(),
                made: Vec::new(),
            }
            .update(edits)
        }
    }
}

/// The entry named `name` among sorted entries.
fn search(entries: &[Entry], name: &[u8]) -> Option<EntryKind> {
    entries
        .binary_search_by(|entry| entry.name.as_slice().cmp(name))
        .ok()
        .map(|index| entries[index].kind.clone())
}

/// The number of entries under a split directory's root, whose children
/// are `children`: it has at least two, else the one below would be the
/// root, and more entries than a flat node holds.
fn split_total(children: &[Child]) -> Result<u64, TreeError> {
    let total = children
        .iter()
        .try_fold(0u64, |sum, child| sum.checked_add(child.entry_count));
    match total {
        Some(total) if children.len() >= 2 && total > MAX_FLAT_ENTRIES as u64 => Ok(total),
        _ => Err(TreeError::NotCanonical),
    }
}

/// Checks that `node`, reached through `child`, is what that reference says:
/// a node of `level`, with the same last name and number of entries.
fn check_child(node: &Node, level: u8, child: &Child) -> Result<(), TreeError> {
    let (last, count) = match node {
        Node::Leaf(entries) if level == 0 => (
            entries.last().map(|entry| entry.name.as_slice()),
            Some(entries.len() as u64),
        ),
        Node::Internal {
            level: found,
            children,
        } if *found == level => (
            children.last().map(|child| child.last_name.as_slice()),
            children
                .iter()
                .try_fold(0u64, |sum, child| sum.checked_add(child.entry_count)),
        ),
        _ => return Err(TreeError::NotCanonical),
    };
    if last == Some(child.last_name.as_slice()) && count == Some(child.entry_count) {
        Ok(())
    } else {
        Err(TreeError::NotCanonical)
    }
}

/// Edits as changes to the items of the leaf level.
fn entry_changes(edits: &[EntryEdit]) -> Vec<(Vec<u8>, Option<Entry>)> {
    edits
        .iter()
        .map(|edit| {
            let entry = edit.kind.clone().map(|kind| Entry {
                name: edit.name.clone(),
                kind,
            });
            (edit.name.clone(), entry)
        })
        .collect()
}

/// Sorted items after sorted changes, each of which sets the item of a name
/// or removes it.
fn merge_items<I: Item>(
    old: impl IntoIterator<Item = I>,
    changes: impl IntoIterator<Item = (Vec<u8>, Option<I>)>,
) -> Vec<I> {
    let mut merged = Vec::new();
    let mut old = old.into_iter().peekable();
    for (name, new) in changes {
        while let Some(item) = old.next_if(|item| item.name() < name.as_slice()) {
            merged.push(item);
        }
        old.next_if(|item| item.name() == name.as_slice());
        merged.extend(new);
    }
    merged.extend(old);
    merged
}

/// The directory of `entries`, sorted and valid, built from scratch to
/// replace the one whose root was `old_root`: every node but that root, the
/// ones that hold entries paired with `similar`.
fn rebuilt(old_root: ObjectId, similar: Option<ObjectId>, entries: Vec<Entry>) -> Updated {
    let count = entries.len() as u64;
    let built = build_nodes(entries);
    let root = built.last().map_or(old_root, |node| node.id);
    let nodes = built
        .into_iter()
        .filter(|node| node.id != old_root)
        .map(|node| NewNode {
            similar: similar.filter(|_| node.holds_entries),
            id: node.id,
            bytes: node.bytes,
        })
        .collect();
    Updated {
        root,
        entries: count,
        nodes,
    }
}

/// The changes to the items of one level: the item each name gets, or its
/// removal, in name order.
type ItemChanges<I> = Vec<(Vec<u8>, Option<I>)>;

/// A split directory being updated.
struct Split<'c, F> {
    cache: &'c mut NodeCache,
    fetch: F,
    /// The reference to its root before the update.
    root: Child,
    /// The level of that root.
    height: u8,
    splitter: Splitter,
    /// The nodes made so far that the directory did not have, children
    /// before parents, with their level and the node each most likely
    /// replaces.
    made: Vec<(u8, Made, Option<ObjectId>)>,
}

impl<F: FnMut(&ObjectId) -> Option<Vec<u8>>> Split<'_, F> {
    fn node(&mut self, id: ObjectId) -> Result<Rc<Node>, TreeError> {
        self.cache.get(id, &mut self.fetch)
    }

    fn update(mut self, edits: &[EntryEdit]) -> Result<Updated, TreeError> {
        let (mut changes, gained) = self.level::<Entry>(0, entry_changes(edits))?;
        let entries = i128::from(self.root.entry_count) + gained;
        if entries <= MAX_FLAT_ENTRIES as i128 {
            // Few enough entries for one flat node: read them all.
            let (old, largest) = self.entries()?;
            return Ok(rebuilt(
                self.root.node,
                largest,
                merge_items(old, entry_changes(edits)),
            ));
        }
        let entries = u64::try_from(entries).map_err(|_| TreeError::NotCanonical)?;
        for level in 1..self.height {
            if changes.is_empty() {
                // No node changed below this level, so none above it does.
                break;
            }
            changes = self.level::<Child>(level, changes)?.0;
        }
        if changes.is_empty() {
            // Edits that set names to the entries they had.
            debug_assert!(self.made.is_empty());
            return Ok(Updated {
                root: self.root.node,
                entries,
                nodes: Vec::new(),
            });
        }

        // The old root is the only node of its level, so the level is cut
        // again whole, and so is every level above it, until one node is left.
        let old_root = self.root.node;
        let node = self.node(old_root)?;
        let children = Child::items(&node).ok_or(TreeError::NotCanonical)?;
        let mut level = self.height;
        let mut chunker = Chunker::new(level);
        for child in merge_items(children.iter().cloned(), changes) {
            chunker.add(child, &mut self.splitter);
        }
        let mut top = self.record(level, chunker.finish(), Some(old_root));
        while top.len() > 1 {
            level = level.checked_add(1).ok_or(TreeError::NotCanonical)?;
            let mut chunker = Chunker::new(level);
            for child in top {
                chunker.add(child, &mut self.splitter);
            }
            top = self.record(level, chunker.finish(), None);
        }
        let mut root = top.pop().ok_or(TreeError::NotCanonical)?;
        // A root with one child is not one: the first level with one node
        // holds the root, so the levels that lost all but one node go.
        while level > 0 {
            let children = self.children_of(&root, level)?;
            if children.len() != 1 {
                break;
            }
            root = children.into_iter().next().ok_or(TreeError::NotCanonical)?;
            level -= 1;
        }
        if level == 0 || root.entry_count != entries {
            return Err(TreeError::NotCanonical);
        }
        let nodes = self
            .made
            .into_iter()
            .filter(|(made_level, ..)| *made_level <= level)
            .map(|(_, made, similar)| NewNode {
                id: made.child.node,
                bytes: made.bytes,
                similar,
            })
            .collect();
        Ok(Updated {
            root: root.node,
            entries,
            nodes,
        })
    }

    /// Keeps the nodes a whole level was cut into, the ones that are new
    /// paired with `similar`, and returns the references to all of them.
    fn record(&mut self, level: u8, made: Vec<Made>, similar: Option<ObjectId>) -> Vec<Child> {
        made.into_iter()
            .map(|made| {
                let child = made.child.clone();
                if Some(child.node) != similar {
                    self.made.push((level, made, similar));
                }
                child
            })
            .collect()
    }

    /// The children of the internal node of `level` that `node` refers to,
    /// made by this update or read from the directory.
    fn children_of(&mut self, node: &Child, level: u8) -> Result<Vec<Child>, TreeError> {
        let found = match self
            .made
            .iter()
            .find(|(_, made, _)| made.child.node == node.node)
        {
            Some((_, made, _)) => Rc::new(Node::decode(&made.bytes)?),
            None => self.node(node.node)?,
        };
        check_child(&found, level, node)?;
        Child::items(&found)
            .map(<[Child]>::to_vec)
            .ok_or(TreeError::NotCanonical)
    }

    /// The node of `level` before the update that holds `name`, or would:
    /// the first whose last name is at or after `name`, or the last node of
    /// the level for a name after every other. With `after`, the first whose
    /// last name is after `name`, if there is one.
    fn locate(&mut self, level: u8, name: &[u8], after: bool) -> Result<Option<Child>, TreeError> {
        if after && name >= self.root.last_name.as_slice() {
            return Ok(None);
        }
        let mut current = self.root.clone();
        let mut at = self.height;
        while at > level {
            let node = self.node(current.node)?;
            check_child(&node, at, &current)?;
            let children = Child::items(&node).ok_or(TreeError::NotCanonical)?;
            let index = children.partition_point(|child| {
                if after {
                    child.last_name.as_slice() <= name
                } else {
                    child.last_name.as_slice() < name
                }
            });
            current = match (children.get(index), after) {
                (Some(child), _) => child.clone(),
                (None, false) => children.last().ok_or(TreeError::NotCanonical)?.clone(),
                // The node above ends after `name`, so one of its children does.
                (None, true) => return Err(TreeError::NotCanonical),
            };
            at -= 1;
        }
        Ok(Some(current))
    }

    /// Applies `changes` to the level `level`, below the root, one run at a
    /// time, and returns the changes this makes to the level above, with
    /// how many entries the directory gained (lost, if negative).
    fn level<I: Item>(
        &mut self,
        level: u8,
        changes: ItemChanges<I>,
    ) -> Result<(ItemChanges<Child>, i128), TreeError> {
        let mut above = Vec::new();
        let mut gained = 0i128;
        let mut changes = changes.into_iter().peekable();
        while let Some((name, _)) = changes.peek() {
            // The run starts at the node that holds the first name left to
            // change, or would: every boundary before it stays.
            let mut old = self
                .locate(level, name, false)?
                .ok_or(TreeError::NotCanonical)?;
            let mut chunker = Chunker::new(level);
            let mut replaced: Vec<Child> = Vec::new();
            loop {
                let node = self.node(old.node)?;
                check_child(&node, level, &old)?;
                let items = I::items(&node).ok_or(TreeError::NotCanonical)?;
                if let (Some(previous), Some(first)) = (replaced.last(), items.first())
                    && first.name() <= previous.last_name.as_slice()
                {
                    return Err(TreeError::NotCanonical);
                }
                // The last node of the level also takes every name after it.
                let last = old.last_name == self.root.last_name;
                let mut index = 0;
                loop {
                    let change = changes
                        .peek()
                        .map(|(name, _)| name.as_slice())
                        .filter(|name| last || *name <= old.last_name.as_slice());
                    let order = match (items.get(index), change) {
                        (None, None) => break,
                        (Some(_), None) => Ordering::Less,
                        (None, Some(_)) => Ordering::Greater,
                        (Some(item), Some(name)) => item.name().cmp(name),
                    };
                    if order == Ordering::Greater {
                        // A name the node does not have.
                        if let Some((name, Some(new))) = changes.next() {
                            gained += i128::from(new.entries());
                            let cut = self.splitter.ends(level, &name, chunker.next_count());
                            chunker.push(new, cut);
                        }
                        continue;
                    }
                    let item = &items[index];
                    index += 1;
                    // The same name gives the same hash: compute it once, for
                    // the old boundary and for the new one.
                    let mut hit = None;
                    let splitter = &mut self.splitter;
                    let mut hit_of =
                        || *hit.get_or_insert_with(|| splitter.hit(level, item.name()));
                    let ended = ends(index, &mut hit_of);
                    let boundary = match (index == items.len(), last) {
                        (false, _) => Some(false),
                        (true, false) => Some(true),
                        // The end of the directory ends the last node anyway.
                        (true, true) => None,
                    };
                    if boundary.is_some_and(|boundary| boundary != ended) {
                        return Err(TreeError::NotCanonical);
                    }
                    let new = match order {
                        Ordering::Equal => {
                            let (_, new) = changes.next().ok_or(TreeError::NotCanonical)?;
                            gained -= i128::from(item.entries());
                            if let Some(new) = &new {
                                gained += i128::from(new.entries());
                            }
                            new
                        }
                        _ => Some(item.clone()),
                    };
                    if let Some(new) = new {
                        let cut = ends(chunker.next_count(), &mut hit_of);
                        chunker.push(new, cut);
                    }
                }
                replaced.push(old);
                if chunker.at_boundary() || last {
                    // Cut where an old node was cut, or at the end of the
                    // level: what follows is unchanged.
                    break;
                }
                let after = replaced
                    .last()
                    .map(|node| node.last_name.clone())
                    .unwrap_or_default();
                old = self
                    .locate(level, &after, true)?
                    .ok_or(TreeError::NotCanonical)?;
            }
            self.settle(level, &replaced, chunker.finish(), &mut above);
        }
        Ok((above, gained))
    }

    /// Keeps the nodes a run of `level` made in place of the `replaced` ones,
    /// each paired with the old node that held its last name or would have,
    /// and adds what this changes to `above`, the level above.
    fn settle(
        &mut self,
        level: u8,
        replaced: &[Child],
        made: Vec<Made>,
        above: &mut ItemChanges<Child>,
    ) {
        let mut old = replaced.iter().peekable();
        for made in made {
            let name = made.child.last_name.as_slice();
            while let Some(gone) = old.next_if(|old| old.last_name.as_slice() < name) {
                above.push((gone.last_name.clone(), None));
            }
            if old
                .next_if(|old| old.last_name.as_slice() == name)
                .is_some_and(|old| *old == made.child)
            {
                // The same node as before.
                continue;
            }
            let similar = replaced
                .iter()
                .find(|old| old.last_name.as_slice() >= name)
                .or(replaced.last())
                .map(|old| old.node);
            above.push((made.child.last_name.clone(), Some(made.child.clone())));
            self.made.push((level, made, similar));
        }
        above.extend(old.map(|gone| (gone.last_name.clone(), None)));
    }

    /// Every entry of the directory before the update, in order, and its
    /// largest leaf.
    fn entries(&mut self) -> Result<(Vec<Entry>, Option<ObjectId>), TreeError> {
        let mut entries: Vec<Entry> = Vec::new();
        let mut largest: Option<(usize, ObjectId)> = None;
        // No name sorts before the empty one.
        let mut next = self.locate(0, b"", false)?;
        while let Some(leaf) = next {
            let node = self.node(leaf.node)?;
            check_child(&node, 0, &leaf)?;
            let found = Entry::items(&node).ok_or(TreeError::NotCanonical)?;
            if let (Some(previous), Some(first)) = (entries.last(), found.first())
                && first.name <= previous.name
            {
                return Err(TreeError::NotCanonical);
            }
            if largest.is_none_or(|(len, _)| found.len() > len) {
                largest = Some((found.len(), leaf.node));
            }
            entries.extend_from_slice(found);
            next = self.locate(0, &leaf.last_name, true)?;
        }
        Ok((entries, largest.map(|(_, id)| id)))
    }
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
