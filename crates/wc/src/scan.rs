// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Reading a working copy: where it differs from its base, and the records
//! of the stat cache for the next scan.
//!
//! The walk opens each directory once, by its name in its parent's handle
//! and never through a symlink, and reads its entries once, taking their
//! types from the listing where the platform gives them and asking for the
//! metadata of files only. Directories are read in parallel. The walk skips
//! `.braid`, `.git` and the names that alias them, braid's own scratch
//! files, ignored paths that are not tracked, and untracked directories that
//! are another workspace or repository. Files the stat cache cannot vouch
//! for are hashed, in parallel.
//!
//! The base is read a directory at a time, where it is needed: a directory
//! whose record says it was clean against the tree the base still has at its
//! path, and whose entries still match their records, differs in nothing,
//! and its tree is not read. A directory whose stamp says its entries did
//! not change since the scan that recorded it is not listed either: its
//! record stands for its listing, and only its files' metadata is read.
//!
//! A scan may cover only some paths, as committing those paths needs: it
//! then reads only the directories on the way to them and below them, and
//! makes no records. After a commit, the records of a scan of the whole
//! working copy are brought onto the new base, so that the next scan finds
//! the committed directories as the commit left them.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use braid_engine::read_tree;
use braid_format::{ChangeOp, Entry, EntryKind, IdHasher, Kind, ObjectId, Objects};
use cap_std::fs::Dir;
use rayon::prelude::*;

use crate::cache::{
    Cache, CachedDir, DirRecord, FileRecord, KIND_EXECUTABLE, KIND_FILE, KIND_GITLINK, KIND_SUBDIR,
    KIND_SYMLINK, KIND_TREE, Record,
};
use crate::error::Error;
use crate::fsprobe::{FileSystem, SCRATCH_PREFIX};
use crate::ignores::{self, Chain};
use crate::paths::{ancestors, is_within, join, os_name};
use crate::safe::{
    DirStamp, Found, Root, Stat, StatKind, dir_stamp, list, open_child_dir, open_file, read_link,
    stat_entry,
};
use crate::tree::leaves;

/// How many threads read the working copy, at most. File systems serialise
/// much of their work on names and metadata: past a few threads, more of
/// them add contention, not speed.
const THREADS: usize = 4;

/// How many directory handles the walk keeps open for directories whose
/// subdirectories are still to be read. Past this, a subdirectory is opened
/// from the root again, one name at a time: deep trees read in parallel
/// would otherwise hold a handle for every level on every thread.
const HELD_HANDLES: usize = 128;

/// A path where the working copy differs from the base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Delta {
    pub path: Vec<u8>,
    /// What the base has there: a file, a symlink or a gitlink.
    pub old: Option<ChangeOp>,
    /// What the working copy has there that counts.
    pub new: Option<ChangeOp>,
}

/// What a scan found.
#[derive(Debug)]
pub(crate) struct Scan<'c> {
    /// Every path where the working copy differs from the base, sorted.
    pub deltas: Vec<Delta>,
    /// The stat cache for the next scan, when this scan covered the whole
    /// working copy.
    pub cache: Option<Next<'c>>,
    /// How many directories were listed, rather than taken from their
    /// records.
    #[cfg(test)]
    pub listed: usize,
}

/// The stat cache a scan of the whole working copy leaves for the next.
#[derive(Debug)]
pub(crate) struct Next<'c> {
    /// When the scan started, by the file system's clock.
    pub time: (i64, u32),
    /// Every directory's record, by path.
    pub records: Vec<Record<'c>>,
    /// Whether they say something the old cache does not.
    pub changed: bool,
}

/// Scans the working copy at `root` against the base tree `base`, using
/// `old` so that files and trees that did not change are not read again.
/// `fs` says which names the file system takes for the same. Given paths in
/// `scope`, only what lies at or below them is scanned; no paths, or the
/// root among them, is the whole working copy.
pub(crate) fn scan<'c, O: Objects + Sync>(
    root: &Root,
    objects: &O,
    base: ObjectId,
    old: &'c Cache,
    fs: FileSystem,
    scope: &[Vec<u8>],
) -> Result<Scan<'c>, Error> {
    scan_holding(root, objects, base, old, fs, scope, HELD_HANDLES)
}

/// [`scan`], keeping at most `held` directory handles open for directories
/// still to read.
fn scan_holding<'c, O: Objects + Sync>(
    root: &Root,
    objects: &O,
    base: ObjectId,
    old: &'c Cache,
    fs: FileSystem,
    scope: &[Vec<u8>],
    held: usize,
) -> Result<Scan<'c>, Error> {
    let scope = Scope::of(scope);
    let whole = matches!(scope, Scope::Whole);
    let time = if whole {
        root.file_system_now().unwrap_or_else(|_| fallback_now())
    } else {
        old.time()
    };
    let dir = root
        .dir()
        .try_clone()
        .map_err(Error::io("open", root.path()))?;
    let walk = Walk {
        root,
        objects,
        old,
        fs,
        whole,
        time,
        held_limit: held,
        held: AtomicUsize::new(0),
        #[cfg(test)]
        listed: AtomicUsize::new(0),
        failed: AtomicBool::new(false),
        out: Mutex::new(Out::default()),
    };
    let job = Job {
        open: Open::Ready(dir),
        path: Vec::new(),
        base: Some(base),
        rules: None,
        ignored: false,
        scope: scope.clone(),
    };
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(THREADS));
    match rayon::ThreadPoolBuilder::new().num_threads(threads).build() {
        Ok(pool) => pool.scope(|s| walk.run(s, job)),
        Err(_) => rayon::scope(|s| walk.run(s, job)),
    }
    #[cfg(test)]
    let listed = walk.listed.into_inner();
    let out = walk
        .out
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(error) = out.error {
        return Err(error);
    }
    let mut deltas = out.deltas;
    if let Scope::Within(paths) = &scope {
        // Removals found by reading the base may reach past the scope.
        deltas.retain(|delta| paths.iter().any(|path| is_within(&delta.path, path)));
    }
    deltas.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    let cache = whole.then(|| {
        let mut records = out.records;
        records.sort_unstable_by(|a, b| a.path().cmp(b.path()));
        Next {
            time,
            records,
            changed: out.changed || out.seen != old.dir_count(),
        }
    });
    Ok(Scan {
        deltas,
        cache,
        #[cfg(test)]
        listed,
    })
}

/// The records of a scan of the whole working copy, brought onto a new
/// base: one made from the scanned base by changing `paths`, whose tree is
/// `root`. Only the directories on the way to those paths have new trees;
/// each is compared with its new tree, from what its record says, so that
/// the next scan finds it as this one left it.
pub(crate) fn follow<'c, O: Objects>(
    objects: &O,
    records: Vec<Record<'c>>,
    root: ObjectId,
    paths: &[Vec<u8>],
) -> Result<Vec<Record<'c>>, Error> {
    let mut changed: HashSet<&[u8]> = paths.iter().flat_map(|path| ancestors(path)).collect();
    changed.insert(b"");
    // The new trees of those directories, each read once, from the root
    // down.
    let mut trees: HashMap<Vec<u8>, (ObjectId, Vec<Entry>)> = HashMap::new();
    let mut pending = vec![(Vec::new(), root)];
    while let Some((path, tree)) = pending.pop() {
        let entries = read_tree(objects, tree)?;
        for entry in &entries {
            if let EntryKind::Tree { tree } = entry.kind {
                let child = join(&path, &entry.name);
                if changed.contains(child.as_slice()) {
                    pending.push((child, tree));
                }
            }
        }
        trees.insert(path, (tree, entries));
    }
    Ok(records
        .into_iter()
        .map(|record| {
            if !changed.contains(record.path()) {
                return record;
            }
            let dir = record.into_made();
            let rebased = match trees.get(&dir.path) {
                Some((tree, entries)) => rebased(dir, Some(*tree), entries),
                None => rebased(dir, None, &[]),
            };
            Record::Made(rebased)
        })
        .collect())
}

