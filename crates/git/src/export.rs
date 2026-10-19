// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Export: braid objects back to git objects, and into a git repository.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use braid_format::{Changeset, Entry, EntryKind, GitId, Kind, ObjectId, Objects};

use crate::error::Error;
use crate::fetch::{self, TreeCache};
use crate::map::{GitMap, Pairings};
use crate::oid::{GitKind, GitOid};
use crate::raw::{self, EntryType, TreeItem};
use crate::repo::GitRepo;

/// Produces git objects from braid objects.
///
/// What came from git goes back byte for byte: a changeset with a git origin
/// is exported as its original commit, after checking that the tree and the
/// parents the commit names are the changeset's (see below); a git tree that
/// is not canonical as its raw bytes; a tag as its raw bytes.
///
/// The tree a commit from git names must be the changeset's root: the one
/// the map links to it, or, when the map does not know that git tree, the
/// canonical tree of the root as computed here. Each parent it names must
/// be the changeset's parent at the same place: as the map links them, or,
/// when the map does not know that commit, the commit computed here for the
/// parent changeset. So history imported from git exports to its original
/// commits through an exporter whose map knows nothing of it, such as a
/// server's, which receives the changesets and their git origins but not the
/// map of the repository that imported them. Any difference is
/// [`Error::OriginMismatch`]. A commit whose tree is not canonical (one that
/// only its raw bytes reproduce, which only the importing repository's map
/// holds) cannot be checked that way, and is an `OriginMismatch` too.
///
/// Everything else is generated deterministically from braid content alone:
/// a tree with canonical modes and git's entry order, a commit with `tree`,
/// `parent`, `author` and `committer` headers, one header per extra, and the
/// message unchanged.
///
/// An exporter caches the git ids it computes, so exporting many related
/// objects through one exporter does each piece of work once. Nothing is
/// recursive: deep histories and deep directories are walked with explicit
/// stacks. What it computed that the map does not know yet is available from
/// [`pairings`](Self::pairings), to record in the map.
pub struct Exporter<'a, O: Objects> {
    objects: &'a O,
    map: &'a GitMap,
    /// Canonical git trees computed here, for braid trees the map does not
    /// link to one, and back.
    trees: HashMap<ObjectId, GitOid>,
    tree_sources: HashMap<GitOid, ObjectId>,
    /// Git blobs computed here, with their sizes, and back.
    blobs: HashMap<ObjectId, (GitOid, u64)>,
    blob_sources: HashMap<GitOid, ObjectId>,
    /// Commits exported here, and back.
    commits: HashMap<ObjectId, GitOid>,
    commit_sources: HashMap<GitOid, ObjectId>,
    /// Directories read back already, shared with whoever made this
    /// exporter, if anyone.
    cache: Option<&'a TreeCache>,
}

impl<O: Objects> fmt::Debug for Exporter<'_, O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Exporter")
            .field("computed_trees", &self.trees.len())
            .field("computed_blobs", &self.blobs.len())
            .field("computed_commits", &self.commits.len())
            .finish_non_exhaustive()
    }
}

impl<'a, O: Objects> Exporter<'a, O> {
    /// An exporter reading braid objects from `objects`, with what `map`
    /// knows about imported git objects.
    pub fn new(objects: &'a O, map: &'a GitMap) -> Self {
        Self {
            objects,
            map,
            trees: HashMap::new(),
            tree_sources: HashMap::new(),
            blobs: HashMap::new(),
            blob_sources: HashMap::new(),
            commits: HashMap::new(),
            commit_sources: HashMap::new(),
            cache: None,
        }
    }

    /// An exporter that reads directories through `cache`, and keeps what it
    /// reads there.
    pub(crate) fn with_cache(objects: &'a O, map: &'a GitMap, cache: &'a TreeCache) -> Self {
        Self {
            cache: Some(cache),
            ..Self::new(objects, map)
        }
    }

    /// Every entry of a braid directory.
    fn entries(&self, tree: ObjectId) -> Result<Arc<[Entry]>, Error> {
        match self.cache {
            Some(cache) => cache.entries(self.objects, tree),
            None => Ok(fetch::tree_entries(self.objects, tree)?.into()),
        }
    }

