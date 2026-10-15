// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The two-way mapping between git objects and braid objects.
//!
//! Every imported git object has a braid counterpart: a commit has a
//! changeset, a tree a braid tree, a blob a braid blob, and an annotated tag
//! the blob holding its raw bytes. The map records both directions, plus the
//! raw bytes of the git trees that cannot be regenerated from their braid
//! tree. It is derived data: importing the same repository again rebuilds it.

use std::collections::{BTreeMap, HashMap};

use braid_format::codec::{Reader, put_bytes, put_id, put_uv};
use braid_format::{DecodeError, ObjectId};

use crate::oid::{GitKind, GitOid};

/// What the map records about an annotated tag object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TagEntry {
    /// The braid blob holding the tag's raw bytes.
    pub raw: ObjectId,
    /// The object the tag points to.
    pub target: GitOid,
    /// The kind of that object.
    pub target_kind: GitKind,
}

/// How many objects of each kind a map holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MapCounts {
    /// Commits.
    pub commits: usize,
    /// Trees, raw or not.
    pub trees: usize,
    /// Trees kept as raw bytes.
    pub raw_trees: usize,
    /// Blobs.
    pub blobs: usize,
    /// Annotated tag objects.
    pub tags: usize,
    /// Tag names.
    pub tag_names: usize,
}

/// The two-way mapping between imported git objects and braid objects.
///
/// - **Commits** map one to one to changesets.
/// - **Trees**: every git tree maps to one braid tree. A git tree is
///   *canonical* when exporting its braid tree gives it back byte for byte; a
///   braid tree has at most one canonical git tree, which is its reverse
///   mapping. A git tree that is not canonical (a mode such as `100664`,
///   unsorted or duplicate entries, or a subtree that is itself not canonical)
///   keeps its raw bytes in a blob, keyed by its git id: several git trees can
///   share one braid tree, so the git id is what tells them apart.
/// - **Blobs** map one to one, with the size of the content.
/// - **Tags**: an annotated tag maps to the blob with its raw bytes, and
///   records its target. Tag names map to the tag object they point to.
///
/// [`to_bytes`](Self::to_bytes) and [`from_bytes`](Self::from_bytes) give a
/// versioned, canonical encoding, so the store can keep the map as a derived
/// record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitMap {
    commits: HashMap<GitOid, ObjectId>,
    changesets: HashMap<ObjectId, GitOid>,
    trees: HashMap<GitOid, ObjectId>,
    canonical_trees: HashMap<ObjectId, GitOid>,
    raw_trees: HashMap<GitOid, ObjectId>,
    blobs: HashMap<GitOid, (ObjectId, u64)>,
    git_blobs: HashMap<ObjectId, GitOid>,
    tags: HashMap<GitOid, TagEntry>,
    tag_blobs: HashMap<ObjectId, GitOid>,
    tag_names: BTreeMap<Vec<u8>, GitOid>,
}

impl GitMap {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// The changeset imported from a commit.
    pub fn changeset(&self, commit: &GitOid) -> Option<ObjectId> {
        self.commits.get(commit).copied()
    }

    /// The commit a changeset was imported from.
    pub fn commit(&self, changeset: &ObjectId) -> Option<GitOid> {
        self.changesets.get(changeset).copied()
    }

    /// The braid tree of a git tree.
    pub fn tree(&self, tree: &GitOid) -> Option<ObjectId> {
        self.trees.get(tree).copied()
    }

    /// The canonical git tree of a braid tree, if one was imported.
    pub fn git_tree(&self, tree: &ObjectId) -> Option<GitOid> {
        self.canonical_trees.get(tree).copied()
    }

    /// The blob holding the raw bytes of a git tree that is not canonical.
    pub fn raw_tree(&self, tree: &GitOid) -> Option<ObjectId> {
        self.raw_trees.get(tree).copied()
    }

    /// The braid blob of a git blob, and its size.
    pub fn blob(&self, blob: &GitOid) -> Option<(ObjectId, u64)> {
        self.blobs.get(blob).copied()
    }

    /// The git blob of a braid blob.
    pub fn git_blob(&self, blob: &ObjectId) -> Option<GitOid> {
        self.git_blobs.get(blob).copied()
    }

    /// What the map knows about an annotated tag object.
    pub fn tag(&self, tag: &GitOid) -> Option<TagEntry> {
        self.tags.get(tag).copied()
    }