/// A directory's record, compared with `tree`, whose entries are `entries`,
/// instead of the tree it was made against.
fn rebased(mut dir: DirRecord, tree: Option<ObjectId>, entries: &[Entry]) -> DirRecord {
    let mut leaves = entries
        .iter()
        .filter(|entry| !matches!(entry.kind, EntryKind::Tree { .. }));
    let mut same = true;
    for file in &mut dir.files {
        if file.kind >= KIND_TREE {
            let subtree = match at_tree(entries, &file.name) {
                At::Tree(subtree) => Some(subtree),
                _ => None,
            };
            file.kind = if subtree.is_some() {
                KIND_TREE
            } else {
                KIND_SUBDIR
            };
            file.blob = subtree.unwrap_or(ObjectId::from_bytes([0; 32]));
            continue;
        }
        same &= leaves.next().is_some_and(|entry| {
            let (kind, blob) = match &entry.kind {
                EntryKind::File { blob, .. } => (KIND_FILE, Some(*blob)),
                EntryKind::Executable { blob, .. } => (KIND_EXECUTABLE, Some(*blob)),
                EntryKind::Symlink { blob, .. } => (KIND_SYMLINK, Some(*blob)),
                _ => (KIND_GITLINK, None),
            };
            entry.name == file.name
                && kind == file.kind
                && blob.is_none_or(|blob| blob == file.blob)
        });
    }
    let subtrees_there = entries.iter().all(|entry| match entry.kind {
        EntryKind::Tree { .. } => dir
            .files
            .binary_search_by(|file| file.name.as_slice().cmp(&entry.name))
            .is_ok_and(|index| dir.files[index].kind == KIND_TREE),
        _ => true,
    });
    dir.clean = tree.is_some() && same && leaves.next().is_none() && subtrees_there;
    dir.base = tree;
    dir
}

/// What part of a directory a scan covers.
#[derive(Clone, Debug)]
enum Scope {
    /// All of it.
    Whole,
    /// What lies at or below these paths, relative to it.
    Within(Vec<Vec<u8>>),
}

impl Scope {
    fn of(paths: &[Vec<u8>]) -> Scope {
        if paths.is_empty() || paths.iter().any(|path| path.is_empty()) {
            Scope::Whole
        } else {
            Scope::Within(paths.to_vec())
        }
    }

    /// The part of the entry `name` covered, if any.
    fn enter(&self, name: &[u8]) -> Option<Scope> {
        let Scope::Within(paths) = self else {
            return Some(Scope::Whole);
        };
        let mut below = Vec::new();
        for path in paths {
            if path.as_slice() == name {
                return Some(Scope::Whole);
            }
            if path.len() > name.len() && path.starts_with(name) && path[name.len()] == b'/' {
                let rest = &path[name.len() + 1..];
                if rest.is_empty() {
                    return Some(Scope::Whole);
                }
                below.push(rest.to_vec());
            }
        }
        (!below.is_empty()).then_some(Scope::Within(below))
    }
}

/// One directory to read.
struct Job<'s> {
    open: Open<'s>,
    path: Vec<u8>,
    /// The base's tree at this path, if it has one.
    base: Option<ObjectId>,
    /// The ignore rules in force above it.
    rules: Chain,
    /// Whether it is ignored: inside, only what the base has counts.
    ignored: bool,
    scope: Scope,
}

/// How a job gets its directory's handle.
enum Open<'s> {
    /// It has it: the root.
    Ready(Dir),
    /// By its name in its parent's handle, which the parent's jobs share.
    Child(Arc<Shared<'s>>, Vec<u8>),
    /// From the root, one name at a time.
    FromRoot,
}

/// A directory handle that jobs not started yet share, counted while it is
/// open.
struct Shared<'s> {
    dir: Dir,
    held: &'s AtomicUsize,
}

impl Drop for Shared<'_> {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::Relaxed);
    }
}

/// What every job adds to.
#[derive(Default)]
struct Out<'c> {
    deltas: Vec<Delta>,
    records: Vec<Record<'c>>,
    /// Whether a record differs from the old cache's, or a file was read
    /// whose record will vouch for it.
    changed: bool,
    /// How many directories read had a record in the old cache.
    seen: usize,
    error: Option<Error>,
}

/// What the base has at a name of a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
    Nothing,
    /// A file, executable or symlink.
    Leaf,
    Gitlink,
    Tree(ObjectId),
}

/// An entry of a directory, as listed.
struct Listed {
    name: Vec<u8>,
    /// The listing says it is a directory.
    dir: bool,
}

/// A file, symlink or gitlink of the working copy that counts.
struct Counted {
    name: Vec<u8>,
    /// Its metadata; `None` for a gitlink's directory.
    stat: Option<Stat>,
    /// What it is now, once known; `None` for a gitlink, which is what the
    /// base has.
    op: Option<ChangeOp>,
    /// Whether its record may vouch for it in the next scan.
    vouches: bool,
    /// Whether this scan read it.
    read: bool,
}

impl Counted {
    fn kind(&self) -> u8 {
        match &self.op {
            None => KIND_GITLINK,
            Some(ChangeOp::Executable { .. }) => KIND_EXECUTABLE,
            Some(ChangeOp::Symlink { .. }) => KIND_SYMLINK,
            Some(_) => KIND_FILE,
        }
    }

    fn blob(&self) -> Option<ObjectId> {
        match &self.op {
            Some(
                ChangeOp::File { blob, .. }
                | ChangeOp::Executable { blob, .. }
                | ChangeOp::Symlink { blob, .. },
            ) => Some(*blob),
            _ => None,
        }
    }

    fn record(self) -> FileRecord {
        let kind = self.kind();
        match (&self.stat, self.blob()) {
            (Some(stat), Some(blob)) => FileRecord::new(self.name, stat, kind, self.vouches, blob),
            _ => FileRecord::bare(self.name, kind, None),
        }
    }
}

/// What a directory holds, sorted by [`Walk::classify`].
struct Sorted {
    /// What counts, by name.
    counted: Vec<Counted>,
    /// The subdirectories to read, by name.
    children: Vec<Child>,
    /// Whether an entry was gone by the time it was looked at.
    vanished: bool,
}

/// A subdirectory to read.
struct Child {
    name: Vec<u8>,
    path: Vec<u8>,
    base: Option<ObjectId>,
    ignored: bool,
    scope: Scope,
}

struct Walk<'a, 'c, O> {
    root: &'a Root,
    objects: &'a O,
    old: &'c Cache,
    fs: FileSystem,
    /// Whether the scan covers the whole working copy.
    whole: bool,
    /// When the scan started, by the file system's clock.
    time: (i64, u32),
    /// How many directory handles to keep open for jobs not started yet.
    held_limit: usize,
    /// Directory handles kept open for jobs not started yet.
    held: AtomicUsize,
    /// Directories listed.
    #[cfg(test)]
    listed: AtomicUsize,
    failed: AtomicBool,
    out: Mutex<Out<'c>>,
}

impl<'a, 'c, O: Objects + Sync> Walk<'a, 'c, O> {
    fn out(&self) -> MutexGuard<'_, Out<'c>> {
        self.out.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn run<'s>(&'s self, scope: &rayon::Scope<'s>, job: Job<'s>) {
        if self.failed.load(Ordering::Relaxed) {
            return;
        }
        if let Err(error) = self.visit(scope, job) {
            self.failed.store(true, Ordering::Relaxed);
            self.out().error.get_or_insert(error);
        }
    }