    /// Every pairing this exporter computed that its map does not hold: the
    /// commits of the changesets it exported, and the canonical trees and
    /// the blobs it generated. [`GitMap::record`] adds them to a map, so that
    /// importing these git objects later gives back the same braid objects.
    pub fn pairings(&self) -> Pairings {
        let mut commits: Vec<(GitOid, ObjectId)> = self
            .commits
            .iter()
            .filter(|(changeset, _)| self.map.commit(changeset).is_none())
            .map(|(changeset, oid)| (*oid, *changeset))
            .collect();
        let mut trees: Vec<(GitOid, ObjectId)> = self
            .trees
            .iter()
            .filter(|(tree, _)| self.map.git_tree(tree).is_none())
            .map(|(tree, oid)| (*oid, *tree))
            .collect();
        let mut blobs: Vec<(GitOid, ObjectId, u64)> = self
            .blobs
            .iter()
            .filter(|(blob, _)| self.map.git_blob(blob).is_none())
            .map(|(blob, (oid, size))| (*oid, *blob, *size))
            .collect();
        commits.sort_unstable();
        trees.sort_unstable();
        blobs.sort_unstable();
        Pairings {
            commits,
            trees,
            blobs,
        }
    }

    /// The git commit of a changeset: its id and its bytes.
    pub fn commit(&mut self, changeset: &ObjectId) -> Result<(GitOid, Vec<u8>), Error> {
        let decoded = fetch::changeset(self.objects, changeset)?;
        self.commit_of(changeset, &decoded)
    }

    /// The git id of a changeset's commit. Ancestors whose commits are not
    /// known yet are exported first, parents before children: a generated
    /// commit names its parents' commits, and a commit from git is checked
    /// against them.
    pub fn commit_id(&mut self, changeset: &ObjectId) -> Result<GitOid, Error> {
        if let Some(oid) = self.known_commit(changeset) {
            return Ok(oid);
        }
        let mut stack = vec![(*changeset, fetch::changeset(self.objects, changeset)?)];
        while let Some((id, decoded)) = stack.pop() {
            if self.known_commit(&id).is_some() {
                continue;
            }
            let waiting = decoded
                .parents
                .iter()
                .find(|parent| self.known_commit(parent).is_none())
                .copied();
            match waiting {
                Some(parent) => {
                    let parent_changeset = fetch::changeset(self.objects, &parent)?;
                    stack.push((id, decoded));
                    stack.push((parent, parent_changeset));
                }
                None => {
                    self.commit_of(&id, &decoded)?;
                }
            }
        }
        self.known_commit(changeset)
            .ok_or_else(|| Error::NotMapped {
                what: "changeset",
                id: changeset.to_hex(),
            })
    }

    fn known_commit(&self, changeset: &ObjectId) -> Option<GitOid> {
        self.map
            .commit(changeset)
            .or_else(|| self.commits.get(changeset).copied())
    }

    fn commit_of(
        &mut self,
        id: &ObjectId,
        changeset: &Changeset,
    ) -> Result<(GitOid, Vec<u8>), Error> {
        let (oid, bytes) = match &changeset.git_origin {
            Some(origin) => self.origin_commit(id, changeset, origin)?,
            None => {
                let mut parents = Vec::with_capacity(changeset.parents.len());
                for parent in &changeset.parents {
                    parents.push(self.commit_id(parent)?);
                }
                let tree = self.tree_id(&changeset.root)?;
                let bytes = raw::encode_commit(
                    tree,
                    &parents,
                    &changeset.author,
                    &changeset.committer,
                    &changeset.extras,
                    &changeset.message,
                )
                .map_err(|detail| Error::Unrepresentable {
                    changeset: *id,
                    detail,
                })?;
                (GitOid::for_object(GitKind::Commit, &bytes)?, bytes)
            }
        };
        self.commits.insert(*id, oid);
        self.commit_sources.insert(oid, *id);
        Ok((oid, bytes))
    }

