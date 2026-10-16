// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Import: git history into braid objects.
//!
//! [`import`] does everything on the calling thread. [`import_parallel`]
//! makes exactly the same objects, the same map and the same report, gives
//! the store the same hints and makes the same checks, with the reading,
//! storing and checking spread over threads: see its documentation for how.

use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use braid_format::name::forbidden_as_symlink;
use braid_format::{
    ChangesetParts, Directory, Entry, EntryKind, GitId, Kind, ObjectId, Objects, Signature,
    build_directory, validate_name,
};
use rayon::prelude::*;

use crate::diff;
use crate::error::Error;
use crate::export::Exporter;
use crate::fetch::{self, TreeCache};
use crate::map::{Added, GitMap, TagEntry};
use crate::oid::{GitKind, GitOid};
use crate::raw::{self, EntryType};
use crate::repo::GitRepo;

/// Commits per batch of a parallel import.
const BATCH_COMMITS: usize = 64;

/// How many bytes of blob content the reading ahead of one batch keeps until
/// the blobs are stored. Past that, a blob read ahead keeps only its id and
/// size, and is read again when it is stored.
const KEPT_BYTES: usize = 128 << 20;

/// Blobs of this size or larger are never kept between reading and storing:
/// they are read again when they are stored.
const LARGE_BLOB: usize = 4 << 20;

/// What to import.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// The refs to import, by full name such as `refs/heads/main` (a short
    /// name is looked up the way git does). Empty means every branch and
    /// every tag.
    pub refs: Vec<Vec<u8>>,
}

/// A ref that was imported.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ImportedRef {
    /// Its full name.
    pub name: Vec<u8>,
    /// The git object it points to.
    pub oid: GitOid,
    /// The kind of that object.
    pub kind: GitKind,
    /// The braid object for it: the changeset of a commit, the blob with the
    /// raw bytes of an annotated tag, the braid tree of a tree, the braid blob
    /// of a blob.
    pub braid: ObjectId,
}

/// What an import did. Counts are of objects new to the map.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Commits imported.
    pub commits: usize,
    /// Git trees converted.
    pub trees: usize,
    /// Of those trees, the ones kept as raw bytes because they are not
    /// canonical.
    pub raw_trees: usize,
    /// Blobs imported.
    pub blobs: usize,
    /// Annotated tags imported.
    pub tags: usize,
    /// Every ref that was asked for, sorted by name, new or not.
    pub refs: Vec<ImportedRef>,
}

/// Imports the history of the git repository at `git_repo` into `objects`,
/// recording every mapping in `map`.
///
/// Every commit reachable from the refs is imported, parents before
/// children; commits already in `map` are skipped together with their
/// history, so an import can be repeated to bring in new commits, or resumed
/// after a failure. For each commit:
///
/// - its tree is converted, each git tree once: blobs keep their exact bytes,
///   modes become file, executable, symlink, directory or submodule entries,
///   and a git tree that the export cannot regenerate exactly keeps its raw
///   bytes. Braid trees never hold an empty subdirectory, so empty
///   directories, and directories holding only empty ones, are left out; a
///   git tree that loses entries this way keeps its raw bytes too;
/// - its raw bytes are stored and become the changeset's git origin, with the
///   change list against its first parent, the parents, and the author,
///   committer and message read from it;
/// - it is exported again from what was stored, and the import stops with an
///   error naming the commit unless the id is the same.
///
/// Annotated tags keep their raw bytes too, and are checked the same way.
///
/// Shallow clones and SHA-256 repositories are refused. Replacement objects
/// and grafts are ignored, and objects are read raw: no filters, no
/// line-ending conversion, no LFS. A tree holding a name that braid refuses,
/// such as an alias of `.git`, stops the import with an error naming the
/// commit and the path.
///
/// Everything happens on the calling thread; [`import_parallel`] does the
/// same work on several.
pub fn import<O: Objects>(
    git_repo: &Path,
    objects: &O,
    map: &mut GitMap,
    options: &ImportOptions,
) -> Result<ImportReport, Error> {
    let (mut importer, refs, plan) = Importer::start(git_repo, objects, map, options)?;
    for commit in importer.commit_order(&plan.tips)? {
        importer.import_commit(commit)?;
    }
    importer.import_loose(&plan)?;
    importer.import_tags(&plan)?;
    importer.finish(refs)
}

/// [`import`], on `threads` threads, or one per core when `threads` is 0.
///
/// The result is the same as [`import`]'s: the same objects, the same map,
/// the same report, and the same hints to the store, each given while the
/// object it names is stored, or not, as it was then. Commits are still
/// converted and mapped one at a time, parents before children, and every
/// check [`import`] makes by reading back what it stored is made too; what
/// changes is who does the work, and when:
///
/// - Commits go in batches. Before a batch is converted, its commits, the
///   git trees it will convert and the blobs it will import are read from
///   git on every thread, and each blob's braid id is computed there.
/// - Converting a batch makes every object and maps it, but stores nothing:
///   each object to store and each check to make is queued, in the order
///   [`import`] would have stored and checked them. Whether a git tree is
///   canonical is decided from the directory as converted, which is what
///   reading it back gives.
/// - A batch's objects are stored on every thread while the next batch is
///   read and converted. An object whose hint names another object of its
///   batch is stored after it, and an object queued twice is stored once.
///   Then the batch's checks run on every thread, each reading back from
///   the store what it checks: every git tree converted, from its braid
///   tree or its raw bytes, and every commit, exported again.
/// - The first failure, in that order, is the error, as with [`import`];
///   what the import added to the map after it is taken back, so that the
///   map is the one [`import`] would have left, and never names an object
///   that was not stored.
///
/// When the threads cannot be started, this is [`import`].
pub fn import_parallel<O: Objects + Sync>(
    git_repo: &Path,
    objects: &O,
    map: &mut GitMap,
    options: &ImportOptions,
    threads: usize,
) -> Result<ImportReport, Error> {
    let Ok(pool) = rayon::ThreadPoolBuilder::new().num_threads(threads).build() else {
        return import(git_repo, objects, map, options);
    };
    pool.install(|| {
        let (mut importer, refs, plan) = Importer::start(git_repo, objects, map, options)?;
        let order = importer.commit_order(&plan.tips)?;
        let handles = Handles::new(&importer.repo);
        importer.import_in_batches(&order, &plan, &handles)?;
        importer.import_tags(&plan)?;
        importer.finish(refs)
    })
}