    fn visit<'s>(&'s self, scope: &rayon::Scope<'s>, job: Job<'s>) -> Result<(), Error> {
        let Job {
            open,
            path,
            base,
            rules: above,
            ignored,
            scope: covered,
        } = job;
        let dir = match open {
            Open::Ready(dir) => Some(dir),
            Open::Child(parent, name) => {
                let os = os_name(&name).ok_or_else(|| unrepresentable(&path))?;
                let opened = open_child_dir(&parent.dir, os);
                drop(parent);
                opened.map_err(|err| self.root.error("open", &path, err))?
            }
            Open::FromRoot => match self.root.open_dir(&path)? {
                Found::Here(dir) => Some(dir),
                Found::Missing | Found::Blocked(_) => None,
            },
        };
        let Some(dir) = dir else {
            // Gone since its parent was read, or not a directory any more:
            // what the base has there is not in the working copy.
            return self.gone(&path, base);
        };
        if base.is_none() && holds_marker(&dir) {
            // Another workspace or repository: not part of this one.
            return Ok(());
        }
        let cached = self.old.dir(&path);
        // Taken before the directory is listed: a change made while it is
        // listed shows in the next scan.
        let stamp = if self.whole { dir_stamp(&dir) } else { None };

        // The base's tree is read unless the directory's record may stand
        // for it: the directory was clean against the same tree.
        let whole = matches!(covered, Scope::Whole);
        let trusted = whole
            && base.is_some()
            && cached.is_some_and(|record| record.clean() && record.base() == base);
        let mut tree: Option<Vec<Entry>> = match base {
            Some(id) if !trusted => Some(read_tree(self.objects, id)?),
            _ => None,
        };

        // The record stands for the listing while the directory did not
        // change; the listing is read when it did, or when an entry the
        // record names turns out to be gone.
        let mut record_lists = cached.filter(|record| {
            self.whole && record_holds(record, base, ignored, stamp, self.old.time())
        });
        let (found, rules, ignore_files) = loop {
            let at = |name: &[u8]| match (&tree, cached) {
                (Some(entries), _) => at_tree(entries, name),
                (None, Some(record)) if trusted => at_record(&record, name),
                _ => At::Nothing,
            };
            if let Some(record) = record_lists.take() {
                let files = ignores::from_bits(record.ignore_files());
                let rules = if ignored {
                    above.clone()
                } else {
                    ignores::enter(&dir, &path, &above, &files)
                };
                // A name that the listing could not have given, as a damaged
                // or forged cache could hold, is never looked up.
                if ignores::digest(&rules) != record.rules()
                    || record
                        .files()
                        .any(|entry| !one_name(entry.name) || skipped(entry.name))
                {
                    continue;
                }
                let listed = record
                    .files()
                    .map(|entry| Listed {
                        name: entry.name.to_vec(),
                        dir: entry.kind >= KIND_GITLINK,
                    })
                    .collect();
                let found =
                    self.classify(&dir, &path, listed, &rules, ignored, &covered, &at, cached)?;
                if !found.vanished {
                    break (found, rules, record.ignore_files());
                }
                continue;
            }
            let listed = self.list(&dir, &path)?;
            let files = ignores::ignore_files(self.fs, listed.iter().map(|e| e.name.as_slice()));
            let ignore_files = ignores::bits(&files);
            let rules = if ignored {
                above.clone()
            } else {
                ignores::enter(&dir, &path, &above, &files)
            };
            let found =
                self.classify(&dir, &path, listed, &rules, ignored, &covered, &at, cached)?;
            break (found, rules, ignore_files);
        };
        let Sorted {
            counted, children, ..
        } = found;
        let counted = self.hash(&dir, &path, counted)?;

        let clean_by_record =
            trusted && cached.is_some_and(|record| matches_record(&record, &counted, &children));
        let (deltas, clean) = if clean_by_record {
            (Vec::new(), true)
        } else {
            if tree.is_none()
                && let Some(id) = base
            {
                tree = Some(read_tree(self.objects, id)?);
            }
            self.differ(
                &path,
                &covered,
                tree.as_deref().unwrap_or(&[]),
                &counted,
                &children,
            )?
        };

        let record = if self.whole {
            // Records that will vouch, or a listing that will stand, where
            // the old ones could not.
            let fresh = counted.iter().any(|file| {
                file.read && file.vouches && file.stat.is_some_and(|stat| stat.mtime < self.time)
            }) || stamp.is_some_and(|stamp| {
                !stamp.older_than(self.old.time()) && stamp.older_than(self.time)
            });
            let mut files: Vec<FileRecord> = counted.into_iter().map(Counted::record).collect();
            files.extend(children.iter().map(|child| {
                let kind = if child.base.is_some() {
                    KIND_TREE
                } else {
                    KIND_SUBDIR
                };
                FileRecord::bare(child.name.clone(), kind, child.base)
            }));
            files.sort_unstable_by(|a, b| a.name.cmp(&b.name));
            let made = DirRecord {
                path: path.clone(),
                base,
                clean,
                stamp,
                ignored,
                rules: ignores::digest(&rules),
                ignore_files,
                files,
            };
            Some(match cached {
                Some(old) if !fresh && made.same_as(&old) => (Record::Kept(old), false),
                _ => (Record::Made(made), true),
            })
        } else {
            None
        };

        if !children.is_empty() {
            let shared = (self.held.load(Ordering::Relaxed) < self.held_limit).then(|| {
                self.held.fetch_add(1, Ordering::Relaxed);
                Arc::new(Shared {
                    dir,
                    held: &self.held,
                })
            });
            for child in children {
                let open = match &shared {
                    Some(shared) => Open::Child(Arc::clone(shared), child.name),
                    None => Open::FromRoot,
                };
                let job = Job {
                    open,
                    path: child.path,
                    base: child.base,
                    rules: rules.clone(),
                    ignored: child.ignored,
                    scope: child.scope,
                };
                scope.spawn(move |s| self.run(s, job));
            }
        }

        let mut out = self.out();
        out.deltas.extend(deltas);
        if let Some((record, changed)) = record {
            out.records.push(record);
            out.changed |= changed;
            out.seen += usize::from(cached.is_some());
        }
        Ok(())
    }

    /// Sorts a directory's entries into what counts and what to read next.
    /// `at` says what the base has at a name, and `cached` is the
    /// directory's record. An entry that is gone by the time it is looked at
    /// is left out, and noted.
    #[allow(clippy::too_many_arguments)]
    fn classify(
        &self,
        dir: &Dir,
        path: &[u8],
        listed: Vec<Listed>,
        rules: &Chain,
        ignored: bool,
        covered: &Scope,
        at: &dyn Fn(&[u8]) -> At,
        cached: Option<CachedDir<'_>>,
    ) -> Result<Sorted, Error> {
        let mut found = Sorted {
            counted: Vec::with_capacity(listed.len()),
            children: Vec::new(),
            vanished: false,
        };
        for Listed { name, dir: is_dir } in listed {
            let Some(child_scope) = covered.enter(&name) else {
                continue;
            };
            let base_at = at(&name);
            let stat = if is_dir {
                None
            } else {
                let os = os_name(&name).ok_or_else(|| unrepresentable(&join(path, &name)))?;
                match stat_entry(dir, os) {
                    Ok(stat) => Some(stat),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        found.vanished = true;
                        continue;
                    }
                    Err(err) => return Err(self.root.error("read", &join(path, &name), err)),
                }
            };
            match stat {
                None
                | Some(Stat {
                    kind: StatKind::Dir,
                    ..
                }) => {
                    if base_at == At::Gitlink {
                        // A submodule's directory: its content is not ours.
                        found.counted.push(Counted {
                            name,
                            stat: None,
                            op: None,
                            vouches: false,
                            read: false,
                        });
                        continue;
                    }
                    let child_path = join(path, &name);
                    let child_ignored = ignored || ignores::is_ignored(rules, &child_path, true);
                    let child_base = match base_at {
                        At::Tree(id) => Some(id),
                        _ if child_ignored => continue,
                        _ => None,
                    };
                    found.children.push(Child {
                        name,
                        path: child_path,
                        base: child_base,
                        ignored: child_ignored,
                        scope: child_scope,
                    });
                }
                Some(
                    stat @ Stat {
                        kind: StatKind::File | StatKind::Symlink,
                        ..
                    },
                ) => {
                    let tracked = matches!(base_at, At::Leaf | At::Gitlink);
                    if !tracked
                        && (ignored || ignores::is_ignored(rules, &join(path, &name), false))
                    {
                        continue;
                    }
                    // Symlinks are cheap to read again, and their size in
                    // the metadata is not reliable everywhere.
                    let vouched = match stat.kind {
                        StatKind::File => cached
                            .and_then(|record| record.file(&name))
                            .and_then(|file| file.vouch(&stat, self.old.time())),
                        _ => None,
                    };
                    found.counted.push(Counted {
                        name,
                        stat: Some(stat),
                        op: vouched.map(|blob| op_for(&stat, blob, stat.size)),
                        vouches: vouched.is_some(),
                        read: false,
                    });
                }
                Some(_) => {}
            }
        }
        Ok(found)
    }

    /// A directory the base has, whose path holds no directory any more:
    /// everything the base has below it is gone.
    fn gone(&self, path: &[u8], base: Option<ObjectId>) -> Result<(), Error> {
        let Some(tree) = base else {
            return Ok(());
        };
        let removed = leaves(self.objects, tree, path)?;
        self.out()
            .deltas
            .extend(removed.into_iter().map(|(path, op)| Delta {
                path,
                old: Some(op),
                new: None,
            }));
        Ok(())
    }

    /// The entries of `dir`, by name, less those the walk never looks at.
    fn list(&self, dir: &Dir, path: &[u8]) -> Result<Vec<Listed>, Error> {
        #[cfg(test)]
        self.listed.fetch_add(1, Ordering::Relaxed);
        let mut listed: Vec<Listed> = list(dir)
            .map_err(|err| self.root.error("read", path, err))?
            .into_iter()
            .filter(|(name, _)| !skipped(name))
            .map(|(name, dir)| Listed { name, dir })
            .collect();
        listed.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(listed)
    }

    /// Reads the files and symlinks the cache did not vouch for, in
    /// parallel. Files that vanished since they were listed are left out.
    fn hash(&self, dir: &Dir, path: &[u8], counted: Vec<Counted>) -> Result<Vec<Counted>, Error> {
        let missing = |file: &Counted| file.stat.is_some() && file.op.is_none();
        let misses = counted.iter().filter(|file| missing(file)).count();
        if misses == 0 {
            return Ok(counted);
        }
        let read = |mut file: Counted| -> Result<Option<Counted>, Error> {
            if !missing(&file) {
                return Ok(Some(file));
            }
            let os = os_name(&file.name).ok_or_else(|| unrepresentable(&file.name))?;
            let symlink = file.stat.is_some_and(|stat| stat.kind == StatKind::Symlink);
            let result = if symlink {
                hash_symlink(dir, os)
            } else {
                match hash_file(dir, os) {
                    // Replaced by a symlink since it was listed.
                    Err(_) if dir.symlink_metadata(os).is_ok_and(|meta| meta.is_symlink()) => {
                        hash_symlink(dir, os)
                    }
                    other => other,
                }
            };
            match result {
                Ok((op, stable)) => {
                    file.vouches = stable.is_some() && !matches!(op, ChangeOp::Symlink { .. });
                    if let Some(stable) = stable {
                        file.stat = Some(stable);
                    }
                    file.op = Some(op);
                    file.read = true;
                    Ok(Some(file))
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(self.root.error("read", &join(path, &file.name), err)),
            }
        };
        let read: Vec<Option<Counted>> = if misses > 1 {
            counted
                .into_par_iter()
                .map(read)
                .collect::<Result<_, _>>()?
        } else {
            counted.into_iter().map(read).collect::<Result<_, _>>()?
        };
        Ok(read.into_iter().flatten().collect())
    }

    /// Where a directory's own entries differ from the base tree's
    /// `entries`, and whether it is clean: nothing differs, and every
    /// subdirectory of the tree is there as a directory.
    fn differ(
        &self,
        path: &[u8],
        covered: &Scope,
        entries: &[Entry],
        counted: &[Counted],
        children: &[Child],
    ) -> Result<(Vec<Delta>, bool), Error> {
        let added = |file: &Counted| {
            file.op.as_ref().map(|op| Delta {
                path: join(path, &file.name),
                old: None,
                new: Some(op.clone()),
            })
        };
        let mut deltas = Vec::new();
        let mut all_there = true;
        let mut news = counted.iter().peekable();
        for entry in entries {
            while let Some(file) = news.next_if(|file| file.name < entry.name) {
                deltas.extend(added(file));
            }
            if covered.enter(&entry.name).is_none() {
                continue;
            }
            match &entry.kind {
                EntryKind::Tree { tree } => {
                    let there = children
                        .binary_search_by(|child| child.name.as_slice().cmp(&entry.name))
                        .is_ok_and(|index| children[index].base.is_some());
                    if !there {
                        all_there = false;
                        let removed = leaves(self.objects, *tree, &join(path, &entry.name))?;
                        deltas.extend(removed.into_iter().map(|(path, op)| Delta {
                            path,
                            old: Some(op),
                            new: None,
                        }));
                    }
                }
                kind => {
                    let Some(old) = ChangeOp::from_entry(kind) else {
                        continue;
                    };
                    match news.next_if(|file| file.name == entry.name) {
                        // A gitlink whose directory is there is the base's.
                        Some(Counted { op: None, .. }) => {}
                        Some(Counted { op: Some(new), .. }) => {
                            if *new != old {
                                deltas.push(Delta {
                                    path: join(path, &entry.name),
                                    old: Some(old),
                                    new: Some(new.clone()),
                                });
                            }
                        }
                        None => deltas.push(Delta {
                            path: join(path, &entry.name),
                            old: Some(old),
                            new: None,
                        }),
                    }
                }
            }
        }
        deltas.extend(news.filter_map(added));
        let clean = deltas.is_empty() && all_there;
        Ok((deltas, clean))
    }
}

