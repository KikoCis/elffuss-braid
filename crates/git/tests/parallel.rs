// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The parallel import makes what the one-thread import makes: the same
//! objects, the same map, the same report, and the same writes to the store,
//! each hint given while the object it names is stored, or not, as it was
//! then. It fails the same way too, leaves the same map when it does, and
//! never leaves a map naming an object that was not stored.

mod common;

use std::collections::BTreeMap;
use std::io;
use std::sync::{Mutex, PoisonError};

use braid_format::{Kind, MemoryObjects, ObjectId, Objects};
use braid_git::{Error, GitMap, GitOid, ImportOptions, ImportReport, import, import_parallel};
use common::{Entry, Item, Repo, exec, file, link, raw_tree, submodule};

/// The thread counts the parallel import is compared at.
const THREADS: [usize; 3] = [1, 2, 8];

/// One write to the store: what was written, and the hint it came with, if
/// any, with whether the object the hint names was stored already.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Write {
    kind: Kind,
    id: ObjectId,
    hint: Option<(ObjectId, bool)>,
}

/// Objects in memory that log every write, and can be told to refuse to
/// store an object, or to lose one once stored.
#[derive(Default)]
struct Logged {
    objects: MemoryObjects,
    log: Mutex<Vec<Write>>,
    refused: Mutex<Option<ObjectId>>,
    lost: Mutex<Option<ObjectId>>,
}

impl Logged {
    fn write(
        &self,
        kind: Kind,
        payload: &[u8],
        similar: Option<&ObjectId>,
    ) -> Result<ObjectId, io::Error> {
        let id = ObjectId::hash(kind, payload);
        if *lock(&self.refused) == Some(id) {
            return Err(io::Error::other("this store refuses that object"));
        }
        let hint = similar.map(|similar| {
            let Ok(present) = self.objects.contains(similar);
            (*similar, present)
        });
        let Ok(stored) = self.objects.put(kind, payload);
        lock(&self.log).push(Write {
            kind,
            id: stored,
            hint,
        });
        Ok(stored)
    }

    /// The first write of each object so far, in a canonical order: writing
    /// an object again does nothing, and the parallel import does not repeat
    /// a write its batch already makes.
    fn writes(&self) -> Vec<Write> {
        let mut seen = std::collections::HashSet::new();
        let mut writes: Vec<Write> = lock(&self.log)
            .iter()
            .filter(|write| seen.insert(write.id))
            .copied()
            .collect();
        writes.sort();
        writes
    }

    /// Every object held, with its kind and payload.
    fn contents(&self) -> BTreeMap<ObjectId, (Kind, Vec<u8>)> {
        self.objects
            .ids()
            .into_iter()
            .map(|id| {
                let Ok(Some(found)) = self.objects.get(&id) else {
                    unreachable!("listed objects are held")
                };
                (id, found)
            })
            .collect()
    }
}

impl Objects for Logged {
    type Error = io::Error;

    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Self::Error> {
        if *lock(&self.lost) == Some(*id) {
            return Ok(None);
        }
        let Ok(found) = self.objects.get(id);
        Ok(found)
    }

    fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Self::Error> {
        self.write(kind, payload, None)
    }