/// The refs to import, each with the object it points to.
type Refs = Vec<(Vec<u8>, GitOid)>;

#[derive(Default)]
struct Plan {
    /// Commits to walk from.
    tips: Vec<GitOid>,
    /// Trees and blobs that refs or tags point to.
    loose: Vec<(GitOid, GitKind)>,
    /// Annotated tags to import, each after the tags it points to.
    tags: Vec<GitOid>,
    planned_tags: HashSet<GitOid>,
}

struct Importer<'o, 'm, O: Objects> {
    repo: GitRepo,
    objects: &'o O,
    map: &'m mut GitMap,
    report: ImportReport,
    /// Root trees and git origins of the changesets seen in this import.
    roots: HashMap<ObjectId, (ObjectId, Option<ObjectId>)>,
    /// The braid id of the empty directory.
    empty_tree: ObjectId,
    /// Directories read back from `objects`, and in a parallel import those
    /// converted and not stored yet.
    trees: TreeCache,
    /// In a parallel import, what was read ahead for the batch being
    /// converted, and what converting it queued to store and check.
    deferred: Option<Deferred>,
}

/// The state of a parallel import between its batches.
#[derive(Default)]
struct Deferred {
    /// What was read ahead for the batch being converted.
    ahead: ReadAhead,
    /// What converting the batch did that is not finished yet.
    batch: Batch,
    /// The place of the last put, check or addition queued, in the order a
    /// one-thread import would have made them.
    last: u64,
}

impl Deferred {
    fn next(&mut self) -> u64 {
        self.last += 1;
        self.last
    }

    /// Queues an object to store, unless the batch stores it already: storing
    /// an object that is there does nothing.
    fn put(&mut self, kind: Kind, id: ObjectId, data: Data, similar: Option<ObjectId>) {
        if self.batch.queued.insert(id) {
            let seq = self.next();
            self.batch.puts.push(Put {
                seq,
                kind,
                id,
                data,
                similar,
            });
        }
    }

    fn check(&mut self, check: Check) {
        let seq = self.next();
        self.batch.checks.push((seq, check));
    }

    fn added(&mut self, added: Added) {
        let seq = self.next();
        self.batch.added.push((seq, added));
    }
}

/// Objects read from git ahead of converting a batch, by id. Each was read
/// with [`GitRepo::read_kind`] for the kind it is listed under, so taking it
/// from here is the same as reading it then.
#[derive(Default)]
struct ReadAhead {
    commits: HashMap<GitOid, Vec<u8>>,
    trees: HashMap<GitOid, Vec<u8>>,
    blobs: HashMap<GitOid, ReadBlob>,
}

/// A blob read from git: its braid id and size, and its content unless it was
/// let go to save memory.
struct ReadBlob {
    id: ObjectId,
    size: u64,
    content: Option<Vec<u8>>,
}

impl ReadBlob {
    fn new(content: Vec<u8>, keep: bool) -> Self {
        Self {
            id: ObjectId::hash(Kind::Blob, &content),
            size: content.len() as u64,
            content: keep.then_some(content),
        }
    }
}

/// What converting a batch did that is not finished: the objects to store,
/// the checks to make, and what it added to the map, each with its place in
/// the order a one-thread import would have made them.
#[derive(Default)]
struct Batch {
    puts: Vec<Put>,
    /// The ids of `puts`.
    queued: HashSet<ObjectId>,
    checks: Vec<(u64, Check)>,
    added: Vec<(u64, Added)>,
    /// The directories it converted, held by the tree cache until stored.
    held: Vec<ObjectId>,
}

/// An object to store, with the hint [`import`] would have given the store.
struct Put {
    seq: u64,
    kind: Kind,
    id: ObjectId,
    data: Data,
    similar: Option<ObjectId>,
}

/// What an object to store holds.
enum Data {
    /// Its payload.
    Bytes(Vec<u8>),
    /// A blob let go to save memory, to read from git again.
    Blob(GitOid),
}

/// A check that reads back what was stored, as [`import`] does right after
/// storing it.
enum Check {
    /// The git tree with bytes `raw` is canonical: the braid tree `braid`,
    /// read back, must export as exactly these bytes.
    Canonical { braid: ObjectId, raw: Vec<u8> },
    /// The git tree `oid` keeps its raw bytes in `blob`: they must read back.
    Raw { oid: GitOid, blob: ObjectId },
    /// The commit `oid`: its changeset, exported again, must give it back,
    /// and its tree must export.
    Commit {
        oid: GitOid,
        changeset: ObjectId,
        tree: GitOid,
    },
}

/// A failure of a parallel import, with its place in the order of a
/// one-thread import.
type Failure = (u64, Error);

/// A git tree being converted, waiting for its subtrees.
struct Frame {
    oid: GitOid,
    raw: Vec<u8>,
    /// Path from the root of the commit's tree; empty for the root.
    path: Vec<u8>,
    /// The entries as written, with their types.
    entries: Vec<(EntryType, Vec<u8>, GitOid)>,
    next: usize,
    /// Braid entries for the entries converted so far, in the same order.
    done: Vec<Entry>,
    /// Whether every subtree so far is canonical.
    canonical_subtrees: bool,
    /// Whether an empty directory was left out.
    dropped_empty: bool,
    /// The same path in the first parent, as hints for the store.
    similar: Similar,
}

/// The braid tree at a path in the first parent of the commit being
/// imported: what its new trees and blobs are probably close to.
struct Similar {
    tree: Option<ObjectId>,
    /// Its entries, read on first use.
    entries: Option<Arc<[Entry]>>,
}

impl Similar {
    fn new(tree: Option<ObjectId>) -> Self {
        Self {
            tree,
            entries: None,
        }
    }

    /// The entry named `name`, reading the directory the first time.
    fn entry<O: Objects>(
        &mut self,
        objects: &O,
        trees: &TreeCache,
        name: &[u8],
    ) -> Result<Option<EntryKind>, Error> {
        let entries = self.entries(objects, trees)?;
        Ok(entries
            .binary_search_by(|entry| entry.name.as_slice().cmp(name))
            .ok()
            .map(|at| entries[at].kind.clone()))
    }