/// What the base tree's `entries` have at `name`.
fn at_tree(entries: &[Entry], name: &[u8]) -> At {
    match entries.binary_search_by(|entry| entry.name.as_slice().cmp(name)) {
        Ok(index) => match &entries[index].kind {
            EntryKind::Tree { tree } => At::Tree(*tree),
            EntryKind::Gitlink { .. } => At::Gitlink,
            _ => At::Leaf,
        },
        Err(_) => At::Nothing,
    }
}

/// What the base has at `name`, by the record of a directory that was
/// clean against the same tree: the record lists every entry of that tree.
fn at_record(record: &CachedDir<'_>, name: &[u8]) -> At {
    match record.file(name).map(|file| (file.kind, file.blob)) {
        Some((KIND_TREE, tree)) => At::Tree(tree),
        Some((KIND_GITLINK, _)) => At::Gitlink,
        Some((KIND_SUBDIR, _)) | None => At::Nothing,
        Some(_) => At::Leaf,
    }
}

/// Whether a directory's entries match, one for one, the record of a clean
/// directory: the same files, symlinks and gitlinks with the same content,
/// and the same subdirectories of the base tree, there as directories.
fn matches_record(record: &CachedDir<'_>, counted: &[Counted], children: &[Child]) -> bool {
    let mut files = counted.iter();
    let mut dirs = children.iter().filter(|child| child.base.is_some());
    for entry in record.files() {
        let matched = match entry.kind {
            KIND_SUBDIR => true,
            KIND_TREE => dirs
                .next()
                .is_some_and(|dir| dir.name == entry.name && dir.base == Some(entry.blob)),
            _ => files.next().is_some_and(|file| {
                file.name == entry.name
                    && file.kind() == entry.kind
                    && (entry.kind == KIND_GITLINK || file.blob() == Some(entry.blob))
            }),
        };
        if !matched {
            return false;
        }
    }
    files.next().is_none() && dirs.next().is_none()
}