    /// The original commit of a changeset imported from git, after checking
    /// that what it names agrees with the changeset: with the map where it
    /// knows, and with what this exporter computes where it does not.
    fn origin_commit(
        &mut self,
        id: &ObjectId,
        changeset: &Changeset,
        origin: &ObjectId,
    ) -> Result<(GitOid, Vec<u8>), Error> {
        let mismatch = |detail: String| Error::OriginMismatch {
            changeset: *id,
            detail,
        };
        let bytes = fetch::get(self.objects, origin, Kind::Blob)?;
        let fields = raw::parse_commit(&bytes)
            .map_err(|reason| mismatch(format!("its git origin is not a commit: {reason}")))?;
        match self.map.tree(&fields.tree) {
            Some(root) if root == changeset.root => {}
            Some(root) => {
                return Err(mismatch(format!(
                    "its commit names tree {}, which the map links to {root}, not to its root {}",
                    fields.tree, changeset.root
                )));
            }
            None => {
                let computed = self.tree_id(&changeset.root)?;
                if computed != fields.tree {
                    return Err(mismatch(format!(
                        "its commit names tree {}, but its root {} exports as tree {computed}{}",
                        fields.tree,
                        changeset.root,
                        if self.map.git_tree(&changeset.root).is_some() {
                            ""
                        } else {
                            " (a git tree that is not canonical can be checked only with the map of the repository that imported it)"
                        }
                    )));
                }
            }
        }
        if fields.parents.len() != changeset.parents.len() {
            return Err(mismatch(format!(
                "its commit names {} parents, the changeset has {}",
                fields.parents.len(),
                changeset.parents.len()
            )));
        }
        for (index, (git, braid)) in fields.parents.iter().zip(&changeset.parents).enumerate() {
            match self.map.changeset(git) {
                Some(known) if known == *braid => {}
                Some(known) => {
                    return Err(mismatch(format!(
                        "parent {index} of its commit is {git}, which the map links to {known}, not to {braid}"
                    )));
                }
                None => {
                    let computed = self.commit_id(braid)?;
                    if computed != *git {
                        return Err(mismatch(format!(
                            "parent {index} of its commit is {git}, but its parent {braid} exports as commit {computed}"
                        )));
                    }
                }
            }
        }
        let oid = GitOid::for_object(GitKind::Commit, &bytes)?;
        if let Some(known) = self.map.commit(id)
            && known != oid
        {
            return Err(mismatch(format!(
                "the map links it to commit {known}, but its git origin is commit {oid}"
            )));
        }
        Ok((oid, bytes))
    }

    /// The canonical git tree of a braid tree: its id and its bytes.
    pub fn tree(&mut self, tree: &ObjectId) -> Result<(GitOid, Vec<u8>), Error> {
        let entries = self.entries(*tree)?;
        self.tree_of(tree, &entries)
    }

    /// [`tree`](Self::tree) for the braid tree `tree` whose entries the caller
    /// has already: those it was just built from, which is what reading it
    /// back from the store gives.
    pub(crate) fn tree_of(
        &mut self,
        tree: &ObjectId,
        entries: &[Entry],
    ) -> Result<(GitOid, Vec<u8>), Error> {
        let bytes = self.encode_entries(tree, entries)?;
        let oid = GitOid::for_object(GitKind::Tree, &bytes)?;
        match self.known_tree(tree) {
            Some(known) if known != oid => Err(Error::BadBraidObject {
                id: *tree,
                detail: format!("the map links it to git tree {known}, but it exports as {oid}"),
            }),
            Some(_) => Ok((oid, bytes)),
            None => {
                self.trees.insert(*tree, oid);
                self.tree_sources.insert(oid, *tree);
                Ok((oid, bytes))
            }
        }
    }

    /// The id of the canonical git tree of a braid tree.
    pub fn tree_id(&mut self, tree: &ObjectId) -> Result<GitOid, Error> {
        if let Some(oid) = self.known_tree(tree) {
            return Ok(oid);
        }
        // Directories waiting for their subdirectories: id, entries, and the
        // index of the next entry to look at.
        let mut stack = vec![(*tree, self.entries(*tree)?, 0usize)];
        while let Some((_, entries, next)) = stack.last_mut() {
            let mut child = None;
            while child.is_none() && *next < entries.len() {
                if let EntryKind::Tree { tree } = &entries[*next].kind
                    && self.known_tree(tree).is_none()
                {
                    child = Some(*tree);
                }
                *next += 1;
            }
            if let Some(child) = child {
                let entries = self.entries(child)?;
                stack.push((child, entries, 0));
                continue;
            }
            let (id, entries, _) = stack.pop().expect("the loop runs on a non-empty stack");
            if self.known_tree(&id).is_some() {
                continue;
            }
            let bytes = self.encode_entries(&id, &entries)?;
            let oid = GitOid::for_object(GitKind::Tree, &bytes)?;
            self.trees.insert(id, oid);
            self.tree_sources.insert(oid, id);
        }
        self.known_tree(tree).ok_or_else(|| Error::NotMapped {
            what: "tree",
            id: tree.to_hex(),
        })
    }