    fn entries<O: Objects>(&mut self, objects: &O, trees: &TreeCache) -> Result<&[Entry], Error> {
        if self.entries.is_none() {
            self.entries = Some(match self.tree {
                Some(tree) => trees.entries(objects, tree)?,
                None => Vec::new().into(),
            });
        }
        Ok(self.entries.as_deref().unwrap_or_default())
    }

    /// The tree at `name` below this one.
    fn subtree<O: Objects>(
        &mut self,
        objects: &O,
        trees: &TreeCache,
        name: &[u8],
    ) -> Result<Similar, Error> {
        Ok(Similar::new(match self.entry(objects, trees, name)? {
            Some(EntryKind::Tree { tree }) => Some(tree),
            _ => None,
        }))
    }

    /// The blob of the file, executable or symlink at `name`.
    fn blob<O: Objects>(
        &mut self,
        objects: &O,
        trees: &TreeCache,
        name: &[u8],
    ) -> Result<Option<ObjectId>, Error> {
        Ok(match self.entry(objects, trees, name)? {
            Some(
                EntryKind::File { blob, .. }
                | EntryKind::Executable { blob, .. }
                | EntryKind::Symlink { blob, .. },
            ) => Some(blob),
            _ => None,
        })
    }

    /// For each node of the new directory `directory`, the node of this one
    /// it most likely replaces. A split directory with as many nodes as
    /// before pairs them in order, since boundaries depend only on names;
    /// otherwise only the root is paired, with the old root.
    fn nodes<O: Objects>(
        &mut self,
        objects: &O,
        trees: &TreeCache,
        directory: &Directory,
    ) -> Result<Vec<Option<ObjectId>>, Error> {
        let count = directory.nodes.len();
        let mut similar = vec![None; count];
        let Some(tree) = self.tree else {
            return Ok(similar);
        };
        if count > 1
            && let Ok(old) = build_directory(self.entries(objects, trees)?.to_vec())
            && old.nodes.len() == count
        {
            return Ok(old.nodes.iter().map(|(id, _)| Some(*id)).collect());
        }
        if let Some(root) = similar.last_mut() {
            *root = Some(tree);
        }
        Ok(similar)
    }
}

impl<'o, 'm, O: Objects> Importer<'o, 'm, O> {
    /// Opens the repository, reads the refs, and plans what they point to:
    /// commits to walk from, trees and blobs that tags point to directly,
    /// and annotated tags, each after its target.
    fn start(
        git_repo: &Path,
        objects: &'o O,
        map: &'m mut GitMap,
        options: &ImportOptions,
    ) -> Result<(Self, Refs, Plan), Error> {
        let repo = GitRepo::open(git_repo)?;
        if repo.is_shallow()? {
            return Err(Error::Shallow);
        }
        let refs = repo.refs(&options.refs)?;
        let mut importer = Importer {
            repo,
            objects,
            map,
            report: ImportReport::default(),
            roots: HashMap::new(),
            empty_tree: build_directory(Vec::new())
                .expect("the empty directory is valid")
                .root(),
            trees: TreeCache::default(),
            deferred: None,
        };
        let mut plan = Plan::default();
        for (_, oid) in &refs {
            importer.plan_ref(*oid, &mut plan)?;
        }
        Ok((importer, refs, plan))
    }

    /// Imports the trees and blobs that refs or tags point to directly.
    fn import_loose(&mut self, plan: &Plan) -> Result<(), Error> {
        for (oid, kind) in &plan.loose {
            match kind {
                GitKind::Tree => {
                    self.convert_tree(*oid, *oid, None)?;
                }
                _ => {
                    self.import_blob(*oid, None)?;
                }
            }
        }
        Ok(())
    }

    fn import_tags(&mut self, plan: &Plan) -> Result<(), Error> {
        for tag in &plan.tags {
            self.import_tag(*tag)?;
        }
        Ok(())
    }

    /// The report, with every ref that was asked for, after recording the
    /// names of annotated tags.
    fn finish(mut self, refs: Refs) -> Result<ImportReport, Error> {
        for (name, oid) in refs {
            let kind = self.repo.kind(&oid)?;
            let map = &mut *self.map;
            let braid = match kind {
                GitKind::Commit => map.changeset(&oid),
                GitKind::Tag => map.tag(&oid).map(|entry| entry.raw),
                GitKind::Tree => map.tree(&oid),
                GitKind::Blob => map.blob(&oid).map(|(id, _)| id),
            }
            .ok_or_else(|| Error::NotMapped {
                what: kind.name(),
                id: oid.to_hex(),
            })?;
            if name.starts_with(b"refs/tags/") {
                match kind {
                    GitKind::Tag => map.set_tag_name(name.clone(), oid),
                    // The name may have pointed to an annotated tag before.
                    _ => map.remove_tag_name(&name),
                }
            }
            self.report.refs.push(ImportedRef {
                name,
                oid,
                kind,
                braid,
            });
        }
        Ok(self.report)
    }

    /// Follows a ref through annotated tags to a commit, a tree or a blob.
    fn plan_ref(&mut self, oid: GitOid, plan: &mut Plan) -> Result<(), Error> {
        let mut chain = Vec::new();
        let mut current = oid;
        loop {
            match self.repo.kind(&current)? {
                GitKind::Commit => {
                    plan.tips.push(current);
                    break;
                }
                kind @ (GitKind::Tree | GitKind::Blob) => {
                    plan.loose.push((current, kind));
                    break;
                }
                GitKind::Tag => {
                    // A tag already imported or planned has its target taken
                    // care of.
                    if self.map.tag(&current).is_some()
                        || plan.planned_tags.contains(&current)
                        || chain.contains(&current)
                    {
                        break;
                    }
                    chain.push(current);
                    let bytes = self.repo.read_kind(&current, GitKind::Tag)?;
                    current = raw::parse_tag(&bytes).map_err(|reason| Error::Malformed {
                        oid: current,
                        kind: GitKind::Tag,
                        reason,
                    })?;
                }
            }
        }
        for tag in chain.into_iter().rev() {
            plan.planned_tags.insert(tag);
            plan.tags.push(tag);
        }
        Ok(())
    }