/// Whether a directory's record may stand for its listing: the directory's
/// stamp is the recorded one, both its times strictly older than the scan
/// that recorded it, and the walk takes its entries as it did then, against
/// the same base tree and ignored or not as then. The ignore rules are
/// compared once they are read.
fn record_holds(
    record: &CachedDir<'_>,
    base: Option<ObjectId>,
    ignored: bool,
    stamp: Option<DirStamp>,
    time: (i64, u32),
) -> bool {
    stamp.is_some_and(|stamp| record.stamp() == Some(stamp) && stamp.older_than(time))
        && record.base() == base
        && record.ignored() == ignored
}

/// Whether `name` is one name, as a listing gives them: not empty, not `.`
/// or `..`, and without `/` or NUL.
fn one_name(name: &[u8]) -> bool {
    !name.is_empty() && name != b"." && name != b".." && !name.contains(&b'/') && !name.contains(&0)
}

/// Names the walk never looks at: `.braid` and `.git` and every name that
/// some file system would read as one of them, and braid's scratch files.
fn skipped(name: &[u8]) -> bool {
    name.starts_with(SCRATCH_PREFIX.as_bytes())
        || (may_be_reserved(name) && braid_format::name::is_reserved(name))
}

/// Whether `name` may alias `.braid` or `.git`, a quick test before the
/// full one. Aliases fold to `.braid`, `.git`, `braid~N` or `git~N`, and an
/// ASCII name folds by changing letter case only, after cutting it at its
/// first `:` and dropping trailing dots and spaces.
fn may_be_reserved(name: &[u8]) -> bool {
    if !name.is_ascii() {
        return true;
    }
    let end = name
        .iter()
        .position(|byte| *byte == b':')
        .unwrap_or(name.len());
    let mut stem = &name[..end];
    while let [rest @ .., b'.' | b' '] = stem {
        stem = rest;
    }
    match stem.len() {
        4 | 6 => stem[0] == b'.',
        5 | 7 => stem[stem.len() - 2] == b'~',
        _ => false,
    }
}

/// Whether the directory holds a `.braid` or `.git` entry: another workspace
/// or repository, which an untracked directory is not part of this one.
fn holds_marker(dir: &Dir) -> bool {
    [braid_repo::DIR, ".git"]
        .iter()
        .any(|marker| dir.symlink_metadata(marker).is_ok())
}

fn unrepresentable(path: &[u8]) -> Error {
    Error::Unsafe {
        path: path.to_vec(),
        reason: "this platform cannot represent the name".to_owned(),
    }
}

/// The change operation that sets a path to this file.
fn op_for(stat: &Stat, blob: ObjectId, size: u64) -> ChangeOp {
    match stat.kind {
        StatKind::Symlink => ChangeOp::Symlink { blob, size },
        _ if stat.executable => ChangeOp::Executable { blob, size },
        _ => ChangeOp::File { blob, size },
    }
}

/// What reading a file gave: the change that sets it, and its metadata if
/// it did not change while it was read.
type Hashed = (ChangeOp, Option<Stat>);

fn hash_file(dir: &Dir, name: &OsStr) -> io::Result<Hashed> {
    let mut file = open_file(dir, name)?;
    let before = Stat::of(&file.metadata()?);
    if before.kind != StatKind::File {
        return Err(io::Error::other("not a regular file"));
    }
    let mut hasher = IdHasher::new(Kind::Blob);
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let after = Stat::of(&file.metadata()?);
    let blob = hasher.finalize();
    let stable = (before == after && size == after.size).then_some(after);
    Ok((op_for(&before, blob, size), stable))
}

fn hash_symlink(dir: &Dir, name: &OsStr) -> io::Result<Hashed> {
    let target = read_link(dir, name)?;
    let blob = ObjectId::hash(Kind::Blob, &target);
    Ok((
        ChangeOp::Symlink {
            blob,
            size: target.len() as u64,
        },
        None,
    ))
}