    fn put_similar(
        &self,
        kind: Kind,
        payload: &[u8],
        similar: &ObjectId,
    ) -> Result<ObjectId, Self::Error> {
        self.write(kind, payload, Some(similar))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Imports with one thread (`None`) or on that many threads.
fn run(
    threads: Option<usize>,
    git: &Repo,
    objects: &Logged,
    map: &mut GitMap,
    options: &ImportOptions,
) -> Result<ImportReport, Error> {
    match threads {
        None => import(git.path(), objects, map, options),
        Some(threads) => import_parallel(git.path(), objects, map, options, threads),
    }
}

/// Fifty lines, one of which says `version`, so that versions are close.
fn text(version: usize) -> String {
    (0..50)
        .map(|n| format!("line {n} of {}\n", if n == 25 { version } else { 0 }))
        .collect()
}

/// A file just large enough that the parallel import reads it twice rather
/// than keep it until it is stored.
fn large(version: usize) -> Vec<u8> {
    let mut content = text(version).repeat((4 << 20) / 1000 + 1).into_bytes();
    content.truncate((4 << 20) + 1);
    content
}

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Commits made by one `git fast-import`, to build a long history quickly.
/// Each commit holds exactly the entries it is given, as a snapshot does.
#[derive(Default)]
struct FastImport {
    stream: Vec<u8>,
    marks: usize,
}

impl FastImport {
    fn mark(&mut self) -> usize {
        self.marks += 1;
        self.marks
    }

    fn line(&mut self, line: &str) {
        self.stream.extend_from_slice(line.as_bytes());
        self.stream.push(b'\n');
    }

    fn data(&mut self, content: &[u8]) {
        self.line(&format!("data {}", content.len()));
        self.stream.extend_from_slice(content);
        self.stream.push(b'\n');
    }

    /// A blob, by its mark.
    fn blob(&mut self, content: &[u8]) -> String {
        let mark = self.mark();
        self.line("blob");
        self.line(&format!("mark :{mark}"));
        self.data(content);
        format!(":{mark}")
    }

    /// A commit on `branch` after `parents`, by their marks; returns its own.
    fn commit(
        &mut self,
        branch: &str,
        parents: &[usize],
        entries: &[Entry],
        message: &str,
    ) -> usize {
        let mut files = Vec::new();
        for (path, item) in entries {
            let (mode, data) = match item {
                Item::File(content) => ("100644", self.blob(content)),
                Item::Exec(content) => ("100755", self.blob(content)),
                Item::Link(target) => ("120000", self.blob(target)),
                Item::Submodule(commit) => ("160000", commit.clone()),
            };
            files.push(format!("M {mode} {data} {}", String::from_utf8_lossy(path)));
        }
        let mark = self.mark();
        let when = 1_700_000_000 + 60 * mark;
        self.line(&format!("commit refs/heads/{branch}"));
        self.line(&format!("mark :{mark}"));
        self.line(&format!(
            "author Ada Lovelace <ada@example.org> {when} +0100"
        ));
        self.line(&format!(
            "committer Charles Babbage <charles@example.org> {when} +0100"
        ));
        self.data(message.as_bytes());
        for (at, parent) in parents.iter().enumerate() {
            self.line(&format!(
                "{} :{parent}",
                if at == 0 { "from" } else { "merge" }
            ));
        }
        self.line("deleteall");
        for file in files {
            self.line(&file);
        }
        mark
    }

    /// An annotated tag on a commit, by its mark.
    fn tag(&mut self, name: &str, commit: usize, message: &str) {
        self.line(&format!("tag {name}"));
        self.line(&format!("from :{commit}"));
        self.line("tagger Ada Lovelace <ada@example.org> 1700000000 +0100");
        self.data(message.as_bytes());
    }
}

/// What commit `i` of the history holds: files edited in every commit, every
/// few commits, or once in a while; deep directories; a directory that comes
/// and goes, and one that grows; the same content at two paths; an
/// executable, a symlink, a submodule; and for a few commits a file too large
/// to keep in memory.
fn files(i: usize) -> Vec<Entry> {
    let mut entries = vec![
        file("hot.txt", text(i)),
        file("src/lib.rs", text(i / 3)),
        file("src/deep/a/b/c.txt", text(i / 10)),
        exec("run.sh", format!("#!/bin/sh\necho {}\n", i / 7)),
        link("latest", format!("v{}", i / 5)),
        file("same-1.txt", text(i / 9)),
        file("same-2.txt", text(i / 9)),
    ];
    for n in 0..i / 25 {
        entries.push(file(format!("grow/f{n}.txt"), text(n)));
    }
    if (i / 20).is_multiple_of(2) {
        entries.push(file("toggle/x.txt", "comes and goes\n"));
    }
    if i >= 90 {
        entries.push(submodule("vendor", &format!("{:040x}", 0x1000 + i / 30)));
    }
    if (60..63).contains(&i) {
        entries.push(file("large.bin", large(i)));
    }
    entries
}

/// A history of more than two batches of the parallel import, made of
/// [`files`], with a side branch merged twice; a commit whose trees are not
/// canonical (an odd mode, a duplicate name, empty directories) and two
/// after it; and annotated tags, one on another, one on a tree that no
/// commit holds and one on a blob, with a plain tag on another blob.
fn history() -> Repo {
    let mut git = Repo::new();
    let mut stream = FastImport::default();
    let mut main: Option<usize> = None;
    for i in 0..140 {
        let entries = files(i);
        let head = match (i, main) {
            (40 | 110, Some(base)) => {
                let mut side = base;
                for n in 0..3 {
                    let mut entries = entries.clone();
                    entries.push(file("side.txt", text(i + n)));
                    side = stream.commit("side", &[side], &entries, &format!("side {i} {n}"));
                }
                let mut entries = entries.clone();
                entries.push(file("side.txt", text(i + 2)));
                stream.commit("main", &[base, side], &entries, &format!("merge {i}"))
            }
            _ => {
                let parents: Vec<usize> = main.into_iter().collect();
                stream.commit("main", &parents, &entries, &format!("commit {i}"))
            }
        };
        main = Some(head);
        if i == 50 {
            stream.tag("v1", head, "release 1");
        }
    }
    stream.line("done");
    git.git_input(&["fast-import", "--quiet", "--done"], &stream.stream);

    // A root that is not canonical, beside the usual files, then two more
    // commits as usual.
    let usual = git.tree(&files(140));
    let x = git.blob(b"x\n");
    let y = git.blob(b"y\n");
    let odd = git.literally(
        "tree",
        &raw_tree(&[
            ("100664", b"group-writable", &x),
            ("100644", b"twice", &x),
            ("100644", b"twice", &y),
        ]),
    );
    let only_empty = git.mktree(&[("40000", "tree", EMPTY_TREE, b"empty")]);
    let root = git.mktree(&[
        ("40000", "tree", &usual, b"usual"),
        ("40000", "tree", &odd, b"odd"),
        ("40000", "tree", EMPTY_TREE, b"empty"),
        ("40000", "tree", &only_empty, b"only-empty"),
    ]);
    let parent = git.head("main").unwrap();
    let commit = git.commit(&root, &[&parent], "odd trees");
    git.set_ref("refs/heads/main", &commit);
    for i in 141..143 {
        git.snapshot("main", &files(i), &format!("commit {i}"));
    }

    let release = git.git(&["rev-parse", "refs/tags/v1"]);
    git.annotated_tag("v1-on-v1", &release, "a tag on a tag");
    let loose_tree = git.tree(&[
        file("only-here/a.txt", "a tree no commit holds\n"),
        file("only-here/b.txt", text(1000)),
    ]);
    git.annotated_tag("tree-tag", &loose_tree, "a tree");
    let tagged_blob = git.blob(b"a blob a tag points to\n");
    git.annotated_tag("blob-tag", &tagged_blob, "a blob");
    let plain_blob = git.blob(b"a blob a plain tag points to\n");
    git.set_ref("refs/tags/plain-blob", &plain_blob);
    git
}

/// Imports `git` with one thread and in parallel at every count of
/// [`THREADS`], each into new objects and a new map, and checks that they
/// all agree. Returns what the one-thread import made.
fn assert_same(git: &Repo, options: &ImportOptions) -> (Logged, GitMap, ImportReport) {
    let one = Logged::default();
    let mut one_map = GitMap::new();
    let one_report = run(None, git, &one, &mut one_map, options).unwrap();
    for threads in THREADS {
        let many = Logged::default();
        let mut many_map = GitMap::new();
        let report = run(Some(threads), git, &many, &mut many_map, options).unwrap();
        assert_eq!(report, one_report, "{threads} threads");
        assert_eq!(many_map.to_bytes(), one_map.to_bytes(), "{threads} threads");
        assert!(many_map == one_map, "{threads} threads: the maps differ");
        assert!(
            many.contents() == one.contents(),
            "{threads} threads: the objects differ"
        );
        assert_eq!(many.writes(), one.writes(), "{threads} threads");
    }
    (one, one_map, one_report)
}

/// Every git object reachable from a ref.
fn git_objects(git: &Repo) -> Vec<GitOid> {
    git.git(&["rev-list", "--objects", "--all"])
        .lines()
        .map(|line| line.split(' ').next().unwrap().parse().unwrap())
        .collect()
}

/// Everything the map names is in the store: a resumed import trusts it.
fn assert_mapped_objects_are_stored(git: &Repo, objects: &Logged, map: &GitMap) {
    let stored = |id: &ObjectId| {
        let Ok(found) = objects.objects.contains(id);
        found
    };
    for oid in git_objects(git) {
        if let Some(changeset) = map.changeset(&oid) {
            assert!(stored(&changeset), "the changeset of commit {oid}");
        }
        if let Some(tree) = map.tree(&oid) {
            assert!(stored(&tree), "the braid tree of tree {oid}");
        }
        if let Some(raw) = map.raw_tree(&oid) {
            assert!(stored(&raw), "the raw bytes of tree {oid}");
        }
        if let Some((blob, _)) = map.blob(&oid) {
            assert!(stored(&blob), "the braid blob of blob {oid}");
        }
        if let Some(tag) = map.tag(&oid) {
            assert!(stored(&tag.raw), "the raw bytes of tag {oid}");
        }
    }
}

#[test]
fn the_parallel_import_makes_what_the_one_thread_import_makes() {
    let git = history();
    let (one, map, report) = assert_same(&git, &ImportOptions::default());
    // The history is what it says it is.
    assert!(report.commits > 140, "{report:?}");
    assert!(report.raw_trees >= 2, "{report:?}");
    assert_eq!(report.tags, 4, "{report:?}");
    assert!(
        one.writes()
            .iter()
            .any(|write| write.kind == Kind::Blob && write.hint.is_some_and(|(_, stored)| stored)),
        "blobs come with hints"
    );
    assert_mapped_objects_are_stored(&git, &one, &map);
}

#[test]
fn a_parallel_import_continues_one_as_a_one_thread_import_does() {
    let git = history();
    // The tag on release 1 first, then everything, into the same objects
    // and map: the second import only brings in what is new.
    let first = ImportOptions {
        refs: vec![b"refs/tags/v1".to_vec()],
    };
    let one = Logged::default();
    let mut one_map = GitMap::new();
    run(None, &git, &one, &mut one_map, &first).unwrap();
    let one_report = run(None, &git, &one, &mut one_map, &ImportOptions::default()).unwrap();
    for threads in THREADS {
        let many = Logged::default();
        let mut many_map = GitMap::new();
        // Each half on either side: parallel after one thread, and the
        // other way round.
        run(None, &git, &many, &mut many_map, &first).unwrap();
        let report = run(
            Some(threads),
            &git,
            &many,
            &mut many_map,
            &ImportOptions::default(),
        )
        .unwrap();
        assert_eq!(report, one_report, "{threads} threads");
        assert_eq!(many_map.to_bytes(), one_map.to_bytes(), "{threads} threads");
        assert!(many.contents() == one.contents(), "{threads} threads");
        assert_eq!(many.writes(), one.writes(), "{threads} threads");

        let other = Logged::default();
        let mut other_map = GitMap::new();
        run(Some(threads), &git, &other, &mut other_map, &first).unwrap();
        let report = run(
            None,
            &git,
            &other,
            &mut other_map,
            &ImportOptions::default(),
        )
        .unwrap();
        assert_eq!(report, one_report, "{threads} threads");
        assert_eq!(
            other_map.to_bytes(),
            one_map.to_bytes(),
            "{threads} threads"
        );
        assert_eq!(other.writes(), one.writes(), "{threads} threads");
    }
}

#[test]
fn a_refused_name_stops_both_imports_at_the_same_commit_with_the_same_map() {
    let mut git = history();
    let hook = git.tree(&[exec("hooks/post-checkout", "#!/bin/sh\necho owned\n")]);
    let evil = git.literally("tree", &raw_tree(&[("40000", b".GIT", &hook)]));
    let parent = git.head("main").unwrap();
    let commit = git.commit(&evil, &[&parent], "evil");
    git.set_ref("refs/heads/main", &commit);
    for threads in THREADS {
        let one = Logged::default();
        let mut one_map = GitMap::new();
        let one_err = run(None, &git, &one, &mut one_map, &ImportOptions::default()).unwrap_err();
        assert!(matches!(one_err, Error::RefusedPath { .. }), "{one_err}");
        let many = Logged::default();
        let mut many_map = GitMap::new();
        let many_err = run(
            Some(threads),
            &git,
            &many,
            &mut many_map,
            &ImportOptions::default(),
        )
        .unwrap_err();
        assert_eq!(many_err.to_string(), one_err.to_string());
        assert_eq!(many_map.to_bytes(), one_map.to_bytes(), "{threads} threads");
        assert_eq!(many.writes(), one.writes(), "{threads} threads");
        assert_mapped_objects_are_stored(&git, &many, &many_map);
    }
}

#[test]
fn a_store_that_fails_leaves_the_map_a_one_thread_import_leaves() {
    let git = history();
    let (_, clean_map, clean_report) = assert_same(&git, &ImportOptions::default());
    // A version of the file edited in every commit, past the first batch.
    let refused = ObjectId::hash(Kind::Blob, text(100).as_bytes());
    let mut first: Option<(String, Vec<u8>)> = None;
    for threads in [None, Some(1), Some(2), Some(8)] {
        let objects = Logged::default();
        *lock(&objects.refused) = Some(refused);
        let mut map = GitMap::new();
        let err = run(threads, &git, &objects, &mut map, &ImportOptions::default()).unwrap_err();
        assert!(matches!(err, Error::Objects(_)), "{threads:?}: {err}");
        assert!(map.counts().commits < clean_report.commits, "{threads:?}");
        assert_mapped_objects_are_stored(&git, &objects, &map);
        let failed = (err.to_string(), map.to_bytes());
        match &first {
            None => first = Some(failed),
            Some(first) => assert!(*first == failed, "{threads:?}: another error or map"),
        }

        // Once the store works, the import resumes from that map and ends
        // where an import that never failed does.
        *lock(&objects.refused) = None;
        run(threads, &git, &objects, &mut map, &ImportOptions::default()).unwrap();
        assert_eq!(map.to_bytes(), clean_map.to_bytes(), "{threads:?}");
        assert_mapped_objects_are_stored(&git, &objects, &map);
    }
}

#[test]
fn a_store_that_loses_what_it_stored_fails_the_checks_of_both_imports() {
    let git = history();
    let (_, clean_map, _) = assert_same(&git, &ImportOptions::default());
    let commit: GitOid = git
        .git(&["log", "--format=%H", "--grep=^commit 100$", "main"])
        .parse()
        .unwrap();
    let root: GitOid = git
        .git(&["rev-parse", &format!("{commit}^{{tree}}")])
        .parse()
        .unwrap();
    // Read back by the check of the commit, and by the check of its tree.
    for lost in [
        clean_map.changeset(&commit).unwrap(),
        clean_map.tree(&root).unwrap(),
    ] {
        let mut first: Option<(String, Vec<u8>)> = None;
        for threads in [None, Some(1), Some(2), Some(8)] {
            let objects = Logged::default();
            *lock(&objects.lost) = Some(lost);
            let mut map = GitMap::new();
            let err =
                run(threads, &git, &objects, &mut map, &ImportOptions::default()).unwrap_err();
            assert!(err.to_string().contains("missing"), "{threads:?}: {err}");
            assert!(map.changeset(&commit).is_none(), "{threads:?}");
            let failed = (err.to_string(), map.to_bytes());
            match &first {
                None => first = Some(failed),
                Some(first) => assert!(*first == failed, "{threads:?}: another error or map"),
            }
        }
    }
}