    /// Every commit reachable from `tips` and not yet in the map, parents
    /// before children. The walk is depth first along first parents, without
    /// recursion.
    fn commit_order(&self, tips: &[GitOid]) -> Result<Vec<GitOid>, Error> {
        let mut order = Vec::new();
        let mut seen = HashSet::new();
        for tip in tips {
            if self.map.changeset(tip).is_some() || !seen.insert(*tip) {
                continue;
            }
            let mut stack = vec![(*tip, self.parents(tip)?, 0usize)];
            while let Some((oid, parents, next)) = stack.last_mut() {
                if let Some(parent) = parents.get(*next).copied() {
                    *next += 1;
                    if self.map.changeset(&parent).is_none() && seen.insert(parent) {
                        let grandparents = self.parents(&parent)?;
                        stack.push((parent, grandparents, 0));
                    }
                    continue;
                }
                order.push(*oid);
                stack.pop();
            }
        }
        Ok(order)
    }

    fn parents(&self, commit: &GitOid) -> Result<Vec<GitOid>, Error> {
        let bytes = self.repo.read_kind(commit, GitKind::Commit)?;
        let fields = raw::parse_commit(&bytes).map_err(|reason| Error::Malformed {
            oid: *commit,
            kind: GitKind::Commit,
            reason,
        })?;
        Ok(fields.parents)
    }

    /// Reads a git object of `kind`, taking it from what was read ahead if it
    /// is there.
    fn read(&mut self, oid: &GitOid, kind: GitKind) -> Result<Vec<u8>, Error> {
        if let Some(deferred) = &mut self.deferred {
            let ahead = match kind {
                GitKind::Commit => deferred.ahead.commits.remove(oid),
                GitKind::Tree => deferred.ahead.trees.remove(oid),
                GitKind::Blob | GitKind::Tag => None,
            };
            if let Some(bytes) = ahead {
                return Ok(bytes);
            }
        }
        self.repo.read_kind(oid, kind)
    }

    /// Keeps track, in a parallel import, of what an insert added to the
    /// map.
    fn added(&mut self, added: Added) {
        if let Some(deferred) = &mut self.deferred {
            deferred.added(added);
        }
    }

    /// Stores an object, or in a parallel import queues it with its batch.
    /// `id` is `ObjectId::hash(kind, payload)`.
    fn put(
        &mut self,
        kind: Kind,
        payload: &[u8],
        similar: Option<ObjectId>,
        id: ObjectId,
    ) -> Result<(), Error> {
        match &mut self.deferred {
            None => {
                fetch::put_known(self.objects, kind, payload, similar.as_ref(), id)?;
            }
            Some(deferred) => deferred.put(kind, id, Data::Bytes(payload.to_vec()), similar),
        }
        Ok(())
    }

    fn import_commit(&mut self, oid: GitOid) -> Result<(), Error> {
        let bytes = self.read(&oid, GitKind::Commit)?;
        let fields = raw::parse_commit(&bytes).map_err(|reason| Error::Malformed {
            oid,
            kind: GitKind::Commit,
            reason,
        })?;
        let mut parents = Vec::with_capacity(fields.parents.len());
        for parent in &fields.parents {
            parents.push(self.map.changeset(parent).ok_or_else(|| Error::NotMapped {
                what: "commit",
                id: parent.to_hex(),
            })?);
        }
        let (base, base_origin) = match parents.first() {
            Some(parent) => {
                let (root, origin) = self.root_of(parent)?;
                (Some(root), origin)
            }
            None => (None, None),
        };
        // What the first parent has at each path, and its own raw bytes, are
        // what the store may keep new trees, blobs and bytes as deltas
        // against.
        let root = self.convert_tree(fields.tree, oid, base)?;
        let changes = diff::changes(self.objects, &self.trees, base, root)?;
        let origin = ObjectId::hash(Kind::Blob, &bytes);
        self.put(Kind::Blob, &bytes, base_origin, origin)?;
        let unknown = || Signature {
            name: Vec::new(),
            email: Vec::new(),
            seconds: 0,
            offset_minutes: 0,
        };
        let assembled = ChangesetParts {
            parents,
            root,
            changes,
            author: fields.author.map_or_else(unknown, raw::parse_signature),
            committer: fields.committer.map_or_else(unknown, raw::parse_signature),
            message: fields.message.to_vec(),
            extras: Vec::new(),
            git_origin: Some(origin),
        }
        .assemble()
        .map_err(|err| Error::Verification {
            oid,
            kind: GitKind::Commit,
            detail: format!("its changeset cannot be assembled: {err}"),
        })?;
        if let Some((_, list)) = &assembled.external_list {
            self.put(Kind::Blob, list, None, ObjectId::hash(Kind::Blob, list))?;
        }
        let changeset = ObjectId::hash(Kind::Changeset, &assembled.encoded);
        self.put(Kind::Changeset, &assembled.encoded, None, changeset)?;

        // Export what was stored and compare. The commit enters the map only
        // once it has passed, so a resumed import checks it again; a
        // parallel import maps it now, and takes it back if it fails.
        match &mut self.deferred {
            None => {
                let mut exporter = Exporter::with_cache(self.objects, &*self.map, &self.trees);
                check_commit(&mut exporter, oid, &changeset, &fields.tree)?;
            }
            Some(deferred) => deferred.check(Check::Commit {
                oid,
                changeset,
                tree: fields.tree,
            }),
        }

        let added = self.map.insert_commit(oid, changeset);
        self.added(added);
        self.roots.insert(changeset, (root, Some(origin)));
        self.report.commits += 1;
        Ok(())
    }

    /// The root tree and the git origin of a changeset.
    fn root_of(&mut self, changeset: &ObjectId) -> Result<(ObjectId, Option<ObjectId>), Error> {
        if let Some(found) = self.roots.get(changeset) {
            return Ok(*found);
        }
        let read = fetch::changeset(self.objects, changeset)?;
        let found = (read.root, read.git_origin);
        self.roots.insert(*changeset, found);
        Ok(found)
    }