    /// The tag object whose raw bytes are in this braid blob.
    pub fn git_tag(&self, raw: &ObjectId) -> Option<GitOid> {
        self.tag_blobs.get(raw).copied()
    }

    /// The annotated tag a tag name points to, by its full ref name such as
    /// `refs/tags/v1.0`, with what the map knows about it.
    pub fn tag_by_name(&self, name: &[u8]) -> Option<(GitOid, TagEntry)> {
        let oid = *self.tag_names.get(name)?;
        Some((oid, self.tags.get(&oid).copied()?))
    }

    /// Every tag name and the annotated tag it points to, in name order.
    pub fn tag_names(&self) -> impl Iterator<Item = (&[u8], GitOid)> + '_ {
        self.tag_names
            .iter()
            .map(|(name, oid)| (name.as_slice(), *oid))
    }

    /// Every commit and its changeset, in no particular order.
    pub fn commits(&self) -> impl Iterator<Item = (GitOid, ObjectId)> + '_ {
        self.commits.iter().map(|(oid, id)| (*oid, *id))
    }

    /// How many objects of each kind the map holds.
    pub fn counts(&self) -> MapCounts {
        MapCounts {
            commits: self.commits.len(),
            trees: self.trees.len(),
            raw_trees: self.raw_trees.len(),
            blobs: self.blobs.len(),
            tags: self.tags.len(),
            tag_names: self.tag_names.len(),
        }
    }

    pub(crate) fn insert_commit(&mut self, commit: GitOid, changeset: ObjectId) {
        self.commits.insert(commit, changeset);
        self.changesets.insert(changeset, commit);
    }

    /// Records a git tree. `raw` is the blob with its bytes when it is not
    /// canonical.
    pub(crate) fn insert_tree(&mut self, tree: GitOid, braid: ObjectId, raw: Option<ObjectId>) {
        self.trees.insert(tree, braid);
        match raw {
            Some(blob) => {
                self.raw_trees.insert(tree, blob);
            }
            None => {
                self.canonical_trees.entry(braid).or_insert(tree);
            }
        }
    }

    pub(crate) fn insert_blob(&mut self, blob: GitOid, braid: ObjectId, size: u64) {
        self.blobs.insert(blob, (braid, size));
        self.git_blobs.insert(braid, blob);
    }

    pub(crate) fn insert_tag(&mut self, tag: GitOid, entry: TagEntry) {
        self.tags.insert(tag, entry);
        self.tag_blobs.insert(entry.raw, tag);
    }

    pub(crate) fn set_tag_name(&mut self, name: Vec<u8>, tag: GitOid) {
        self.tag_names.insert(name, tag);
    }

    pub(crate) fn remove_tag_name(&mut self, name: &[u8]) {
        self.tag_names.remove(name);
    }

    /// The canonical encoding: a magic string and a version, then each table
    /// sorted by key.
    ///
    /// ```text
    /// "braid-git-map" u8 version = 1
    /// uv n, n × ([20] commit, [32] changeset)
    /// uv n, n × ([20] tree, [32] braid tree)
    /// uv n, n × ([20] tree, [32] blob with its raw bytes)
    /// uv n, n × ([20] blob, [32] braid blob, uv size)
    /// uv n, n × ([20] tag, [32] blob with its raw bytes, [20] target, u8 target kind)
    /// uv n, n × (bytes name, [20] tag)
    /// ```
    ///
    /// Target kinds are 1 commit, 2 tree, 3 blob, 4 tag.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        put_table(&mut out, &self.commits, put_id);
        put_table(&mut out, &self.trees, put_id);
        put_table(&mut out, &self.raw_trees, put_id);
        put_table(&mut out, &self.blobs, |out, (id, size)| {
            put_id(out, id);
            put_uv(out, *size);
        });
        put_table(&mut out, &self.tags, |out, entry| {
            put_id(out, &entry.raw);
            out.extend_from_slice(entry.target.as_bytes());
            out.push(entry.target_kind.code());
        });
        put_uv(&mut out, self.tag_names.len() as u64);
        for (name, oid) in &self.tag_names {
            put_bytes(&mut out, name);
            out.extend_from_slice(oid.as_bytes());
        }
        out
    }

    /// Decodes [`to_bytes`](Self::to_bytes), checking that the encoding is
    /// canonical and that the tables agree with each other.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(bytes);
        let at = reader.position();
        if reader.take(MAGIC.len())? != MAGIC {
            return Err(invalid(at, "git map magic"));
        }
        let at = reader.position();
        if reader.u8()? != VERSION {
            return Err(invalid(at, "git map version"));
        }
        let mut map = GitMap::new();
        for (oid, id) in read_table(&mut reader, 32, |r| r.id())? {
            if map.changesets.insert(id, oid).is_some() {
                return Err(invalid(reader.position(), "changeset mapped twice"));
            }
            map.commits.insert(oid, id);
        }
        map.trees = read_table(&mut reader, 32, |r| r.id())?
            .into_iter()
            .collect();
        for (oid, blob) in read_table(&mut reader, 32, |r| r.id())? {
            if !map.trees.contains_key(&oid) {
                return Err(invalid(reader.position(), "raw tree that is not a tree"));
            }
            map.raw_trees.insert(oid, blob);
        }
        for (oid, braid) in &map.trees {
            if !map.raw_trees.contains_key(oid)
                && map.canonical_trees.insert(*braid, *oid).is_some()
            {
                return Err(invalid(
                    reader.position(),
                    "two canonical trees for one tree",
                ));
            }
        }
        for (oid, (id, size)) in read_table(&mut reader, 33, |r| Ok((r.id()?, r.uv()?)))? {
            if map.git_blobs.insert(id, oid).is_some() {
                return Err(invalid(reader.position(), "blob mapped twice"));
            }
            map.blobs.insert(oid, (id, size));
        }
        let tags = read_table(&mut reader, 53, |r| {
            let raw = r.id()?;
            let target = GitOid::from_bytes(r.array()?);
            let at = r.position();
            let target_kind = GitKind::from_code(r.u8()?).ok_or(invalid(at, "tag target kind"))?;
            Ok(TagEntry {
                raw,
                target,
                target_kind,
            })
        })?;
        for (oid, entry) in tags {
            if map.tag_blobs.insert(entry.raw, oid).is_some() {
                return Err(invalid(reader.position(), "tag mapped twice"));
            }
            map.tags.insert(oid, entry);
        }
        let count = reader.count(1 + GitOid::LEN)?;
        let mut previous: Option<Vec<u8>> = None;
        for _ in 0..count {
            let at = reader.position();
            let name = reader.bytes()?.to_vec();
            let oid = GitOid::from_bytes(reader.array()?);
            if previous.as_ref().is_some_and(|p| *p >= name) {
                return Err(invalid(at, "tag names out of order"));
            }
            if !map.tags.contains_key(&oid) {
                return Err(invalid(at, "tag name for an unknown tag"));
            }
            previous = Some(name.clone());
            map.tag_names.insert(name, oid);
        }
        reader.finish()?;
        Ok(map)
    }
}