    fn known_tree(&self, tree: &ObjectId) -> Option<GitOid> {
        self.map
            .git_tree(tree)
            .or_else(|| self.trees.get(tree).copied())
    }

    /// The canonical bytes of a directory. Its subdirectories normally have
    /// git ids already, so this does not recurse far.
    fn encode_entries(&mut self, tree: &ObjectId, entries: &[Entry]) -> Result<Vec<u8>, Error> {
        let mut items = Vec::with_capacity(entries.len());
        for entry in entries {
            let (kind, oid) = match &entry.kind {
                EntryKind::File { blob, .. } => (EntryType::File, self.blob_id(blob)?),
                EntryKind::Executable { blob, .. } => (EntryType::Executable, self.blob_id(blob)?),
                EntryKind::Symlink { blob, .. } => (EntryType::Symlink, self.blob_id(blob)?),
                EntryKind::Tree { tree } => (EntryType::Tree, self.tree_id(tree)?),
                EntryKind::Gitlink {
                    commit: GitId::Sha1(bytes),
                } => (EntryType::Gitlink, GitOid::from_bytes(*bytes)),
                EntryKind::Gitlink {
                    commit: GitId::Sha256(_),
                } => {
                    return Err(Error::BadBraidObject {
                        id: *tree,
                        detail: "a SHA-256 submodule cannot be written in a SHA-1 tree".into(),
                    });
                }
            };
            items.push(TreeItem {
                kind,
                name: &entry.name,
                oid,
            });
        }
        Ok(raw::encode_tree(&mut items))
    }

    /// The git blob of a braid blob: its id and its content.
    pub fn blob(&mut self, blob: &ObjectId) -> Result<(GitOid, Vec<u8>), Error> {
        let bytes = fetch::get(self.objects, blob, Kind::Blob)?;
        let oid = GitOid::for_object(GitKind::Blob, &bytes)?;
        if let Some(known) = self.map.git_blob(blob)
            && known != oid
        {
            return Err(Error::BadBraidObject {
                id: *blob,
                detail: format!("the map links it to git blob {known}, but its content is {oid}"),
            });
        }
        self.blobs.insert(*blob, (oid, bytes.len() as u64));
        self.blob_sources.insert(oid, *blob);
        Ok((oid, bytes))
    }

    /// The id of the git blob of a braid blob.
    pub fn blob_id(&mut self, blob: &ObjectId) -> Result<GitOid, Error> {
        match self
            .map
            .git_blob(blob)
            .or_else(|| self.blobs.get(blob).map(|(oid, _)| *oid))
        {
            Some(oid) => Ok(oid),
            None => Ok(self.blob(blob)?.0),
        }
    }

    /// Any git object the map or this exporter knows, by its git id: a commit,
    /// a tree (raw or regenerated), a blob or an annotated tag. The bytes are
    /// checked to hash to `oid`.
    pub fn object(&mut self, oid: &GitOid) -> Result<(GitKind, Vec<u8>), Error> {
        let (kind, bytes) = self.object_bytes(oid)?;
        let found = GitOid::for_object(kind, &bytes)?;
        if found != *oid {
            return Err(Error::Verification {
                oid: *oid,
                kind,
                detail: format!("exported as {found}"),
            });
        }
        Ok((kind, bytes))
    }