    /// Converts a git tree and everything below it, and returns its braid
    /// tree. `commit` names the commit (or the object a ref points to) in
    /// errors, and `similar` is the braid tree of its first parent, if it
    /// has one. Subtrees are converted before their parents, without
    /// recursion.
    fn convert_tree(
        &mut self,
        root: GitOid,
        commit: GitOid,
        similar: Option<ObjectId>,
    ) -> Result<ObjectId, Error> {
        if let Some(tree) = self.map.tree(&root) {
            return Ok(tree);
        }
        let mut stack = vec![self.open_tree(root, Vec::new(), commit, Similar::new(similar))?];
        loop {
            let top = stack
                .last_mut()
                .expect("the stack holds the root until it returns");
            if let Some((kind, name, oid)) = top.entries.get(top.next).cloned() {
                let kind = match kind {
                    EntryType::Tree => match self.map.tree(&oid) {
                        Some(tree) => {
                            top.canonical_subtrees &= self.map.raw_tree(&oid).is_none();
                            if tree == self.empty_tree {
                                top.dropped_empty = true;
                                top.next += 1;
                                continue;
                            }
                            EntryKind::Tree { tree }
                        }
                        None => {
                            let path = join(&top.path, &name);
                            let similar = top.similar.subtree(self.objects, &self.trees, &name)?;
                            let child = self.open_tree(oid, path, commit, similar)?;
                            stack.push(child);
                            continue;
                        }
                    },
                    EntryType::Gitlink => EntryKind::Gitlink {
                        commit: GitId::from(oid),
                    },
                    EntryType::File | EntryType::Executable | EntryType::Symlink => {
                        let (blob, size) = match self.map.blob(&oid) {
                            Some(found) => found,
                            None => {
                                let similar = top.similar.blob(self.objects, &self.trees, &name)?;
                                self.import_blob(oid, similar)?
                            }
                        };
                        match kind {
                            EntryType::File => EntryKind::File { blob, size },
                            EntryType::Executable => EntryKind::Executable { blob, size },
                            _ => EntryKind::Symlink { blob, size },
                        }
                    }
                };
                top.done.push(Entry { name, kind });
                top.next += 1;
                continue;
            }
            let finished = stack
                .pop()
                .expect("the stack holds the root until it returns");
            let tree = self.finish_tree(finished, commit)?;
            match stack.last_mut() {
                Some(parent) => {
                    let (_, name, oid) = parent.entries[parent.next].clone();
                    parent.canonical_subtrees &= self.map.raw_tree(&oid).is_none();
                    if tree == self.empty_tree {
                        // An empty directory, or one holding only empty
                        // ones: braid trees leave it out.
                        parent.dropped_empty = true;
                    } else {
                        parent.done.push(Entry {
                            name,
                            kind: EntryKind::Tree { tree },
                        });
                    }
                    parent.next += 1;
                }
                // A root may be empty.
                None => return Ok(tree),
            }
        }
    }

    /// Reads a git tree and checks every entry's mode and name before any of
    /// its content is read.
    fn open_tree(
        &mut self,
        oid: GitOid,
        path: Vec<u8>,
        commit: GitOid,
        similar: Similar,
    ) -> Result<Frame, Error> {
        let raw = self.read(&oid, GitKind::Tree)?;
        let parsed = raw::parse_tree(&raw).map_err(|reason| Error::Malformed {
            oid,
            kind: GitKind::Tree,
            reason,
        })?;
        let mut entries = Vec::with_capacity(parsed.len());
        for entry in &parsed {
            let kind = EntryType::from_mode(entry.mode).ok_or_else(|| Error::UnsupportedMode {
                commit,
                path: join(&path, entry.name),
                mode: entry.mode.to_vec(),
            })?;
            let refused = |reason: String| Error::RefusedPath {
                commit,
                path: join(&path, entry.name),
                reason,
            };
            validate_name(entry.name).map_err(|err| refused(err.to_string()))?;
            if kind == EntryType::Symlink && forbidden_as_symlink(entry.name) {
                return Err(refused("this name must not be a symlink".into()));
            }
            entries.push((kind, entry.name.to_vec(), entry.oid));
        }
        let count = entries.len();
        Ok(Frame {
            oid,
            raw,
            path,
            entries,
            next: 0,
            done: Vec::with_capacity(count),
            canonical_subtrees: true,
            dropped_empty: false,
            similar,
        })
    }

    /// Stores the braid tree of a converted git tree, decides whether the git
    /// tree is canonical, and records it.
    fn finish_tree(&mut self, frame: Frame, commit: GitOid) -> Result<ObjectId, Error> {
        let Frame {
            oid,
            raw,
            path,
            done,
            canonical_subtrees,
            dropped_empty,
            mut similar,
            ..
        } = frame;
        // Braid names are unique: of duplicate entries, the last one written
        // stays, which is the one git puts in the index and checks out. The
        // raw bytes keep the others.
        //
        // Every entry passed `validate_entry`'s checks already: `open_tree`
        // checked each name, and each name of kind symlink, of the git tree
        // these entries come from, and kinds carry over (only a git symlink
        // becomes a braid symlink).
        let written = done.len();
        let mut names = HashSet::with_capacity(written);
        let mut entries: Vec<Entry> = done
            .into_iter()
            .rev()
            .filter(|entry| names.insert(entry.name.clone()))
            .collect();
        let unique = entries.len() == written;
        // A parallel import keeps the entries, sorted as they are read back,
        // for what it converts before the tree is stored.
        let kept: Option<Arc<[Entry]>> = self.deferred.is_some().then(|| {
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            entries.as_slice().into()
        });
        let directory = build_directory(entries).map_err(|err| Error::RefusedPath {
            commit,
            path: path.clone(),
            reason: err.to_string(),
        })?;
        let hints = similar.nodes(self.objects, &self.trees, &directory)?;
        for ((id, bytes), hint) in directory.nodes.iter().zip(hints) {
            // A node that did not change is its own old version: no hint.
            let hint = hint.filter(|hint| hint != id);
            self.put(Kind::Tree, bytes, hint, *id)?;
        }
        let tree = directory.root();

        // Canonical means the export, reading back what was just stored,
        // gives exactly these bytes. A tree naming a subtree that is not
        // canonical cannot be: the subtree's canonical id differs. Neither can
        // one that lost duplicates or empty directories.
        let raw_blob = match kept {
            // A parallel import decides from the directory as converted,
            // which is what reading it back gives, and reads it back later.
            Some(kept) => {
                let canonical = canonical_subtrees
                    && unique
                    && !dropped_empty
                    && Exporter::new(self.objects, &*self.map)
                        .tree_of(&tree, &kept)?
                        .1
                        == raw;
                self.trees.hold(tree, kept);
                self.defer_tree_checks(tree, oid, raw, canonical)
            }
            None => {
                let canonical = canonical_subtrees
                    && unique
                    && !dropped_empty
                    && Exporter::with_cache(self.objects, &*self.map, &self.trees)
                        .tree(&tree)?
                        .1
                        == raw;
                if canonical {
                    None
                } else {
                    let blob = fetch::put(self.objects, Kind::Blob, &raw)?;
                    check_raw(self.objects, oid, &blob)?;
                    Some(blob)
                }
            }
        };
        let added = self.map.insert_tree(oid, tree, raw_blob);
        self.added(added);
        self.report.trees += 1;
        if raw_blob.is_some() {
            self.report.raw_trees += 1;
        }
        Ok(tree)
    }