const MAGIC: &[u8] = b"braid-git-map";
const VERSION: u8 = 1;

fn invalid(at: usize, what: &'static str) -> DecodeError {
    DecodeError::Invalid { at, what }
}

fn put_table<V>(
    out: &mut Vec<u8>,
    table: &HashMap<GitOid, V>,
    mut put_value: impl FnMut(&mut Vec<u8>, &V),
) {
    let mut keys: Vec<&GitOid> = table.keys().collect();
    keys.sort_unstable();
    put_uv(out, keys.len() as u64);
    for key in keys {
        out.extend_from_slice(key.as_bytes());
        put_value(out, &table[key]);
    }
}

/// Reads a table written by [`put_table`]: keys strictly ascending, each
/// value at least `min_value_bytes` long.
fn read_table<V>(
    reader: &mut Reader<'_>,
    min_value_bytes: usize,
    mut read_value: impl FnMut(&mut Reader<'_>) -> Result<V, DecodeError>,
) -> Result<Vec<(GitOid, V)>, DecodeError> {
    let count = reader.count(GitOid::LEN + min_value_bytes)?;
    let mut rows: Vec<(GitOid, V)> = Vec::with_capacity(count);
    for _ in 0..count {
        let at = reader.position();
        let key = GitOid::from_bytes(reader.array()?);
        if rows.last().is_some_and(|(previous, _)| *previous >= key) {
            return Err(invalid(at, "git map keys out of order"));
        }
        let value = read_value(reader)?;
        rows.push((key, value));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::Kind;

    fn oid(byte: u8) -> GitOid {
        GitOid::from_bytes([byte; 20])
    }

    fn id(text: &str) -> ObjectId {
        ObjectId::hash(Kind::Blob, text.as_bytes())
    }

    fn sample() -> GitMap {
        let mut map = GitMap::new();
        map.insert_commit(oid(1), id("c1"));
        map.insert_commit(oid(2), id("c2"));
        map.insert_tree(oid(3), id("t1"), None);
        map.insert_tree(oid(4), id("t1"), Some(id("raw t1")));
        map.insert_tree(oid(5), id("t2"), None);
        map.insert_blob(oid(6), id("b1"), 42);
        map.insert_tag(
            oid(7),
            TagEntry {
                raw: id("tag"),
                target: oid(1),
                target_kind: GitKind::Commit,
            },
        );
        map.set_tag_name(b"refs/tags/v1".to_vec(), oid(7));
        map
    }

    #[test]
    fn lookups_both_ways() {
        let map = sample();
        assert_eq!(map.changeset(&oid(1)), Some(id("c1")));
        assert_eq!(map.commit(&id("c2")), Some(oid(2)));
        assert_eq!(map.tree(&oid(4)), Some(id("t1")));
        assert_eq!(map.git_tree(&id("t1")), Some(oid(3)));
        assert_eq!(map.raw_tree(&oid(4)), Some(id("raw t1")));
        assert_eq!(map.raw_tree(&oid(3)), None);
        assert_eq!(map.blob(&oid(6)), Some((id("b1"), 42)));
        assert_eq!(map.git_blob(&id("b1")), Some(oid(6)));
        assert_eq!(map.git_tag(&id("tag")), Some(oid(7)));
        let (tag, entry) = map.tag_by_name(b"refs/tags/v1").unwrap();
        assert_eq!(tag, oid(7));
        assert_eq!(entry.target, oid(1));
        assert_eq!(
            map.counts(),
            MapCounts {
                commits: 2,
                trees: 3,
                raw_trees: 1,
                blobs: 1,
                tags: 1,
                tag_names: 1,
            }
        );
    }

    #[test]
    fn bytes_round_trip() {
        let map = sample();
        let bytes = map.to_bytes();
        let decoded = GitMap::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, map);
        assert_eq!(decoded.to_bytes(), bytes);
        assert_eq!(
            GitMap::from_bytes(&GitMap::new().to_bytes()).unwrap(),
            GitMap::new()
        );
    }

    #[test]
    fn rejects_bad_encodings() {
        let bytes = sample().to_bytes();
        for cut in 0..bytes.len() {
            assert!(GitMap::from_bytes(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(GitMap::from_bytes(&trailing).is_err());
        let mut version = bytes.clone();
        version[MAGIC.len()] = 2;
        assert!(GitMap::from_bytes(&version).is_err());

        // Two commits for one changeset.
        let mut map = GitMap::new();
        map.commits.insert(oid(1), id("c"));
        map.commits.insert(oid(2), id("c"));
        assert!(GitMap::from_bytes(&map.to_bytes()).is_err());

        // Two canonical trees for one braid tree.
        let mut map = GitMap::new();
        map.trees.insert(oid(1), id("t"));
        map.trees.insert(oid(2), id("t"));
        assert!(GitMap::from_bytes(&map.to_bytes()).is_err());

        // A raw tree that is not a tree, and a name for an unknown tag.
        let mut map = GitMap::new();
        map.raw_trees.insert(oid(1), id("raw"));
        assert!(GitMap::from_bytes(&map.to_bytes()).is_err());
        let mut map = GitMap::new();
        map.tag_names.insert(b"refs/tags/x".to_vec(), oid(9));
        assert!(GitMap::from_bytes(&map.to_bytes()).is_err());

        // Keys out of order: swap the two commit rows.
        let map = sample();
        let mut swapped = map.to_bytes();
        let start = MAGIC.len() + 1 + 1;
        let row = 20 + 32;
        let (first, second) = swapped[start..start + 2 * row].split_at_mut(row);
        first.swap_with_slice(second);
        assert!(GitMap::from_bytes(&swapped).is_err());
    }
}
