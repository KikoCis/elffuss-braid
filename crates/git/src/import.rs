// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Import: git history into braid objects.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use braid_format::name::forbidden_as_symlink;
use braid_format::tree::validate_entry;
use braid_format::{
    ChangesetParts, Entry, EntryKind, GitId, Kind, ObjectId, Objects, Signature, build_directory,
    validate_name,
};

use crate::diff;
use crate::error::Error;
use crate::export::Exporter;
use crate::fetch;
use crate::map::{GitMap, TagEntry};
use crate::oid::{GitKind, GitOid};
use crate::raw::{self, EntryType};
use crate::repo::GitRepo;

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
pub fn import<O: Objects>(
    git_repo: &Path,
    objects: &O,
    map: &mut GitMap,
    options: &ImportOptions,
) -> Result<ImportReport, Error> {
    let repo = GitRepo::open(git_repo)?;
    if repo.is_shallow()? {
        return Err(Error::Shallow);
    }
    let refs = repo.refs(&options.refs)?;
    let mut importer = Importer {
        repo: &repo,
        objects,
        map,
        report: ImportReport::default(),
        roots: HashMap::new(),
        empty_tree: build_directory(Vec::new())
            .expect("the empty directory is valid")
            .root(),
    };

    // What the refs point to: commits to walk from, trees and blobs that tags
    // point to directly, and annotated tags, each after its target.
    let mut plan = Plan::default();
    for (_, oid) in &refs {
        importer.plan_ref(*oid, &mut plan)?;
    }
    for commit in importer.commit_order(&plan.tips)? {
        importer.import_commit(commit)?;
    }
    for (oid, kind) in &plan.loose {
        match kind {
            GitKind::Tree => {
                importer.convert_tree(*oid, *oid)?;
            }
            _ => {
                importer.import_blob(*oid)?;
            }
        }
    }
    for tag in &plan.tags {
        importer.import_tag(*tag)?;
    }

    for (name, oid) in refs {
        let kind = repo.kind(&oid)?;
        let map = &mut *importer.map;
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
        importer.report.refs.push(ImportedRef {
            name,
            oid,
            kind,
            braid,
        });
    }
    Ok(importer.report)
}

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

struct Importer<'r, 'o, 'm, O: Objects> {
    repo: &'r GitRepo,
    objects: &'o O,
    map: &'m mut GitMap,
    report: ImportReport,
    /// Root trees of the changesets seen in this import.
    roots: HashMap<ObjectId, ObjectId>,
    /// The braid id of the empty directory.
    empty_tree: ObjectId,
}

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
}