    /// In a parallel import, queues what [`finish_tree`](Self::finish_tree)
    /// checks of a tree right after storing it, and the tree's raw bytes when
    /// it is not canonical, whose blob it returns then.
    fn defer_tree_checks(
        &mut self,
        tree: ObjectId,
        oid: GitOid,
        raw: Vec<u8>,
        canonical: bool,
    ) -> Option<ObjectId> {
        let deferred = self
            .deferred
            .as_mut()
            .expect("only a parallel import defers its checks");
        deferred.batch.held.push(tree);
        if canonical {
            deferred.check(Check::Canonical { braid: tree, raw });
            return None;
        }
        let blob = ObjectId::hash(Kind::Blob, &raw);
        deferred.put(Kind::Blob, blob, Data::Bytes(raw), None);
        deferred.check(Check::Raw { oid, blob });
        Some(blob)
    }

    /// Imports a blob, with the first parent's blob at the same path, if
    /// any, as a hint for the store. In a parallel import, the blob is
    /// mapped now and stored with its batch.
    fn import_blob(
        &mut self,
        oid: GitOid,
        similar: Option<ObjectId>,
    ) -> Result<(ObjectId, u64), Error> {
        if let Some(found) = self.map.blob(&oid) {
            return Ok(found);
        }
        let Some(deferred) = &mut self.deferred else {
            let bytes = self.repo.read_kind(&oid, GitKind::Blob)?;
            let blob = fetch::put_hinted(self.objects, Kind::Blob, &bytes, similar.as_ref())?;
            let size = bytes.len() as u64;
            self.map.insert_blob(oid, blob, size);
            self.report.blobs += 1;
            return Ok((blob, size));
        };
        let read = match deferred.ahead.blobs.remove(&oid) {
            Some(read) => read,
            None => {
                let content = self.repo.read_kind(&oid, GitKind::Blob)?;
                let keep = content.len() < LARGE_BLOB;
                ReadBlob::new(content, keep)
            }
        };
        let (id, size) = (read.id, read.size);
        let data = match read.content {
            Some(content) => Data::Bytes(content),
            None => Data::Blob(oid),
        };
        deferred.put(Kind::Blob, id, data, similar);
        let added = self.map.insert_blob(oid, id, size);
        deferred.added(added);
        self.report.blobs += 1;
        Ok((id, size))
    }

    fn import_tag(&mut self, oid: GitOid) -> Result<(), Error> {
        if self.map.tag(&oid).is_some() {
            return Ok(());
        }
        let bytes = self.repo.read_kind(&oid, GitKind::Tag)?;
        let target = raw::parse_tag(&bytes).map_err(|reason| Error::Malformed {
            oid,
            kind: GitKind::Tag,
            reason,
        })?;
        let target_kind = self.repo.kind(&target)?;
        let imported = match target_kind {
            GitKind::Commit => self.map.changeset(&target).is_some(),
            GitKind::Tree => self.map.tree(&target).is_some(),
            GitKind::Blob => self.map.blob(&target).is_some(),
            GitKind::Tag => self.map.tag(&target).is_some(),
        };
        if !imported {
            return Err(Error::NotMapped {
                what: target_kind.name(),
                id: target.to_hex(),
            });
        }
        let raw = fetch::put(self.objects, Kind::Blob, &bytes)?;
        let stored = fetch::get(self.objects, &raw, Kind::Blob)?;
        if GitOid::for_object(GitKind::Tag, &stored)? != oid {
            return Err(Error::Verification {
                oid,
                kind: GitKind::Tag,
                detail: "its raw bytes do not read back".into(),
            });
        }
        self.map.insert_tag(
            oid,
            TagEntry {
                raw,
                target,
                target_kind,
            },
        );
        self.report.tags += 1;
        Ok(())
    }

    /// Takes back, the last first, what a batch added to the map after the
    /// place `after`, or all of it.
    fn take_back(&mut self, added: &[(u64, Added)], after: Option<u64>) {
        for (seq, added) in added.iter().rev() {
            if after.is_none_or(|after| *seq > after) {
                self.map.take_back(added);
            }
        }
    }
}

impl<O: Objects + Sync> Importer<'_, '_, O> {
    /// Imports the commits of `order`, then the trees and blobs that refs or
    /// tags point to, in batches: see [`import_parallel`]. Returns once every
    /// object is stored and checked.
    fn import_in_batches(
        &mut self,
        order: &[GitOid],
        plan: &Plan,
        handles: &Handles,
    ) -> Result<(), Error> {
        self.deferred = Some(Deferred::default());
        let mut storing = Batch::default();
        for commits in order.chunks(BATCH_COMMITS) {
            storing = self.step(storing, handles, |importer| {
                importer.read_ahead(commits, handles);
                for commit in commits {
                    importer.import_commit(*commit)?;
                }
                Ok(())
            })?;
        }
        storing = self.step(storing, handles, |importer| importer.import_loose(plan))?;
        self.step(storing, handles, |_| Ok(()))?;
        self.deferred = None;
        Ok(())
    }