/// The system clock less two seconds, for when the file system's own clock
/// cannot be read: older than any modification a coarse file system clock
/// could still date to the same instant.
fn fallback_now() -> (i64, u32) {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(Duration::from_secs(2));
    (since.as_secs() as i64, since.subsec_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};
    use std::path::Path;

    use braid_engine::EMPTY_TREE;
    use braid_format::{Change, MemoryObjects};

    use crate::cache;

    /// Objects in memory, counting the directories read.
    #[derive(Default)]
    struct Counting {
        objects: MemoryObjects,
        trees: AtomicUsize,
    }

    impl Objects for Counting {
        type Error = std::convert::Infallible;

        fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Self::Error> {
            let found = self.objects.get(id)?;
            if matches!(found, Some((Kind::Tree, _))) {
                self.trees.fetch_add(1, Ordering::Relaxed);
            }
            Ok(found)
        }

        fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Self::Error> {
            self.objects.put(kind, payload)
        }
    }

    /// A working copy with a `.braid` file, as every workspace has, a
    /// directory for its stat cache, and objects for its base.
    struct Fixture {
        dir: tempfile::TempDir,
        state: tempfile::TempDir,
        root: Root,
        objects: Counting,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(braid_repo::DIR), b"braid-workspace 1\n").unwrap();
            let root = Root::open(dir.path()).unwrap();
            Fixture {
                dir,
                state: tempfile::tempdir().unwrap(),
                root,
                objects: Counting::default(),
            }
        }

        fn at(&self, path: &str) -> std::path::PathBuf {
            self.dir.path().join(path)
        }

        fn file(&self, content: &str) -> ChangeOp {
            ChangeOp::File {
                blob: self.objects.put(Kind::Blob, content.as_bytes()).unwrap(),
                size: content.len() as u64,
            }
        }

        /// A base tree with these files, also written into the working copy
        /// and dated in the past, as a checkout leaves them.
        fn base(&self, files: &[(&str, &str)]) -> ObjectId {
            let changes: Vec<Change> = files
                .iter()
                .map(|(path, content)| {
                    let full = self.at(path);
                    fs::create_dir_all(full.parent().unwrap()).unwrap();
                    fs::write(&full, content).unwrap();
                    set_mtime(&full, past());
                    Change {
                        path: path.as_bytes().to_vec(),
                        op: self.file(content),
                    }
                })
                .collect();
            braid_engine::apply(&self.objects, EMPTY_TREE, &changes).unwrap()
        }

        /// Scans against `base`, with the stat cache of the previous scan,
        /// and saves the new one. Also returns whether it was saved, and how
        /// many directories of the base were read.
        fn scan_in(
            &self,
            base: ObjectId,
            scope: &[&str],
            held: usize,
        ) -> (Vec<Delta>, bool, usize) {
            let (deltas, saved, reads, _) = self.scan_counting(base, scope, held);
            (deltas, saved, reads)
        }

        /// [`Fixture::scan_in`], also returning how many directories were
        /// listed.
        fn scan_counting(
            &self,
            base: ObjectId,
            scope: &[&str],
            held: usize,
        ) -> (Vec<Delta>, bool, usize, usize) {
            let old = Cache::load(self.state.path());
            let scope: Vec<Vec<u8>> = scope.iter().map(|path| path.as_bytes().to_vec()).collect();
            self.objects.trees.store(0, Ordering::Relaxed);
            let found = scan_holding(
                &self.root,
                &self.objects,
                base,
                &old,
                FileSystem::default(),
                &scope,
                held,
            )
            .unwrap();
            let saved = found.cache.as_ref().is_some_and(|next| next.changed);
            if let Some(next) = found.cache.as_ref().filter(|next| next.changed) {
                cache::save(self.state.path(), next.time, &next.records).unwrap();
            }
            let reads = self.objects.trees.load(Ordering::Relaxed);
            (found.deltas, saved, reads, found.listed)
        }

        fn scan(&self, base: ObjectId) -> (Vec<Delta>, bool, usize) {
            self.scan_in(base, &[], HELD_HANDLES)
        }

        /// Waits until the file system's clock has moved on from every
        /// change made so far: records the next scan makes of them are then
        /// not racy, whatever the clock's granularity.
        fn settle(&self) {
            let then = self.root.file_system_now().unwrap();
            while self.root.file_system_now().unwrap() <= then {
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// A whole scan: how many directories it listed.
        fn listings(&self, base: ObjectId) -> (Vec<Delta>, usize) {
            let (deltas, _, _, listed) = self.scan_counting(base, &[], HELD_HANDLES);
            (deltas, listed)
        }
    }

    fn past() -> SystemTime {
        SystemTime::now() - Duration::from_secs(30)
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    fn blob(content: &str) -> ChangeOp {
        ChangeOp::File {
            blob: ObjectId::hash(Kind::Blob, content.as_bytes()),
            size: content.len() as u64,
        }
    }

    /// The deltas as `path old>new`, with `-` for nothing and `f` for a
    /// file, `l` for a symlink and `g` for a gitlink.
    fn show(deltas: &[Delta]) -> Vec<String> {
        let kind = |op: &Option<ChangeOp>| match op {
            None => "-",
            Some(ChangeOp::Symlink { .. }) => "l",
            Some(ChangeOp::Gitlink { .. }) => "g",
            Some(_) => "f",
        };
        deltas
            .iter()
            .map(|delta| {
                let path = String::from_utf8_lossy(&delta.path);
                format!("{path} {}>{}", kind(&delta.old), kind(&delta.new))
            })
            .collect()
    }

    #[test]
    fn unchanged_files_are_not_read_again() {
        let f = Fixture::new();
        let path = f.at("a.txt");
        fs::write(&path, "one").unwrap();
        set_mtime(&path, past());
        let (first, _, _) = f.scan(EMPTY_TREE);
        assert_eq!(first[0].new, Some(blob("one")));

        // Rewrite the content in place behind the cache's back: same inode,
        // same size, same modification time. The cache vouches for it, so
        // the file is not read, and the old blob is what the scan reports.
        let later = past() + Duration::from_secs(1);
        let rewrite = |content: &[u8]| {
            let mut file = fs::File::options().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(content).unwrap();
            drop(file);
            set_mtime(&path, later);
        };
        rewrite(b"two");
        let (second, _, _) = f.scan(EMPTY_TREE);
        assert_eq!(second[0].new, Some(blob("two")), "the time changed");
        rewrite(b"one");
        let (third, _, _) = f.scan(EMPTY_TREE);
        assert_eq!(third[0].new, Some(blob("two")), "vouched for by the cache");

        // A file whose time is not strictly older than the cache's scan is
        // read again, whatever the rest of its metadata says.
        let (seconds, nanos) = Cache::load(f.state.path()).time();
        set_mtime(&path, UNIX_EPOCH + Duration::new(seconds as u64, nanos));
        let (fourth, _, _) = f.scan(EMPTY_TREE);
        assert_eq!(fourth[0].new, Some(blob("one")));
    }

    #[test]
    fn what_the_walk_skips() {
        let f = Fixture::new();
        let base = f.base(&[
            ("kept.log", "old"),
            ("build/sub/tracked.txt", "tracked"),
            (".gitignore", "*.log\nbuild/\n"),
        ]);
        let write = |path: &str, content: &str| {
            fs::create_dir_all(f.at(path).parent().unwrap()).unwrap();
            fs::write(f.at(path), content).unwrap();
        };
        write("app.log", "ignored");
        write("kept.log", "tracked, so not ignored");
        write("build/out.o", "ignored");
        write("build/new/file", "ignored");
        write(".git/config", "skipped");
        write(".braid-tmp-0123", "scratch");
        write("nested/.braid", "another workspace");
        write("nested/file", "not ours");
        write("repo/.git/HEAD", "another repository");
        write("src.rs", "code");

        let (deltas, _, _) = f.scan(base);
        assert_eq!(show(&deltas), ["kept.log f>f", "src.rs ->f"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_read_not_followed() {
        let f = Fixture::new();
        let base = f.base(&[("dir/x", "x")]);
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("x"), "outside").unwrap();
        fs::remove_dir_all(f.at("dir")).unwrap();
        std::os::unix::fs::symlink(outside.path(), f.at("dir")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("x"), f.at("link")).unwrap();
        let (deltas, _, _) = f.scan(base);
        assert_eq!(show(&deltas), ["dir ->l", "dir/x f>-", "link ->l"]);
        let target = outside.path().as_os_str().as_encoded_bytes().to_vec();
        assert_eq!(
            deltas[0].new,
            Some(ChangeOp::Symlink {
                blob: ObjectId::hash(Kind::Blob, &target),
                size: target.len() as u64,
            })
        );
    }

    #[test]
    fn clean_directories_are_not_compared_again() {
        let f = Fixture::new();
        let base = f.base(&[("a/x", "1"), ("a/y", "2"), ("b/z", "3"), ("top", "4")]);
        f.settle();
        let (deltas, saved, reads) = f.scan(base);
        assert!(deltas.is_empty());
        assert!(saved);
        assert_eq!(reads, 3, "every directory of the base, once");

        // Nothing changed: no directory of the base is read, and the cache
        // is left as it is.
        let (deltas, saved, reads) = f.scan(base);
        assert!(deltas.is_empty());
        assert_eq!((saved, reads), (false, 0));

        // An edit in one directory reads that directory's tree only.
        fs::write(f.at("a/x"), "edited").unwrap();
        f.settle();
        let (deltas, saved, reads) = f.scan(base);
        assert_eq!(show(&deltas), ["a/x f>f"]);
        assert_eq!((saved, reads), (true, 1));
        let (deltas, saved, reads) = f.scan(base);
        assert_eq!(show(&deltas), ["a/x f>f"]);
        assert_eq!((saved, reads), (false, 1));
    }

    #[test]
    fn entries_added_or_removed_in_a_clean_directory_are_seen() {
        let f = Fixture::new();
        let base = f.base(&[("a/x", "1"), ("a/y", "2"), ("b/z", "3")]);
        f.scan(base);
        let a_time = fs::metadata(f.at("a")).unwrap().modified().unwrap();
        let keep_time = |path: &str| {
            fs::File::open(f.at(path))
                .unwrap()
                .set_modified(a_time)
                .unwrap()
        };

        // A file added, the directory's time set back as a coarse clock
        // would leave it.
        fs::write(f.at("a/new"), "new").unwrap();
        keep_time("a");
        let (deltas, _, _) = f.scan(base);
        assert_eq!(show(&deltas), ["a/new ->f"]);

        // A file removed, the same way.
        fs::remove_file(f.at("a/y")).unwrap();
        keep_time("a");
        let (deltas, _, _) = f.scan(base);
        assert_eq!(show(&deltas), ["a/new ->f", "a/y f>-"]);

        // Back as it was: clean again.
        fs::remove_file(f.at("a/new")).unwrap();
        fs::write(f.at("a/y"), "2").unwrap();
        keep_time("a");
        let (deltas, _, _) = f.scan(base);
        assert!(deltas.is_empty());

        // A directory of the base replaced by a file, and a file by a
        // directory.
        fs::remove_dir_all(f.at("b")).unwrap();
        fs::write(f.at("b"), "now a file").unwrap();
        fs::remove_file(f.at("a/x")).unwrap();
        fs::create_dir(f.at("a/x")).unwrap();
        fs::write(f.at("a/x/inside"), "i").unwrap();
        let (deltas, _, _) = f.scan(base);
        assert_eq!(
            show(&deltas),
            ["a/x f>-", "a/x/inside ->f", "b ->f", "b/z f>-"]
        );
    }

    #[test]
    fn a_new_base_reads_only_the_directories_that_changed() {
        let f = Fixture::new();
        let first = f.base(&[("a/x", "1"), ("b/z", "3"), ("c/d/e", "5")]);
        f.scan(first);
        // The same working copy against a base where a/x and c/d/e differ.
        let second = braid_engine::apply(
            &f.objects,
            first,
            &[
                Change {
                    path: b"a/x".to_vec(),
                    op: f.file("one"),
                },
                Change {
                    path: b"c/d/e".to_vec(),
                    op: f.file("five"),
                },
            ],
        )
        .unwrap();
        let (deltas, saved, reads) = f.scan(second);
        assert_eq!(show(&deltas), ["a/x f>f", "c/d/e f>f"]);
        // The root, a, c and c/d: not b.
        assert_eq!((saved, reads), (true, 4));
        assert_eq!(deltas[0].old, Some(blob("one")));
        assert_eq!(deltas[0].new, Some(blob("1")));
    }

    #[test]
    fn scans_of_some_paths() {
        let f = Fixture::new();
        let base = f.base(&[("a/x", "1"), ("a/y", "2"), ("c/d/e", "3"), ("top", "4")]);
        f.scan(base);
        fs::write(f.at("a/x"), "edited").unwrap();
        fs::write(f.at("a/y"), "edited").unwrap();
        fs::write(f.at("new"), "new").unwrap();
        fs::remove_dir_all(f.at("c")).unwrap();
        let (deltas, saved, _) = f.scan_in(base, &["a/x", "c/d", "nothing/here"], HELD_HANDLES);
        assert_eq!(show(&deltas), ["a/x f>f", "c/d/e f>-"]);
        assert!(!saved, "a scan of some paths keeps the cache as it is");
        let (deltas, _, _) = f.scan_in(base, &["a"], HELD_HANDLES);
        assert_eq!(show(&deltas), ["a/x f>f", "a/y f>f"]);
        // The root is the whole working copy.
        let (deltas, saved, _) = f.scan_in(base, &["", "a"], HELD_HANDLES);
        assert_eq!(
            show(&deltas),
            ["a/x f>f", "a/y f>f", "c/d/e f>-", "new ->f"]
        );
        assert!(saved);
    }

    #[test]
    fn gitlinks_are_there_or_not() {
        let f = Fixture::new();
        let gitlink = ChangeOp::Gitlink {
            commit: braid_format::GitId::Sha1([7; 20]),
        };
        let base = braid_engine::apply(
            &f.objects,
            EMPTY_TREE,
            &[
                Change {
                    path: b"vendor/lib".to_vec(),
                    op: gitlink.clone(),
                },
                Change {
                    path: b"vendor/README".to_vec(),
                    op: f.file("r"),
                },
            ],
        )
        .unwrap();
        fs::create_dir_all(f.at("vendor/lib")).unwrap();
        fs::write(f.at("vendor/lib/inside"), "not ours").unwrap();
        fs::write(f.at("vendor/README"), "r").unwrap();
        for _ in 0..2 {
            let (deltas, _, _) = f.scan(base);
            assert!(deltas.is_empty(), "{:?}", show(&deltas));
        }
        fs::remove_dir_all(f.at("vendor/lib")).unwrap();
        let (deltas, _, _) = f.scan(base);
        assert_eq!(show(&deltas), ["vendor/lib g>-"]);
        fs::write(f.at("vendor/lib"), "a file").unwrap();
        let (deltas, _, _) = f.scan(base);
        assert_eq!(show(&deltas), ["vendor/lib g>f"]);
    }

    #[test]
    fn directories_reopened_from_the_root_read_the_same() {
        let f = Fixture::new();
        let base = f.base(&[("a/b/c/d", "1"), ("a/b/e", "2"), ("f/g", "3"), ("h", "4")]);
        fs::write(f.at("a/b/c/d"), "edited").unwrap();
        fs::write(f.at("f/new"), "new").unwrap();
        fs::remove_file(f.at("h")).unwrap();
        let (held, _, _) = f.scan_in(base, &[], HELD_HANDLES);
        let (reopened, _, _) = f.scan_in(base, &[], 0);
        assert_eq!(show(&held), ["a/b/c/d f>f", "f/new ->f", "h f>-"]);
        assert_eq!(held, reopened);
    }

    // Directories have a stamp only where the system gives one.
    #[cfg(unix)]
    #[test]
    fn unchanged_directories_are_not_listed_again() {
        let f = Fixture::new();
        let base = f.base(&[("a/x", "1"), ("a/b/y", "2"), ("c/z", "3")]);
        fs::create_dir(f.at("untracked")).unwrap();
        fs::write(f.at("untracked/u"), "u").unwrap();
        f.settle();
        let (deltas, listed) = f.listings(base);
        assert_eq!(show(&deltas), ["untracked/u ->f"]);
        assert_eq!(listed, 5);
        let (deltas, listed) = f.listings(base);
        assert_eq!(show(&deltas), ["untracked/u ->f"]);
        assert_eq!(listed, 0, "every directory is as its record says");

        // A file edited in place changes no directory: it is found by its
        // metadata, and nothing is listed.
        fs::write(f.at("a/b/y"), "edited").unwrap();
        let (deltas, listed) = f.listings(base);
        assert_eq!(show(&deltas), ["a/b/y f>f", "untracked/u ->f"]);
        assert_eq!(listed, 0);

        // A new file is a new entry: its directory alone is listed again.
        fs::write(f.at("c/new"), "n").unwrap();
        f.settle();
        let (deltas, listed) = f.listings(base);
        assert_eq!(show(&deltas), ["a/b/y f>f", "c/new ->f", "untracked/u ->f"]);
        assert_eq!(listed, 1);
        fs::remove_file(f.at("untracked/u")).unwrap();
        f.settle();
        let (deltas, listed) = f.listings(base);
        assert_eq!(show(&deltas), ["a/b/y f>f", "c/new ->f"]);
        assert_eq!(listed, 1);
    }

    // Directories have a stamp only where the system gives one.
    #[cfg(unix)]
    #[test]
    fn a_listing_stands_only_while_the_rules_and_the_base_do() {
        let f = Fixture::new();
        let base = f.base(&[(".gitignore", "*.log\n"), ("d/kept", "k")]);
        fs::write(f.at("d/app.log"), "ignored").unwrap();
        f.settle();
        let (deltas, _) = f.listings(base);
        assert!(deltas.is_empty(), "{:?}", show(&deltas));
        let (_, listed) = f.listings(base);
        assert_eq!(listed, 0);

        // Rules edited in place: no directory changed, but what is ignored
        // did, below too.
        fs::write(f.at(".gitignore"), "*.tmp\n").unwrap();
        let (deltas, listed) = f.listings(base);
        assert_eq!(show(&deltas), [".gitignore f>f", "d/app.log ->f"]);
        assert_eq!(listed, 2);
        fs::write(f.at(".gitignore"), "*.log\n").unwrap();
        let (deltas, _) = f.listings(base);
        assert!(deltas.is_empty(), "{:?}", show(&deltas));
        let (_, listed) = f.listings(base);
        assert_eq!(listed, 0);

        // A base that tracks the ignored file: the walk must count it now,
        // though its directory did not change.
        let tracked = braid_engine::apply(
            &f.objects,
            base,
            &[Change {
                path: b"d/app.log".to_vec(),
                op: f.file("tracked"),
            }],
        )
        .unwrap();
        let (deltas, _) = f.listings(tracked);
        assert_eq!(show(&deltas), ["d/app.log f>f"]);
        assert_eq!(deltas[0].new, Some(blob("ignored")));
    }

    #[test]
    fn a_listing_stands_only_while_the_stamp_is_older_than_the_scan() {
        let stamp = DirStamp {
            mtime: (100, 5),
            ctime: (100, 6),
            ino: 7,
        };
        let state = tempfile::tempdir().unwrap();
        let record = |stamp: DirStamp, time: (i64, u32)| {
            let dir = DirRecord {
                path: b"d".to_vec(),
                base: None,
                clean: false,
                stamp: Some(stamp),
                ignored: false,
                rules: 0,
                ignore_files: 0,
                files: Vec::new(),
            };
            cache::save(state.path(), time, &[Record::Made(dir)]).unwrap();
            Cache::load(state.path())
        };
        let holds = |cache: &Cache, now: DirStamp| {
            let dir = cache.dir(b"d").unwrap();
            record_holds(&dir, None, false, Some(now), cache.time())
        };
        let older = record(stamp, (100, 7));
        assert!(holds(&older, stamp));
        // Another inode, or either time changed.
        assert!(!holds(&older, DirStamp { ino: 8, ..stamp }));
        assert!(!holds(
            &older,
            DirStamp {
                mtime: (100, 4),
                ..stamp
            }
        ));
        assert!(!holds(
            &older,
            DirStamp {
                ctime: (101, 0),
                ..stamp
            }
        ));
        // Changed in the same instant as the scan that recorded it, as a
        // coarse clock would date it: the listing is read again.
        let racy = record(stamp, (100, 6));
        assert!(!holds(&racy, stamp));
        let racy = record(stamp, (100, 5));
        assert!(!holds(&racy, stamp));
        // Ignored, or with a base, where it was not.
        let dir = older.dir(b"d").unwrap();
        assert!(!record_holds(&dir, None, true, Some(stamp), older.time()));
        let tree = Some(ObjectId::hash(Kind::Tree, b"t"));
        assert!(!record_holds(&dir, tree, false, Some(stamp), older.time()));
        // Where the system gives no stamp, the directory is always listed.
        assert!(!record_holds(&dir, None, false, None, older.time()));
    }

    /// A commit of `changes` on `base`, as `braid commit` makes one from a
    /// scan: what it changed, and the new base's tree.
    fn commit(f: &Fixture, base: ObjectId, changes: &[&Delta]) -> (Vec<Vec<u8>>, ObjectId) {
        let changes: Vec<Change> = changes
            .iter()
            .map(|delta| Change {
                path: delta.path.clone(),
                op: delta.new.clone().unwrap_or(ChangeOp::Delete),
            })
            .collect();
        let tree = braid_engine::apply(&f.objects, base, &changes).unwrap();
        (
            changes.into_iter().map(|change| change.path).collect(),
            tree,
        )
    }

    /// Scans, commits the deltas `keep` accepts, and saves the scan's
    /// records brought onto the new base. Returns the new base's tree.
    fn scan_and_commit(f: &Fixture, base: ObjectId, keep: impl Fn(&Delta) -> bool) -> ObjectId {
        let old = Cache::load(f.state.path());
        let found = scan(&f.root, &f.objects, base, &old, FileSystem::default(), &[]).unwrap();
        let kept: Vec<&Delta> = found.deltas.iter().filter(|delta| keep(delta)).collect();
        let (paths, tree) = commit(f, base, &kept);
        let next = found.cache.unwrap();
        let records = follow(&f.objects, next.records, tree, &paths).unwrap();
        cache::save(f.state.path(), next.time, &records).unwrap();
        tree
    }

    // Directories have a stamp only where the system gives one.
    #[cfg(unix)]
    #[test]
    fn a_scan_brought_onto_its_commit_leaves_nothing_to_do() {
        let f = Fixture::new();
        let base = f.base(&[("a/x", "1"), ("a/y", "2"), ("c/d/e", "3"), ("top", "4")]);
        fs::write(f.at("a/x"), "edited").unwrap();
        fs::create_dir_all(f.at("new/dir")).unwrap();
        fs::write(f.at("new/dir/f"), "f").unwrap();
        fs::remove_dir_all(f.at("c")).unwrap();
        set_mtime(&f.at("a/x"), past());
        set_mtime(&f.at("new/dir/f"), past());
        f.settle();
        let committed = scan_and_commit(&f, base, |_| true);
        let (deltas, saved, reads, listed) = f.scan_counting(committed, &[], HELD_HANDLES);
        assert!(deltas.is_empty(), "{:?}", show(&deltas));
        assert_eq!((saved, reads, listed), (false, 0, 0));
    }

    #[cfg(unix)]
    #[test]
    fn a_scan_brought_onto_a_commit_of_some_paths() {
        let f = Fixture::new();
        let base = f.base(&[("a/x", "1"), ("a/y", "2"), ("c/d/e", "3"), ("top", "4")]);
        fs::write(f.at("a/x"), "edited").unwrap();
        fs::create_dir_all(f.at("new/dir")).unwrap();
        fs::write(f.at("new/dir/f"), "f").unwrap();
        fs::remove_dir_all(f.at("c")).unwrap();
        set_mtime(&f.at("a/x"), past());
        set_mtime(&f.at("new/dir/f"), past());
        f.settle();
        let committed = scan_and_commit(&f, base, |delta| delta.path == b"a/x");
        let (deltas, saved, reads, listed) = f.scan_counting(committed, &[], HELD_HANDLES);
        assert_eq!(show(&deltas), ["c/d/e f>-", "new/dir/f ->f"]);
        // The root's tree, to find what went missing, and the trees below
        // it: a is as the commit left it.
        assert_eq!((saved, reads, listed), (false, 3, 0));
    }

    #[test]
    fn names_are_single_names() {
        for name in ["a", "a.b", "...", " "] {
            assert!(one_name(name.as_bytes()), "{name}");
        }
        for name in ["", ".", "..", "a/b", "/", "a\0"] {
            assert!(!one_name(name.as_bytes()), "{name:?}");
        }
    }

    #[test]
    fn quick_test_for_reserved_names() {
        for name in [
            ".git",
            ".braid",
            ".GIT",
            "Git~1",
            "braid~2",
            ".git.",
            ".git::$INDEX_ALLOCATION",
        ] {
            assert!(may_be_reserved(name.as_bytes()), "{name}");
            assert!(skipped(name.as_bytes()), "{name}");
        }
        for name in ["f00.txt", "src", "main.rs", ".github", "README.md"] {
            assert!(!skipped(name.as_bytes()), "{name}");
        }
        assert!(skipped(".g\u{200c}it".as_bytes()));
        assert!(skipped(b".braid-tmp-0123"));
    }
}