impl<O: Objects> Importer<'_, '_, '_, O> {
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

    fn import_commit(&mut self, oid: GitOid) -> Result<(), Error> {
        let bytes = self.repo.read_kind(&oid, GitKind::Commit)?;
        let fields = raw::parse_commit(&bytes).map_err(|reason| Error::Malformed {
            oid,
            kind: GitKind::Commit,
            reason,
        })?;
        let root = self.convert_tree(fields.tree, oid)?;
        let mut parents = Vec::with_capacity(fields.parents.len());
        for parent in &fields.parents {
            parents.push(self.map.changeset(parent).ok_or_else(|| Error::NotMapped {
                what: "commit",
                id: parent.to_hex(),
            })?);
        }
        let base = match parents.first() {
            Some(parent) => Some(self.root_of(parent)?),
            None => None,
        };
        let changes = diff::changes(self.objects, base, root)?;
        let origin = fetch::put(self.objects, Kind::Blob, &bytes)?;
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
            fetch::put(self.objects, Kind::Blob, list)?;
        }
        let changeset = fetch::put(self.objects, Kind::Changeset, &assembled.encoded)?;

        // Export what was stored and compare. The commit enters the map only
        // once it has passed, so a resumed import checks it again.
        let verification = |detail: String| Error::Verification {
            oid,
            kind: GitKind::Commit,
            detail,
        };
        let mut exporter = Exporter::new(self.objects, &*self.map);
        let (exported, _) = exporter
            .commit(&changeset)
            .map_err(|err| verification(err.to_string()))?;
        if exported != oid {
            return Err(verification(format!("exported again as {exported}")));
        }
        exporter
            .object(&fields.tree)
            .map_err(|err| verification(format!("its tree: {err}")))?;

        self.map.insert_commit(oid, changeset);
        self.roots.insert(changeset, root);
        self.report.commits += 1;
        Ok(())
    }

    fn root_of(&mut self, changeset: &ObjectId) -> Result<ObjectId, Error> {
        if let Some(root) = self.roots.get(changeset) {
            return Ok(*root);
        }
        let root = fetch::changeset(self.objects, changeset)?.root;
        self.roots.insert(*changeset, root);
        Ok(root)
    }

    /// Converts a git tree and everything below it, and returns its braid
    /// tree. `commit` names the commit (or the object a ref points to) in
    /// errors. Subtrees are converted before their parents, without recursion.
    fn convert_tree(&mut self, root: GitOid, commit: GitOid) -> Result<ObjectId, Error> {
        if let Some(tree) = self.map.tree(&root) {
            return Ok(tree);
        }
        let mut stack = vec![self.open_tree(root, Vec::new(), commit)?];
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
                            let child = self.open_tree(oid, path, commit)?;
                            stack.push(child);
                            continue;
                        }
                    },
                    EntryType::Gitlink => EntryKind::Gitlink {
                        commit: GitId::from(oid),
                    },
                    EntryType::File | EntryType::Executable | EntryType::Symlink => {
                        let (blob, size) = self.import_blob(oid)?;
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
    fn open_tree(&self, oid: GitOid, path: Vec<u8>, commit: GitOid) -> Result<Frame, Error> {
        let raw = self.repo.read_kind(&oid, GitKind::Tree)?;
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
            ..
        } = frame;
        // Braid names are unique: of duplicate entries, the last one written
        // stays, which is the one git puts in the index and checks out. The
        // raw bytes keep the others.
        let written = done.len();
        let mut names = HashSet::with_capacity(written);
        let entries: Vec<Entry> = done
            .into_iter()
            .rev()
            .filter(|entry| names.insert(entry.name.clone()))
            .collect();
        let unique = entries.len() == written;
        for entry in &entries {
            validate_entry(entry).map_err(|err| Error::RefusedPath {
                commit,
                path: join(&path, &entry.name),
                reason: err.to_string(),
            })?;
        }
        let directory = build_directory(entries).map_err(|err| Error::RefusedPath {
            commit,
            path: path.clone(),
            reason: err.to_string(),
        })?;
        for (_, bytes) in &directory.nodes {
            fetch::put(self.objects, Kind::Tree, bytes)?;
        }
        let tree = directory.root();

        // Canonical means the export, reading back what was just stored,
        // gives exactly these bytes. A tree naming a subtree that is not
        // canonical cannot be: the subtree's canonical id differs. Neither can
        // one that lost duplicates or empty directories.
        let canonical = canonical_subtrees
            && unique
            && !dropped_empty
            && Exporter::new(self.objects, &*self.map).tree(&tree)?.1 == raw;
        let raw_blob = if canonical {
            None
        } else {
            let blob = fetch::put(self.objects, Kind::Blob, &raw)?;
            let stored = fetch::get(self.objects, &blob, Kind::Blob)?;
            if GitOid::for_object(GitKind::Tree, &stored)? != oid {
                return Err(Error::Verification {
                    oid,
                    kind: GitKind::Tree,
                    detail: "its raw bytes do not read back".into(),
                });
            }
            Some(blob)
        };
        self.map.insert_tree(oid, tree, raw_blob);
        self.report.trees += 1;
        if raw_blob.is_some() {
            self.report.raw_trees += 1;
        }
        Ok(tree)
    }

    fn import_blob(&mut self, oid: GitOid) -> Result<(ObjectId, u64), Error> {
        if let Some(found) = self.map.blob(&oid) {
            return Ok(found);
        }
        let bytes = self.repo.read_kind(&oid, GitKind::Blob)?;
        let blob = fetch::put(self.objects, Kind::Blob, &bytes)?;
        let size = bytes.len() as u64;
        self.map.insert_blob(oid, blob, size);
        self.report.blobs += 1;
        Ok((blob, size))
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