    /// Converts one batch with `convert` on this thread while the objects of
    /// the previous one, `storing`, are stored on the others; then checks
    /// them. Returns what the new batch left to store and check.
    fn step(
        &mut self,
        storing: Batch,
        handles: &Handles,
        convert: impl FnOnce(&mut Self) -> Result<(), Error> + Send,
    ) -> Result<Batch, Error> {
        let objects = self.objects;
        let (converted, stored) = rayon::join(
            || convert(&mut *self),
            || store_puts(objects, handles, &storing.puts),
        );
        let batch = match &mut self.deferred {
            Some(deferred) => {
                deferred.ahead = ReadAhead::default();
                std::mem::take(&mut deferred.batch)
            }
            None => Batch::default(),
        };
        if let Some((seq, err)) = self.finish_batch(&storing, stored) {
            // Nothing of the new batch counts: a one-thread import would have
            // stopped before it.
            self.take_back(&batch.added, None);
            self.take_back(&storing.added, Some(seq));
            return Err(err);
        }
        self.trees.let_go(&storing.held);
        if let Err(err) = converted {
            // Converting stopped: what it did so far is stored and checked
            // first, as a one-thread import would have done before stopping.
            let stored = store_puts(objects, handles, &batch.puts);
            if let Some((seq, failed)) = self.finish_batch(&batch, stored) {
                self.take_back(&batch.added, Some(seq));
                return Err(failed);
            }
            return Err(err);
        }
        Ok(batch)
    }

    /// Checks a batch whose objects were stored, or stored up to a failure,
    /// and returns the first failure, if any: of storing, or of a check made
    /// before it.
    fn finish_batch(&self, batch: &Batch, stored: Result<(), Failure>) -> Option<Failure> {
        let stored = stored.err();
        let before = stored.as_ref().map(|(seq, _)| *seq);
        let checked = run_checks(self.objects, self.map, &batch.checks, before).err();
        match (stored, checked) {
            (Some(stored), Some(checked)) => Some(if checked.0 < stored.0 {
                checked
            } else {
                stored
            }),
            (stored, checked) => stored.or(checked),
        }
    }

    /// Reads from git, on every thread, what converting `commits` will read:
    /// the commits, the git trees not converted yet below them, and the blobs
    /// not imported yet in those trees, with their braid ids. What fails to
    /// read is left out; converting reads it again and fails there, as it
    /// would have.
    fn read_ahead(&mut self, commits: &[GitOid], handles: &Handles) {
        let map: &GitMap = self.map;
        let read_commits: Vec<(GitOid, Vec<u8>, GitOid)> = commits
            .par_iter()
            .map_init(
                || handles.get(),
                |git, oid| {
                    let bytes = git.read_kind(oid, GitKind::Commit).ok()?;
                    let tree = raw::parse_commit(&bytes).ok()?.tree;
                    Some((*oid, bytes, tree))
                },
            )
            .flatten()
            .collect();

        // Each tree is read once, by whichever thread meets it first.
        let seen = Mutex::new(HashSet::new());
        let found: Vec<NewTrees> = read_commits
            .par_iter()
            .map_init(
                || handles.get(),
                |git, (_, _, tree)| new_trees(git, map, &seen, *tree),
            )
            .collect();

        let mut ahead = ReadAhead::default();
        let mut blobs = Vec::new();
        let mut seen_blobs = HashSet::new();
        for (trees, found_blobs) in found {
            ahead.trees.extend(trees);
            blobs.extend(
                found_blobs
                    .into_iter()
                    .filter(|oid| seen_blobs.insert(*oid)),
            );
        }
        let kept = AtomicUsize::new(0);
        ahead.blobs = blobs
            .par_iter()
            .map_init(
                || handles.get(),
                |git, oid| {
                    let content = git.read_kind(oid, GitKind::Blob).ok()?;
                    let len = content.len();
                    let keep = len < LARGE_BLOB
                        && kept.fetch_add(len, Ordering::Relaxed) + len <= KEPT_BYTES;
                    Some((*oid, ReadBlob::new(content, keep)))
                },
            )
            .flatten()
            .collect();
        ahead.commits = read_commits
            .into_iter()
            .map(|(oid, bytes, _)| (oid, bytes))
            .collect();
        if let Some(deferred) = &mut self.deferred {
            deferred.ahead = ahead;
        }
    }
}

/// Git trees with their bytes, and the blobs they name.
type NewTrees = (Vec<(GitOid, Vec<u8>)>, Vec<GitOid>);

/// The git trees below `root` that the map does not hold and no other thread
/// claimed in `seen`, with their bytes, and the blobs they name that the map
/// does not hold.
fn new_trees(git: &GitRepo, map: &GitMap, seen: &Mutex<HashSet<GitOid>>, root: GitOid) -> NewTrees {
    let mut trees = Vec::new();
    let mut blobs = Vec::new();
    let mut pending = vec![root];
    while let Some(oid) = pending.pop() {
        if map.tree(&oid).is_some() || !lock(seen).insert(oid) {
            continue;
        }
        let Ok(bytes) = git.read_kind(&oid, GitKind::Tree) else {
            continue;
        };
        if let Ok(entries) = raw::parse_tree(&bytes) {
            for entry in entries {
                match EntryType::from_mode(entry.mode) {
                    Some(EntryType::Tree) => pending.push(entry.oid),
                    Some(EntryType::File | EntryType::Executable | EntryType::Symlink)
                        if map.blob(&entry.oid).is_none() =>
                    {
                        blobs.push(entry.oid);
                    }
                    _ => {}
                }
            }
        }
        trees.push((oid, bytes));
    }
    (trees, blobs)
}