    fn object_bytes(&mut self, oid: &GitOid) -> Result<(GitKind, Vec<u8>), Error> {
        if let Some(changeset) = self
            .map
            .changeset(oid)
            .or_else(|| self.commit_sources.get(oid).copied())
        {
            return Ok((GitKind::Commit, self.commit(&changeset)?.1));
        }
        if let Some(raw) = self.map.raw_tree(oid) {
            return Ok((GitKind::Tree, fetch::get(self.objects, &raw, Kind::Blob)?));
        }
        if let Some(tree) = self
            .map
            .tree(oid)
            .or_else(|| self.tree_sources.get(oid).copied())
        {
            return Ok((GitKind::Tree, self.tree(&tree)?.1));
        }
        if let Some(blob) = self
            .map
            .blob(oid)
            .map(|(id, _)| id)
            .or_else(|| self.blob_sources.get(oid).copied())
        {
            return Ok((GitKind::Blob, fetch::get(self.objects, &blob, Kind::Blob)?));
        }
        if let Some(entry) = self.map.tag(oid) {
            let bytes = fetch::get(self.objects, &entry.raw, Kind::Blob)?;
            if raw::parse_tag(&bytes).ok() != Some(entry.target) {
                return Err(Error::Verification {
                    oid: *oid,
                    kind: GitKind::Tag,
                    detail: format!("its raw bytes do not point to {}", entry.target),
                });
            }
            return Ok((GitKind::Tag, bytes));
        }
        Err(Error::NotMapped {
            what: "git object",
            id: oid.to_hex(),
        })
    }
}

/// The git commit of a changeset: its id and its bytes. See [`Exporter`].
pub fn export_commit<O: Objects>(
    objects: &O,
    map: &GitMap,
    changeset: &ObjectId,
) -> Result<(GitOid, Vec<u8>), Error> {
    Exporter::new(objects, map).commit(changeset)
}

/// The canonical git tree of a braid tree: its id and its bytes.
pub fn export_tree<O: Objects>(
    objects: &O,
    map: &GitMap,
    tree: &ObjectId,
) -> Result<(GitOid, Vec<u8>), Error> {
    Exporter::new(objects, map).tree(tree)
}

/// The git blob of a braid blob: its id and its content.
pub fn export_blob<O: Objects>(
    objects: &O,
    map: &GitMap,
    blob: &ObjectId,
) -> Result<(GitOid, Vec<u8>), Error> {
    Exporter::new(objects, map).blob(blob)
}

/// An imported git object, by its git id: its kind and its exact bytes.
pub fn export_object<O: Objects>(
    objects: &O,
    map: &GitMap,
    oid: &GitOid,
) -> Result<(GitKind, Vec<u8>), Error> {
    Exporter::new(objects, map).object(oid)
}

/// What a ref written by [`write_to_repo`] points to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefTarget {
    /// A changeset, written as a commit.
    Changeset(ObjectId),
    /// A git object the map knows: a commit, an annotated tag, a tree or a
    /// blob.
    Git(GitOid),
}

/// A ref to set in [`write_to_repo`], which overwrites it without a lease.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RefUpdate {
    /// The full ref name, such as `refs/heads/main`.
    pub name: Vec<u8>,
    /// What it points to.
    pub target: RefTarget,
}

/// What [`write_to_repo`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteReport {
    /// Commits written.
    pub commits: usize,
    /// Trees written.
    pub trees: usize,
    /// Blobs written.
    pub blobs: usize,
    /// Annotated tags written.
    pub tags: usize,
    /// Each ref and the object it points to now.
    pub refs: Vec<(Vec<u8>, GitOid)>,
}

/// Writes into the git repository at `git_repo` every object the refs need,
/// then points the refs at them in one transaction.
///
/// Refs are overwritten, whatever they pointed to before: there is no lease
/// and no fast-forward check here. That is meant for local exports and for a
/// mirror; updating a shared repository with a lease, as a fast-forward, is
/// the job of `braid git sync`, which pushes from such a mirror.
///
/// Objects the repository already has are not written again, and neither is
/// anything they reach. Objects are written after everything they reach, so an
/// interrupted write never leaves an object whose closure is missing.
///
/// The map is not changed: [`write_to_repo_recording`] also records what the
/// export generated.
pub fn write_to_repo<O: Objects>(
    git_repo: &Path,
    objects: &O,
    map: &GitMap,
    refs: &[RefUpdate],
) -> Result<WriteReport, Error> {
    write(git_repo, objects, map, refs).map(|(report, _)| report)
}

/// [`write_to_repo`], then records in `map` every commit, canonical tree and
/// blob the export generated (see [`Exporter::pairings`]), so that importing
/// them again, from this repository or from any other that received them,
/// gives back the same braid objects instead of new ones.
///
/// Nothing is recorded when the write fails. The caller saves the map.
pub fn write_to_repo_recording<O: Objects>(
    git_repo: &Path,
    objects: &O,
    map: &mut GitMap,
    refs: &[RefUpdate],
) -> Result<WriteReport, Error> {
    let (report, pairings) = write(git_repo, objects, map, refs)?;
    map.record(&pairings);
    Ok(report)
}