/// Stores the objects a batch queued, on every thread. An object whose hint
/// names another object of the batch is stored after it, and the rest in
/// any order, so that the store sees each hint stored, or not, as it would
/// have one object at a time. The failure is the first in the order of
/// `puts`; what depends on an object that failed is not stored.
fn store_puts<O: Objects + Sync>(
    objects: &O,
    handles: &Handles,
    puts: &[Put],
) -> Result<(), Failure> {
    if puts.is_empty() {
        return Ok(());
    }
    let position: HashMap<ObjectId, usize> = puts
        .iter()
        .enumerate()
        .map(|(at, put)| (put.id, at))
        .collect();
    // The objects to store after each one, and the ones to start from,
    // longest chains first.
    let mut after: Vec<Vec<usize>> = vec![Vec::new(); puts.len()];
    let mut starts = Vec::new();
    for (at, put) in puts.iter().enumerate() {
        match put.similar.and_then(|similar| position.get(&similar)) {
            Some(&hint) if hint < at => after[hint].push(at),
            _ => starts.push(at),
        }
    }
    let mut below = vec![1usize; puts.len()];
    for at in (0..puts.len()).rev() {
        below[at] += after[at].iter().map(|next| below[*next]).sum::<usize>();
    }
    starts.sort_by_key(|start| std::cmp::Reverse(below[*start]));
    let failed = starts
        .par_iter()
        .with_max_len(1)
        .map_init(
            || handles.get(),
            |git, start| {
                let mut pending = vec![*start];
                while let Some(at) = pending.pop() {
                    let put = &puts[at];
                    if let Err(err) = store_put(objects, git, put) {
                        return Some((put.seq, err));
                    }
                    pending.extend(after[at].iter().rev());
                }
                None
            },
        )
        .flatten()
        .min_by_key(|(seq, _)| *seq);
    match failed {
        Some(failure) => Err(failure),
        None => Ok(()),
    }
}

fn store_put<O: Objects>(objects: &O, git: &GitRepo, put: &Put) -> Result<(), Error> {
    let read;
    let payload = match &put.data {
        Data::Bytes(bytes) => bytes,
        Data::Blob(oid) => {
            read = git.read_kind(oid, GitKind::Blob)?;
            &read
        }
    };
    fetch::put_known(objects, put.kind, payload, put.similar.as_ref(), put.id)?;
    Ok(())
}

/// Makes a batch's checks, on every thread, reading back from the store; with
/// `before`, only the checks before that place. The failure is the first in
/// the order of `checks`.
fn run_checks<O: Objects + Sync>(
    objects: &O,
    map: &GitMap,
    checks: &[(u64, Check)],
    before: Option<u64>,
) -> Result<(), Failure> {
    let failed = checks
        .par_iter()
        .filter(|(seq, _)| before.is_none_or(|before| *seq < before))
        .filter_map(|(seq, check)| {
            let mut exporter = Exporter::new(objects, map);
            let checked = match check {
                Check::Canonical { braid, raw } => match exporter.tree(braid) {
                    Ok((_, bytes)) if bytes == *raw => Ok(()),
                    Ok((oid, _)) => Err(Error::Verification {
                        oid,
                        kind: GitKind::Tree,
                        detail: "it does not read back as it was converted".into(),
                    }),
                    Err(err) => Err(err),
                },
                Check::Raw { oid, blob } => check_raw(objects, *oid, blob),
                Check::Commit {
                    oid,
                    changeset,
                    tree,
                } => check_commit(&mut exporter, *oid, changeset, tree),
            };
            checked.err().map(|err| (*seq, err))
        })
        .min_by_key(|(seq, _)| *seq);
    match failed {
        Some(failure) => Err(failure),
        None => Ok(()),
    }
}

/// Exports a changeset again from what was stored, and checks that it is the
/// commit `oid` it was imported from, and that the commit's tree exports.
fn check_commit<O: Objects>(
    exporter: &mut Exporter<'_, O>,
    oid: GitOid,
    changeset: &ObjectId,
    tree: &GitOid,
) -> Result<(), Error> {
    let verification = |detail: String| Error::Verification {
        oid,
        kind: GitKind::Commit,
        detail,
    };
    let (exported, _) = exporter
        .commit(changeset)
        .map_err(|err| verification(err.to_string()))?;
    if exported != oid {
        return Err(verification(format!("exported again as {exported}")));
    }
    exporter
        .object(tree)
        .map_err(|err| verification(format!("its tree: {err}")))?;
    Ok(())
}

/// Checks that the raw bytes of the git tree `oid`, stored as `blob`, read
/// back as that tree.
fn check_raw<O: Objects>(objects: &O, oid: GitOid, blob: &ObjectId) -> Result<(), Error> {
    let stored = fetch::get(objects, blob, Kind::Blob)?;
    if GitOid::for_object(GitKind::Tree, &stored)? != oid {
        return Err(Error::Verification {
            oid,
            kind: GitKind::Tree,
            detail: "its raw bytes do not read back".into(),
        });
    }
    Ok(())
}

/// Handles on the git repository for the threads of a parallel import, which
/// cannot share one.
struct Handles {
    /// What new handles are made from.
    template: Mutex<GitRepo>,
    /// Handles not in use.
    free: Mutex<Vec<GitRepo>>,
}

impl Handles {
    fn new(repo: &GitRepo) -> Self {
        Self {
            template: Mutex::new(repo.worker_handle()),
            free: Mutex::new(Vec::new()),
        }
    }

    /// A handle for this thread, until the guard is dropped.
    fn get(&self) -> Handle<'_> {
        let free = lock(&self.free).pop();
        let repo = free.unwrap_or_else(|| lock(&self.template).worker_handle());
        Handle {
            handles: self,
            repo: Some(repo),
        }
    }
}

/// A handle in use; it goes back to its [`Handles`] when dropped.
struct Handle<'h> {
    handles: &'h Handles,
    repo: Option<GitRepo>,
}

impl Deref for Handle<'_> {
    type Target = GitRepo;

    fn deref(&self) -> &GitRepo {
        self.repo
            .as_ref()
            .expect("a handle holds its repository until dropped")
    }
}

impl Drop for Handle<'_> {
    fn drop(&mut self) {
        if let Some(repo) = self.repo.take() {
            lock(&self.handles.free).push(repo);
        }
    }
}

/// Locks a mutex that only guards plain inserts and removals: a panic while
/// it was held cannot have left it half-written.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn join(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return name.to_vec();
    }
    let mut path = Vec::with_capacity(prefix.len() + 1 + name.len());
    path.extend_from_slice(prefix);
    path.push(b'/');
    path.extend_from_slice(name);
    path
}