/// Writes the objects and the refs, and returns what the export generated.
fn write<O: Objects>(
    git_repo: &Path,
    objects: &O,
    map: &GitMap,
    refs: &[RefUpdate],
) -> Result<(WriteReport, Pairings), Error> {
    let mut repo = GitRepo::open(git_repo)?;
    let mut writer = Writer {
        exporter: Exporter::new(objects, map),
        repo: &repo,
        written: HashSet::new(),
        report: WriteReport::default(),
    };
    let mut targets = Vec::with_capacity(refs.len());
    for update in refs {
        let oid = match update.target {
            RefTarget::Changeset(changeset) => writer.exporter.commit_id(&changeset)?,
            RefTarget::Git(oid) => oid,
        };
        writer.write_closure(oid)?;
        targets.push((update.name.clone(), oid));
    }
    let pairings = writer.exporter.pairings();
    let mut report = writer.report;
    repo.set_refs(&targets, "braid: export")?;
    report.refs = targets;
    Ok((report, pairings))
}

struct Writer<'r, 'a, O: Objects> {
    exporter: Exporter<'a, O>,
    repo: &'r GitRepo,
    written: HashSet<GitOid>,
    report: WriteReport,
}

/// An object waiting for what it reaches to be written.
struct Pending {
    oid: GitOid,
    kind: GitKind,
    bytes: Vec<u8>,
    children: Vec<GitOid>,
    next: usize,
}

impl<O: Objects> Writer<'_, '_, O> {
    fn present(&self, oid: &GitOid) -> bool {
        self.written.contains(oid) || self.repo.contains(oid)
    }

    fn pending(&mut self, oid: GitOid) -> Result<Pending, Error> {
        let (kind, bytes) = self.exporter.object(&oid)?;
        let children = reached(oid, kind, &bytes)?;
        Ok(Pending {
            oid,
            kind,
            bytes,
            children,
            next: 0,
        })
    }

    fn write_closure(&mut self, root: GitOid) -> Result<(), Error> {
        if self.present(&root) {
            return Ok(());
        }
        let mut stack = vec![self.pending(root)?];
        while let Some(top) = stack.last_mut() {
            if top.next < top.children.len() {
                let child = top.children[top.next];
                top.next += 1;
                if !self.present(&child) {
                    let pending = self.pending(child)?;
                    stack.push(pending);
                }
                continue;
            }
            let done = stack.pop().expect("the loop runs on a non-empty stack");
            if self.present(&done.oid) {
                continue;
            }
            let stored = self.repo.write(done.kind, &done.bytes)?;
            if stored != done.oid {
                return Err(Error::Verification {
                    oid: done.oid,
                    kind: done.kind,
                    detail: format!("the repository stored it as {stored}"),
                });
            }
            self.written.insert(done.oid);
            match done.kind {
                GitKind::Commit => self.report.commits += 1,
                GitKind::Tree => self.report.trees += 1,
                GitKind::Blob => self.report.blobs += 1,
                GitKind::Tag => self.report.tags += 1,
            }
        }
        Ok(())
    }
}

/// The objects a git object names: a commit's tree and parents, a tree's
/// entries except submodules, a tag's target.
pub(crate) fn reached(oid: GitOid, kind: GitKind, bytes: &[u8]) -> Result<Vec<GitOid>, Error> {
    let malformed = |reason| Error::Malformed { oid, kind, reason };
    Ok(match kind {
        GitKind::Commit => {
            let fields = raw::parse_commit(bytes).map_err(malformed)?;
            let mut out = Vec::with_capacity(1 + fields.parents.len());
            out.push(fields.tree);
            out.extend(fields.parents);
            out
        }
        GitKind::Tree => {
            let mut out = Vec::new();
            for entry in raw::parse_tree(bytes).map_err(malformed)? {
                match EntryType::from_mode(entry.mode) {
                    Some(EntryType::Gitlink) => {}
                    Some(_) => out.push(entry.oid),
                    None => return Err(malformed("an entry has an unsupported mode")),
                }
            }
            out
        }
        GitKind::Tag => vec![raw::parse_tag(bytes).map_err(malformed)?],
        GitKind::Blob => Vec::new(),
    })
}
