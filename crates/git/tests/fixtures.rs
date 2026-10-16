// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Round trips of repositories built for one feature each, the import's
//! refusals, and the export of changesets made in braid.

mod common;

use std::collections::BTreeMap;

use braid_format::changeset::decode_change_list;
use braid_format::{
    Change, ChangeOp, Changes, Changeset, ChangesetParts, Entry, EntryKind, GitId, Kind,
    MemoryObjects, ObjectId, Objects, Signature, build_directory, read_directory,
};
use braid_git::{
    Error, GitKind, GitMap, GitOid, ImportOptions, RefTarget, RefUpdate, RoundTripReport,
    export_commit, export_object, import, import_parallel, verify_round_trip, write_to_repo,
    write_to_repo_recording,
};
use common::{Repo, exec, file, link, raw_tree, submodule};

fn oid(text: &str) -> GitOid {
    text.parse().unwrap()
}

fn round_trip(repo: &Repo) -> RoundTripReport {
    let tmp = tempfile::tempdir().unwrap();
    let report = verify_round_trip(repo.path(), tmp.path()).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(report.commits, repo.count_commits());
    report
}

fn import_all(repo: &Repo, objects: &MemoryObjects, map: &mut GitMap) -> braid_git::ImportReport {
    import(repo.path(), objects, map, &ImportOptions::default())
        .unwrap_or_else(|err| panic!("{err}"))
}

/// The thread counts of the parallel imports `check` compares.
const THREADS: [usize; 2] = [1, 4];

/// Every object held, with its kind and payload.
fn contents(objects: &MemoryObjects) -> BTreeMap<ObjectId, (Kind, Vec<u8>)> {
    objects
        .ids()
        .into_iter()
        .map(|id| (id, objects.get(&id).unwrap().unwrap()))
        .collect()
}

/// Imports into `objects` and `map` in parallel, and checks that the report,
/// the map and the objects are the one-thread import's.
fn assert_parallel_same(
    repo: &Repo,
    (objects, map): (&MemoryObjects, &mut GitMap),
    threads: usize,
    (expected_objects, expected_map, expected_report): (
        &MemoryObjects,
        &GitMap,
        &braid_git::ImportReport,
    ),
) {
    let report = import_parallel(
        repo.path(),
        objects,
        map,
        &ImportOptions::default(),
        threads,
    )
    .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(&report, expected_report, "{threads} threads");
    assert_eq!(map.to_bytes(), expected_map.to_bytes(), "{threads} threads");
    assert!(
        contents(objects) == contents(expected_objects),
        "{threads} threads: the objects differ"
    );
}

/// The checks every fixture goes through: the round trip holds; importing
/// imports every commit; importing again with the saved map imports nothing;
/// after `extend` adds commits, importing brings in exactly those; the round
/// trip still holds; and every imported changeset passes admission. The
/// parallel import, from scratch and then after `extend`, gives the same
/// reports, maps and objects as the one-thread import.
fn check(repo: &mut Repo, extend: impl FnOnce(&mut Repo) -> usize) -> (MemoryObjects, GitMap) {
    round_trip(repo);
    let objects = MemoryObjects::new();
    let mut map = GitMap::new();
    let first = import_all(repo, &objects, &mut map);
    assert_eq!(first.commits, repo.count_commits());
    assert_eq!(map.counts().commits, first.commits);
    let mut parallel: Vec<(MemoryObjects, GitMap)> = THREADS
        .iter()
        .map(|threads| {
            let (many, mut many_map) = (MemoryObjects::new(), GitMap::new());
            assert_parallel_same(
                repo,
                (&many, &mut many_map),
                *threads,
                (&objects, &map, &first),
            );
            (many, many_map)
        })
        .collect();

    let mut map = GitMap::from_bytes(&map.to_bytes()).unwrap();
    let again = import_all(repo, &objects, &mut map);
    assert_eq!(
        (again.commits, again.trees, again.blobs, again.tags),
        (0, 0, 0, 0),
        "nothing is new"
    );
    assert_eq!(again.refs, first.refs);

    let added = extend(repo);
    assert!(added > 0);
    let next = import_all(repo, &objects, &mut map);
    assert_eq!(next.commits, added, "only the new commits are imported");
    assert_eq!(map.counts().commits, repo.count_commits());
    for ((many, many_map), threads) in parallel.iter_mut().zip(THREADS) {
        assert_parallel_same(repo, (many, many_map), threads, (&objects, &map, &next));
    }
    round_trip(repo);
    assert_admissible(&objects, &map);
    (objects, map)
}

fn decode(objects: &MemoryObjects, id: &ObjectId) -> Changeset {
    let (kind, bytes) = objects.get(id).unwrap().unwrap();
    assert_eq!(kind, Kind::Changeset);
    Changeset::decode(&bytes).unwrap()
}

fn changeset(objects: &MemoryObjects, map: &GitMap, commit: &str) -> Changeset {
    decode(
        objects,
        &map.changeset(&oid(commit)).expect("commit imported"),
    )
}

fn change_list(objects: &MemoryObjects, changeset: &Changeset) -> Vec<Change> {
    match &changeset.changes {
        Changes::Inline(list) => list.clone(),
        Changes::External { list, .. } => {
            decode_change_list(&objects.get(list).unwrap().unwrap().1).unwrap()
        }
    }
}

/// A directory held in memory, to apply change lists to.
type Dir = BTreeMap<Vec<u8>, Node>;

#[derive(Clone, Debug)]
enum Node {
    Leaf(EntryKind),
    Dir(Dir),
}

/// Reads a braid directory and everything below it, and checks that no
/// subdirectory is empty: braid trees never hold one.
fn load(objects: &MemoryObjects, tree: ObjectId) -> Dir {
    let entries =
        read_directory(tree, |id| objects.get(id).unwrap().map(|(_, bytes)| bytes)).unwrap();
    entries
        .into_iter()
        .map(|entry| {
            let node = match entry.kind {
                EntryKind::Tree { tree } => {
                    let below = load(objects, tree);
                    assert!(
                        !below.is_empty(),
                        "{:?} is an empty subdirectory",
                        String::from_utf8_lossy(&entry.name)
                    );
                    Node::Dir(below)
                }
                kind => Node::Leaf(kind),
            };
            (entry.name, node)
        })
        .collect()
}

/// The braid id of a directory held in memory.
fn tree_id(dir: &Dir) -> ObjectId {
    let entries = dir
        .iter()
        .map(|(name, node)| Entry {
            name: name.clone(),
            kind: match node {
                Node::Leaf(kind) => kind.clone(),
                Node::Dir(below) => EntryKind::Tree {
                    tree: tree_id(below),
                },
            },
        })
        .collect();
    build_directory(entries).unwrap().root()
}

/// Applies a change list as the store's admission check does. Each change
/// gives a file's final value, so deletes go first; a directory goes with its
/// last entry, and setting a file creates the directories above it.
fn apply(dir: &mut Dir, changes: &[Change]) {
    let split = |path: &[u8]| -> Vec<Vec<u8>> {
        path.split(|byte| *byte == b'/')
            .map(<[u8]>::to_vec)
            .collect()
    };
    for change in changes.iter().filter(|c| c.op == ChangeOp::Delete) {
        delete(dir, &split(&change.path));
    }
    for change in changes.iter().filter(|c| c.op != ChangeOp::Delete) {
        set(dir, &split(&change.path), change.op.to_entry().unwrap());
    }
}

fn delete(dir: &mut Dir, path: &[Vec<u8>]) {
    match path {
        [name] => assert!(
            matches!(dir.remove(name), Some(Node::Leaf(_))),
            "deleting a file that is not there"
        ),
        [name, rest @ ..] => {
            let Some(Node::Dir(below)) = dir.get_mut(name) else {
                panic!("deleting below a directory that is not there")
            };
            delete(below, rest);
            if below.is_empty() {
                dir.remove(name);
            }
        }
        [] => unreachable!("paths are never empty"),
    }
}

fn set(dir: &mut Dir, path: &[Vec<u8>], kind: EntryKind) {
    match path {
        [name] => {
            let previous = dir.insert(name.clone(), Node::Leaf(kind));
            assert!(
                !matches!(previous, Some(Node::Dir(_))),
                "a file over a directory that still has entries"
            );
        }
        [name, rest @ ..] => {
            let node = dir
                .entry(name.clone())
                .or_insert_with(|| Node::Dir(Dir::new()));
            let Node::Dir(below) = node else {
                panic!("a directory over a file that was not deleted")
            };
            set(below, rest, kind);
        }
        [] => unreachable!("paths are never empty"),
    }
}

/// Every imported changeset passes admission: its change list, applied to
/// the root of its first parent (or to nothing), gives its own root, and no
/// tree it holds has an empty subdirectory.
fn assert_admissible(objects: &MemoryObjects, map: &GitMap) {
    for (commit, id) in map.commits() {
        let changeset = decode(objects, &id);
        let mut dir = match changeset.parents.first() {
            Some(parent) => load(objects, decode(objects, parent).root),
            None => Dir::new(),
        };
        apply(&mut dir, &change_list(objects, &changeset));
        assert_eq!(tree_id(&dir), changeset.root, "commit {commit}");
        load(objects, changeset.root);
    }
}

/// The change list of an imported commit, as path and operation.
fn changes(objects: &MemoryObjects, map: &GitMap, commit: &str) -> Vec<(String, &'static str)> {
    let list = change_list(objects, &changeset(objects, map, commit));
    list.into_iter()
        .map(|change| {
            let op = match change.op {
                ChangeOp::Delete => "delete",
                ChangeOp::File { .. } => "file",
                ChangeOp::Executable { .. } => "executable",
                ChangeOp::Symlink { .. } => "symlink",
                ChangeOp::Gitlink { .. } => "gitlink",
            };
            (String::from_utf8_lossy(&change.path).into_owned(), op)
        })
        .collect()
}

fn pairs(list: &[(&str, &'static str)]) -> Vec<(String, &'static str)> {
    list.iter()
        .map(|(path, op)| (path.to_string(), *op))
        .collect()
}

#[test]
fn linear_history() {
    let mut repo = Repo::new();
    let first = repo.snapshot(
        "main",
        &[
            file("README.md", "hello\n"),
            file("src/lib.rs", "pub fn f() {}\n"),
            file("src/util/mod.rs", ""),
        ],
        "first",
    );
    let second = repo.snapshot(
        "main",
        &[
            file("README.md", "hello again\n"),
            file("src/lib.rs", "pub fn f() {}\n"),
            file("src/util/mod.rs", ""),
            file("docs/guide.md", "guide\n"),
        ],
        "second",
    );
    let third = repo.snapshot(
        "main",
        &[
            file("README.md", "hello again\n"),
            file("docs/guide.md", "guide\n"),
        ],
        "third: drop src",
    );
    let empty = repo.snapshot("main", &[], "everything deleted");
    let (objects, map) = check(&mut repo, |repo| {
        repo.snapshot("main", &[file("README.md", "back\n")], "fourth");
        repo.snapshot("main", &[file("README.md", "and again\n")], "fifth");
        2
    });
    assert_eq!(
        changes(&objects, &map, &first),
        pairs(&[
            ("README.md", "file"),
            ("src/lib.rs", "file"),
            ("src/util/mod.rs", "file")
        ])
    );
    assert_eq!(
        changes(&objects, &map, &second),
        pairs(&[("README.md", "file"), ("docs/guide.md", "file")])
    );
    assert_eq!(
        changes(&objects, &map, &third),
        pairs(&[("src/lib.rs", "delete"), ("src/util/mod.rs", "delete")])
    );
    assert_eq!(
        changes(&objects, &map, &empty),
        pairs(&[("README.md", "delete"), ("docs/guide.md", "delete")])
    );
    let first_changeset = changeset(&objects, &map, &first);
    assert!(first_changeset.parents.is_empty());
    assert_eq!(first_changeset.message, b"first\n");
    assert_eq!(first_changeset.author.name, b"Ada Lovelace");
    assert_eq!(first_changeset.author.email, b"ada@example.org");
    assert_eq!(first_changeset.author.offset_minutes, 60);
    assert_eq!(first_changeset.committer.name, b"Charles Babbage");
    assert!(first_changeset.extras.is_empty());
    assert_eq!(
        changeset(&objects, &map, &second).parents,
        [map.changeset(&oid(&first)).unwrap()]
    );
}

#[test]
fn branches_and_merges() {
    let mut repo = Repo::new();
    let base = repo.snapshot("main", &[file("a", "1")], "base");
    for branch in ["left", "right", "third"] {
        repo.set_ref(&format!("refs/heads/{branch}"), &base);
    }
    let left = repo.snapshot("left", &[file("a", "1"), file("l", "left")], "left");
    let right = repo.snapshot("right", &[file("a", "1"), file("r", "right")], "right");
    let third = repo.snapshot("third", &[file("a", "1"), file("t", "third")], "third");
    repo.snapshot("main", &[file("a", "2")], "main moves on");
    let merge = repo.merge(
        "main",
        &["left"],
        &[file("a", "2"), file("l", "left")],
        "merge left",
    );
    let octopus = repo.merge(
        "main",
        &["right", "third"],
        &[
            file("a", "2"),
            file("l", "left"),
            file("r", "right"),
            file("t", "third"),
        ],
        "octopus",
    );
    let (objects, map) = check(&mut repo, |repo| {
        repo.snapshot("left", &[file("a", "1"), file("l", "more")], "left again");
        1
    });
    assert_eq!(changes(&objects, &map, &merge), pairs(&[("l", "file")]));
    assert_eq!(
        changes(&objects, &map, &octopus),
        pairs(&[("r", "file"), ("t", "file")])
    );
    let parents = changeset(&objects, &map, &octopus).parents;
    assert_eq!(parents.len(), 3);
    assert_eq!(parents[1], map.changeset(&oid(&right)).unwrap());
    assert_eq!(parents[2], map.changeset(&oid(&third)).unwrap());
    assert_eq!(
        changeset(&objects, &map, &merge).parents[1],
        map.changeset(&oid(&left)).unwrap()
    );
}

#[test]
fn renames_and_a_directory_replaced_by_a_file() {
    let mut repo = Repo::new();
    repo.snapshot(
        "main",
        &[
            file("old.txt", "content\n"),
            file("dir/a", "a"),
            file("dir/b/c", "c"),
        ],
        "start",
    );
    let rename = repo.snapshot(
        "main",
        &[
            file("new.txt", "content\n"),
            file("dir/a", "a"),
            file("dir/b/c", "c"),
        ],
        "rename",
    );
    let to_file = repo.snapshot(
        "main",
        &[file("new.txt", "content\n"), file("dir", "now a file\n")],
        "directory to file",
    );
    let to_dir = repo.snapshot(
        "main",
        &[file("new.txt", "content\n"), file("dir/x", "x")],
        "file to directory",
    );
    let (objects, map) = check(&mut repo, |repo| {
        repo.snapshot("main", &[file("newer.txt", "content\n")], "rename again");
        1
    });
    assert_eq!(
        changes(&objects, &map, &rename),
        pairs(&[("new.txt", "file"), ("old.txt", "delete")])
    );
    assert_eq!(
        changes(&objects, &map, &to_file),
        pairs(&[("dir", "file"), ("dir/a", "delete"), ("dir/b/c", "delete")])
    );
    assert_eq!(
        changes(&objects, &map, &to_dir),
        pairs(&[("dir", "delete"), ("dir/x", "file")])
    );
}

#[test]
fn symlinks_and_executable_bits() {
    let mut repo = Repo::new();
    let links = [
        link("latest", "run.sh"),
        link("dangling", "nowhere/at/all"),
        link("outside", "../../etc"),
        link("dir/up", ".."),
    ];
    let mut entries = vec![file("run.sh", "#!/bin/sh\necho hi\n")];
    entries.extend(links.iter().cloned());
    repo.snapshot("main", &entries, "links");
    entries[0] = exec("run.sh", "#!/bin/sh\necho hi\n");
    let chmod = repo.snapshot("main", &entries, "make it executable");
    entries[1] = file("latest", "run.sh");
    let unlink = repo.snapshot("main", &entries, "a link becomes a file");
    let (objects, map) = check(&mut repo, |repo| {
        entries[0] = file("run.sh", "#!/bin/sh\necho hi\n");
        repo.snapshot("main", &entries, "not executable any more");
        1
    });
    assert_eq!(
        changes(&objects, &map, &chmod),
        pairs(&[("run.sh", "executable")])
    );
    assert_eq!(
        changes(&objects, &map, &unlink),
        pairs(&[("latest", "file")])
    );
}

const SUBMODULE_A: &str = "1111111111111111111111111111111111111111";
const SUBMODULE_B: &str = "2222222222222222222222222222222222222222";

#[test]
fn submodule_gitlinks() {
    let mut repo = Repo::new();
    let text = "[submodule \"vendor\"]\n\tpath = vendor\n\turl = https://example.org/vendor.git\n";
    let modules = file(".gitmodules", text);
    // The gitlink goes in through the index, as `git submodule add` does.
    let modules_blob = repo.blob(text.as_bytes());
    let tree = repo.index_tree(&[
        format!("100644,{modules_blob},.gitmodules"),
        format!("160000,{SUBMODULE_A},vendor"),
    ]);
    assert_eq!(
        tree,
        repo.tree(&[modules.clone(), submodule("vendor", SUBMODULE_A)])
    );
    let add = repo.commit(&tree, &[], "add a submodule");
    repo.set_ref("refs/heads/main", &add);
    let bump = repo.snapshot(
        "main",
        &[modules.clone(), submodule("vendor", SUBMODULE_B)],
        "bump it",
    );
    let (objects, map) = check(&mut repo, |repo| {
        repo.snapshot("main", &[file(".gitmodules", "")], "remove it");
        1
    });
    assert_eq!(
        changes(&objects, &map, &add),
        pairs(&[(".gitmodules", "file"), ("vendor", "gitlink")])
    );
    assert_eq!(
        changes(&objects, &map, &bump),
        pairs(&[("vendor", "gitlink")])
    );
}

#[test]
fn awkward_names_and_contents() {
    let mut repo = Repo::new();
    let mut entries: Vec<common::Entry> = vec![
        file(b"latin1-caf\xe9.txt", "a Latin-1 name\n"),
        file(b"\xff\xfe\xfd", "not UTF-8 at all\n"),
        file("café.txt", "precomposed\n"),
        file("cafe\u{301}.txt", "decomposed\n"),
        file("🦀.rs", "fn crab() {}\n"),
        file("日本語/ファイル.txt", "日本語\n"),
        file("עברית.txt", "right to left\n"),
        file("zero\u{200d}width.txt", "joiner\n"),
        file("with space/and tab\there", "x"),
        file("-leading-dash", "x"),
        file("#hash", "x"),
        file(".hidden", "x"),
        file("a:b", "a colon"),
        file("back\\slash", "x"),
        file("x".repeat(255), "a long name"),
        file("empty", ""),
        file("no-newline", "last line"),
        file("crlf.txt", "a\r\nb\r\n"),
        file("binary.bin", [0u8, 1, 2, 255, 0, 10, 13]),
    ];
    let first = repo.snapshot("main", &entries, "awkward");
    let (objects, map) = check(&mut repo, |repo| {
        entries[1] = file(b"\xff\xfe\xfd", "still not UTF-8\n");
        entries.push(file(b"dir-\xc0/inner", "under a non-UTF-8 directory"));
        repo.snapshot("main", &entries, "more");
        1
    });
    let listed = changes(&objects, &map, &first);
    assert_eq!(listed.len(), 19);
    let changeset = changeset(&objects, &map, &first);
    let Changes::Inline(list) = changeset.changes else {
        panic!("a short list is inline")
    };
    assert!(
        list.iter()
            .any(|change| change.path == b"latin1-caf\xe9.txt")
    );
    assert!(
        list.iter()
            .any(|change| change.path == "cafe\u{301}.txt".as_bytes())
    );
}

#[test]
fn crafted_commits_keep_their_bytes() {
    let mut repo = Repo::new();
    let base = repo.snapshot("main", &[file("a", "1")], "base");
    let tree = repo.tree(&[file("a", "1")]);
    let headers = |parent: &str, author: &str, committer: &str| {
        format!("tree {tree}\nparent {parent}\nauthor {author}\ncommitter {committer}\n")
    };
    let ada = "Ada Lovelace <ada@example.org> 1700000000 +0100";

    let mut latin1 = headers(&base, ada, ada).into_bytes();
    latin1.extend_from_slice(b"encoding ISO-8859-1\n\nCaf\xe9 cr\xe8me\n");
    let latin1 = repo.literally("commit", &latin1);

    let extra = format!(
        "{}x-custom value\nmulti first\n continued\n \n after a blank line\n\
         gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEE\n -----END PGP SIGNATURE-----\n\
         \nlooks signed\n",
        headers(&latin1, ada, ada)
    );
    let extra = repo.literally("commit", extra.as_bytes());

    let odd = format!(
        "{}\nno trailing newline",
        headers(
            &extra,
            "Odd<odd@example.org>1700000123 -0000",
            " <> 0 +0000"
        )
    );
    let odd = repo.literally("commit", odd.as_bytes());

    let bare = headers(&odd, ada, "Strange Zone <z@example.org> 1700000000 +1560");
    let bare = repo.literally("commit", bare.as_bytes());

    let uppercase = format!(
        "tree {}\nparent {bare}\nauthor {ada}\ncommitter {ada}\n\nupper-case tree id\n",
        tree.to_uppercase()
    );
    let uppercase = repo.literally("commit", uppercase.as_bytes());
    repo.set_ref("refs/heads/crafted", &uppercase);

    let (objects, map) = check(&mut repo, |repo| {
        repo.snapshot("crafted", &[file("a", "2")], "ordinary again");
        1
    });

    for commit in [&latin1, &extra, &odd, &bare, &uppercase] {
        let changeset = changeset(&objects, &map, commit);
        let origin = objects
            .get(&changeset.git_origin.unwrap())
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(origin, repo.git_bytes(&["cat-file", "commit", commit]));
        assert!(changeset.extras.is_empty());
    }
    assert_eq!(
        changeset(&objects, &map, &latin1).message,
        b"Caf\xe9 cr\xe8me\n"
    );
    assert_eq!(changeset(&objects, &map, &extra).message, b"looks signed\n");
    let odd = changeset(&objects, &map, &odd);
    assert_eq!(odd.message, b"no trailing newline");
    assert_eq!(
        (odd.author.name.as_slice(), odd.author.email.as_slice()),
        (&b"Odd"[..], &b"odd@example.org"[..])
    );
    assert_eq!(
        (odd.author.seconds, odd.author.offset_minutes),
        (1_700_000_123, 0)
    );
    assert_eq!(odd.committer.name, b"");
    assert_eq!(odd.committer.seconds, 0);
    let bare = changeset(&objects, &map, &bare);
    assert_eq!(bare.message, b"");
    assert_eq!(bare.committer.offset_minutes, 15 * 60 + 60);
    assert_eq!(changes(&objects, &map, &uppercase), pairs(&[]));
}

#[test]
fn non_canonical_trees_keep_their_raw_bytes() {
    let mut repo = Repo::new();
    let x = repo.blob(b"x\n");
    let y = repo.blob(b"y\n");
    let inner = repo.tree(&[file("inner", "x\n")]);
    let group_writable = repo.literally("tree", &raw_tree(&[("100664", b"group-writable", &x)]));
    let padded = repo.literally("tree", &raw_tree(&[("040000", b"dir", &inner)]));
    let padded_file = repo.literally("tree", &raw_tree(&[("0100644", b"f", &x)]));
    let unsorted = repo.literally(
        "tree",
        &raw_tree(&[("100644", b"b", &x), ("100644", b"a", &y)]),
    );
    let duplicate = repo.literally(
        "tree",
        &raw_tree(&[("100644", b"same", &x), ("100644", b"same", &y)]),
    );
    let file_and_dir = repo.literally(
        "tree",
        &raw_tree(&[("100644", b"twice", &x), ("40000", b"twice", &inner)]),
    );
    // Canonical on its own, but it names a tree that is not.
    let holder = repo.mktree(&[("40000", "tree", &group_writable, b"nested")]);
    let odd_trees = [
        ("100664", &group_writable),
        ("padded", &padded),
        ("padded-file", &padded_file),
        ("unsorted", &unsorted),
        ("duplicate", &duplicate),
        ("file-and-dir", &file_and_dir),
        ("holder", &holder),
    ];
    let root_with = |repo: &Repo, plain: &str| {
        let plain = repo.blob(plain.as_bytes());
        let mut entries: Vec<(&str, &str, &str, &[u8])> = odd_trees
            .iter()
            .map(|(name, id)| ("40000", "tree", id.as_str(), name.as_bytes()))
            .collect();
        entries.push(("100644", "blob", &plain, b"plain"));
        entries.push(("40000", "tree", &inner, b"canonical"));
        repo.mktree(&entries)
    };
    let first_root = root_with(&repo, "one\n");
    let first = repo.commit(&first_root, &[], "odd trees");
    let second_root = root_with(&repo, "two\n");
    let second = repo.commit(&second_root, &[&first], "edit beside them");
    repo.set_ref("refs/heads/main", &second);

    let (objects, map) = check(&mut repo, |repo| {
        let third_root = root_with(repo, "three\n");
        let third = repo.commit(&third_root, &[&second], "again");
        repo.set_ref("refs/heads/main", &third);
        1
    });
    for (name, id) in odd_trees {
        assert!(map.raw_tree(&oid(id)).is_some(), "{name} keeps its bytes");
        assert_eq!(
            export_object(&objects, &map, &oid(id)).unwrap(),
            (GitKind::Tree, repo.git_bytes(&["cat-file", "tree", id]))
        );
    }
    assert!(
        map.raw_tree(&oid(&first_root)).is_some(),
        "it holds odd trees"
    );
    assert!(map.raw_tree(&oid(&inner)).is_none(), "canonical");
    assert_eq!(map.counts().raw_trees, odd_trees.len() + 3);
    assert_eq!(
        changes(&objects, &map, &second),
        pairs(&[("plain", "file")])
    );

    // Braid reads `100664` as a plain file, keeps the last of duplicate
    // names as a git checkout does, and sorts by name bytes.
    let entries = |tree: &str| {
        let root = map.tree(&oid(tree)).unwrap();
        let (_, bytes) = objects.get(&root).unwrap().unwrap();
        match braid_format::Node::decode(&bytes).unwrap() {
            braid_format::Node::Flat(entries) => entries,
            other => panic!("unexpected node {other:?}"),
        }
    };
    assert!(matches!(
        entries(&group_writable)[0].kind,
        EntryKind::File { size: 2, .. }
    ));
    let names: Vec<Vec<u8>> = entries(&unsorted).into_iter().map(|e| e.name).collect();
    assert_eq!(names, [b"a".to_vec(), b"b".to_vec()]);
    let same = entries(&duplicate);
    assert_eq!(same.len(), 1);
    assert!(matches!(
        same[0].kind,
        EntryKind::File { blob, .. } if blob == ObjectId::hash(Kind::Blob, b"y\n")
    ));
    assert!(matches!(
        entries(&file_and_dir)[0].kind,
        EntryKind::Tree { .. }
    ));

    // Git agrees: reading the duplicates into an index keeps the last one.
    let index = repo.scratch().join("duplicates-index");
    let mut read_tree = repo.command();
    read_tree.env("GIT_INDEX_FILE", &index);
    read_tree.args(["read-tree", &duplicate]);
    assert!(read_tree.status().unwrap().success());
    let mut list = repo.command();
    list.env("GIT_INDEX_FILE", &index);
    list.args(["ls-files", "--stage"]);
    let listed = String::from_utf8(list.output().unwrap().stdout).unwrap();
    assert_eq!(listed, format!("100644 {y} 0\tsame\n"));
}

#[test]
fn annotated_tags() {
    let mut repo = Repo::new();
    let first = repo.snapshot("main", &[file("a", "1")], "first");
    let v1 = repo.annotated_tag("v1", &first, "release 1");
    repo.git(&["tag", "lightweight", &first]);
    let nested = repo.annotated_tag("v1-blessed", &v1, "a tag of a tag");
    let tree = repo.tree(&[file("only-in-a-tag", "t\n"), file("dir/deeper", "d\n")]);
    let tree_tag = repo.annotated_tag("tree-tag", &tree, "points to a tree");
    let blob = repo.blob(b"a blob on its own\n");
    let blob_tag = repo.annotated_tag("blob-tag", &blob, "points to a blob");
    repo.git(&["tag", "lightweight-tree", &tree]);
    let (objects, map) = check(&mut repo, |repo| {
        let second = repo.snapshot("main", &[file("a", "2")], "second");
        repo.annotated_tag("v2", &second, "release 2");
        repo.git(&["tag", "--force", "moved", &v1]);
        1
    });
    // A name that pointed to an annotated tag and now is lightweight.
    let mut map = map;
    let moved = repo.git(&["rev-parse", "refs/tags/moved"]);
    assert_eq!(map.tag_by_name(b"refs/tags/moved").unwrap().0, oid(&moved));
    repo.git(&["tag", "--force", "moved", &first]);
    import_all(&repo, &objects, &mut map);
    assert!(map.tag_by_name(b"refs/tags/moved").is_none());

    let (tag, entry) = map.tag_by_name(b"refs/tags/v1").unwrap();
    assert_eq!(tag, oid(&v1));
    assert_eq!(
        (entry.target, entry.target_kind),
        (oid(&first), GitKind::Commit)
    );
    assert_eq!(
        objects.get(&entry.raw).unwrap().unwrap().1,
        repo.git_bytes(&["cat-file", "tag", &v1])
    );
    let (_, entry) = map.tag_by_name(b"refs/tags/v1-blessed").unwrap();
    assert_eq!((entry.target, entry.target_kind), (oid(&v1), GitKind::Tag));
    let (_, entry) = map.tag_by_name(b"refs/tags/tree-tag").unwrap();
    assert_eq!(
        (entry.target, entry.target_kind),
        (oid(&tree), GitKind::Tree)
    );
    let (_, entry) = map.tag_by_name(b"refs/tags/blob-tag").unwrap();
    assert_eq!(
        (entry.target, entry.target_kind),
        (oid(&blob), GitKind::Blob)
    );
    assert!(map.tag_by_name(b"refs/tags/lightweight").is_none());
    assert!(map.tag_by_name(b"refs/tags/v2").is_some());
    assert_eq!(map.counts().tags, 5);
    for tag in [&v1, &nested, &tree_tag, &blob_tag] {
        assert_eq!(
            map.git_tag(&map.tag(&oid(tag)).unwrap().raw),
            Some(oid(tag))
        );
    }
}

#[test]
fn a_directory_with_more_than_1024_files() {
    let mut repo = Repo::new();
    let mut entries: Vec<common::Entry> = (0..1100)
        .map(|i| file(format!("big/f{i:04}.txt"), format!("{i}\n")))
        .collect();
    entries.push(file("README", "a big directory\n"));
    repo.snapshot("main", &entries, "many files");
    entries[500] = file("big/f0500.txt", "changed\n");
    entries.push(file("big/zz-new", "new\n"));
    let edit = repo.snapshot("main", &entries, "edit one, add one");
    let (objects, map) = check(&mut repo, |repo| {
        entries.retain(|(path, _)| path != b"big/f0007.txt");
        repo.snapshot("main", &entries, "delete one");
        1
    });
    assert_eq!(
        changes(&objects, &map, &edit),
        pairs(&[("big/f0500.txt", "file"), ("big/zz-new", "file")])
    );
    // The braid side of `big` is split into several nodes.
    let root = changeset(&objects, &map, &edit).root;
    let (_, bytes) = objects.get(&root).unwrap().unwrap();
    let braid_format::Node::Flat(top) = braid_format::Node::decode(&bytes).unwrap() else {
        panic!("the root is small")
    };
    let big = top.iter().find(|e| e.name == b"big").unwrap();
    let EntryKind::Tree { tree } = big.kind else {
        panic!("big is a directory")
    };
    let (_, bytes) = objects.get(&tree).unwrap().unwrap();
    assert!(matches!(
        braid_format::Node::decode(&bytes).unwrap(),
        braid_format::Node::Internal { .. }
    ));
}

#[test]
fn replacement_objects_and_grafts_are_ignored() {
    let mut repo = Repo::new();
    let first = repo.snapshot("main", &[file("a", "1")], "one");
    let second = repo.snapshot("main", &[file("a", "2")], "two");
    let fake_tree = repo.tree(&[file("a", "fake")]);
    let fake = repo.commit(&fake_tree, &[], "a replacement");
    repo.git(&["replace", &second, &fake]);
    std::fs::write(repo.path().join("info/grafts"), format!("{second}\n")).unwrap();
    let (objects, map) = check(&mut repo, |repo| {
        repo.snapshot("main", &[file("a", "3")], "three");
        1
    });
    assert_eq!(
        changeset(&objects, &map, &second).parents,
        [map.changeset(&oid(&first)).unwrap()]
    );
    assert_eq!(changes(&objects, &map, &second), pairs(&[("a", "file")]));
    assert!(map.changeset(&oid(&fake)).is_none());
}

#[test]
fn selected_refs_only() {
    let mut repo = Repo::new();
    let base = repo.snapshot("main", &[file("a", "1")], "base");
    repo.set_ref("refs/heads/side", &base);
    repo.snapshot("side", &[file("b", "2")], "side");
    let objects = MemoryObjects::new();
    let mut map = GitMap::new();
    let options = ImportOptions {
        refs: vec![b"main".to_vec()],
    };
    let report = import(repo.path(), &objects, &mut map, &options).unwrap();
    assert_eq!(report.commits, 1);
    assert_eq!(report.refs.len(), 1);
    assert_eq!(report.refs[0].name, b"refs/heads/main");
    let missing = ImportOptions {
        refs: vec![b"refs/heads/nope".to_vec()],
    };
    assert!(matches!(
        import(repo.path(), &objects, &mut map, &missing),
        Err(Error::MissingRef(_))
    ));
}

#[test]
fn shallow_clones_are_refused() {
    let mut repo = Repo::new();
    for i in 0..3 {
        repo.snapshot("main", &[file("a", format!("{i}"))], "commit");
    }
    let shallow = repo.scratch().join("shallow.git");
    let url = format!("file://{}", repo.path().display());
    let mut clone = repo.command();
    clone.args(["clone", "--quiet", "--bare", "--depth", "1", &url]);
    clone.arg(&shallow);
    assert!(clone.status().unwrap().success());
    let err = import(
        &shallow,
        &MemoryObjects::new(),
        &mut GitMap::new(),
        &ImportOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Shallow), "{err}");
    assert!(err.to_string().contains("shallow"));
}

#[test]
fn sha256_repositories_are_refused() {
    let mut repo = Repo::init(&["--object-format=sha256"]);
    repo.snapshot("main", &[file("a", "1")], "a SHA-256 commit");
    let err = import(
        repo.path(),
        &MemoryObjects::new(),
        &mut GitMap::new(),
        &ImportOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Sha256), "{err}");
    assert!(err.to_string().contains("SHA-256"));
    let tmp = tempfile::tempdir().unwrap();
    assert!(matches!(
        verify_round_trip(repo.path(), tmp.path()),
        Err(Error::Sha256)
    ));
}

/// Builds a commit whose tree holds `evil` (a path of names) as a directory
/// with a hook inside, and checks the import refuses it by commit and path.
fn refused(evil: &[&[u8]], expected_path: &[u8]) {
    let mut repo = Repo::new();
    let hook = repo.tree(&[exec("hooks/post-checkout", "#!/bin/sh\necho owned\n")]);
    let mut id = hook;
    for name in evil.iter().rev() {
        id = repo.literally("tree", &raw_tree(&[("40000", name, &id)]));
    }
    let commit = repo.commit(&id, &[], "evil");
    repo.set_ref("refs/heads/main", &commit);
    let err = import(
        repo.path(),
        &MemoryObjects::new(),
        &mut GitMap::new(),
        &ImportOptions::default(),
    )
    .unwrap_err();
    match &err {
        Error::RefusedPath {
            commit: c, path, ..
        } => {
            assert_eq!(c, &oid(&commit));
            assert_eq!(path, expected_path);
        }
        other => panic!("expected a refused path, got {other}"),
    }
    assert!(err.to_string().contains(&commit), "{err}");
}

#[test]
fn names_that_alias_dot_git_are_refused() {
    refused(&[b".git"], b".git");
    refused(&[b".GIT"], b".GIT");
    refused(&[b".Git."], b".Git.");
    refused(&[b"GIT~1"], b"GIT~1");
    refused(&[".g\u{200c}it".as_bytes()], ".g\u{200c}it".as_bytes());
    refused(&[b".braid"], b".braid");
    refused(&[b"sub", b"deeper", b".git"], b"sub/deeper/.git");
    refused(&[b".."], b"..");
    refused(&[b"a/b"], b"a/b");
}

#[test]
fn symlinks_on_files_git_reads_are_refused() {
    let mut repo = Repo::new();
    let target = repo.blob(b"/etc/passwd");
    let tree = repo.literally("tree", &raw_tree(&[("120000", b".gitmodules", &target)]));
    let commit = repo.commit(&tree, &[], "evil link");
    repo.set_ref("refs/heads/main", &commit);
    let err = import(
        repo.path(),
        &MemoryObjects::new(),
        &mut GitMap::new(),
        &ImportOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::RefusedPath { path, .. } if path == b".gitmodules"),
        "{err}"
    );
}

#[test]
fn unknown_modes_are_refused() {
    let mut repo = Repo::new();
    let x = repo.blob(b"x");
    let tree = repo.literally("tree", &raw_tree(&[("20000", b"device", &x)]));
    let commit = repo.commit(&tree, &[], "odd mode");
    repo.set_ref("refs/heads/main", &commit);
    let err = import(
        repo.path(),
        &MemoryObjects::new(),
        &mut GitMap::new(),
        &ImportOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::UnsupportedMode { path, mode, .. } if path == b"device" && mode == b"20000"),
        "{err}"
    );
}

fn store_tree(objects: &MemoryObjects, entries: Vec<Entry>) -> ObjectId {
    let directory = build_directory(entries).unwrap();
    for (_, bytes) in &directory.nodes {
        objects.put(Kind::Tree, bytes).unwrap();
    }
    directory.root()
}

fn store_blob(objects: &MemoryObjects, content: &[u8]) -> (ObjectId, u64) {
    (
        objects.put(Kind::Blob, content).unwrap(),
        content.len() as u64,
    )
}

#[test]
fn changesets_made_in_braid_export_deterministically() {
    let mut repo = Repo::new();
    let imported = repo.snapshot(
        "main",
        &[
            file("README", "hi\n"),
            exec("run.sh", "#!/bin/sh\n"),
            link("latest", "README"),
            submodule("vendor", SUBMODULE_A),
        ],
        "imported",
    );
    let objects = MemoryObjects::new();
    let mut map = GitMap::new();
    import_all(&repo, &objects, &mut map);

    // A change made in braid: edit README, add docs/a.md, drop run.sh.
    let (readme, readme_size) = store_blob(&objects, b"hello from braid\n");
    let (doc, doc_size) = store_blob(&objects, b"# A\n");
    let (latest, latest_size) = store_blob(&objects, b"README");
    let docs = store_tree(
        &objects,
        vec![Entry {
            name: b"a.md".to_vec(),
            kind: EntryKind::File {
                blob: doc,
                size: doc_size,
            },
        }],
    );
    let root = store_tree(
        &objects,
        vec![
            Entry {
                name: b"README".to_vec(),
                kind: EntryKind::File {
                    blob: readme,
                    size: readme_size,
                },
            },
            Entry {
                name: b"docs".to_vec(),
                kind: EntryKind::Tree { tree: docs },
            },
            Entry {
                name: b"latest".to_vec(),
                kind: EntryKind::Symlink {
                    blob: latest,
                    size: latest_size,
                },
            },
            Entry {
                name: b"vendor".to_vec(),
                kind: EntryKind::Gitlink {
                    commit: GitId::Sha1([0x11; 20]),
                },
            },
        ],
    );
    let grace = Signature {
        name: b"Grace Hopper".to_vec(),
        email: b"grace@example.org".to_vec(),
        seconds: 1_700_000_500,
        offset_minutes: -300,
    };
    let assembled = ChangesetParts {
        parents: vec![map.changeset(&oid(&imported)).unwrap()],
        root,
        changes: vec![
            braid_format::Change {
                path: b"README".to_vec(),
                op: ChangeOp::File {
                    blob: readme,
                    size: readme_size,
                },
            },
            braid_format::Change {
                path: b"docs/a.md".to_vec(),
                op: ChangeOp::File {
                    blob: doc,
                    size: doc_size,
                },
            },
            braid_format::Change {
                path: b"run.sh".to_vec(),
                op: ChangeOp::Delete,
            },
        ],
        author: grace.clone(),
        committer: grace,
        message: b"Made in braid\n".to_vec(),
        extras: vec![(b"x-braid-note".to_vec(), b"two\nlines".to_vec())],
        git_origin: None,
    }
    .assemble()
    .unwrap();
    let changeset = objects.put(Kind::Changeset, &assembled.encoded).unwrap();

    let (commit, bytes) = export_commit(&objects, &map, &changeset).unwrap();
    assert_eq!(
        export_commit(&objects, &map, &changeset).unwrap(),
        (commit, bytes.clone())
    );
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains(&format!("\nparent {imported}\n")), "{text}");
    assert!(text.contains("author Grace Hopper <grace@example.org> 1700000500 -0500\n"));
    assert!(text.contains("x-braid-note two\n lines\n\nMade in braid\n"));

    let report = write_to_repo(
        repo.path(),
        &objects,
        &map,
        &[RefUpdate {
            name: b"refs/heads/braid".to_vec(),
            target: RefTarget::Changeset(changeset),
        }],
    )
    .unwrap();
    assert_eq!((report.commits, report.trees, report.blobs), (1, 2, 2));
    assert_eq!(
        repo.git(&["rev-parse", "refs/heads/braid"]),
        commit.to_hex()
    );
    repo.git(&["fsck", "--strict", "--no-dangling"]);

    // Git writes the same tree for the same content.
    let same = repo.tree(&[
        file("README", "hello from braid\n"),
        file("docs/a.md", "# A\n"),
        link("latest", "README"),
        submodule("vendor", SUBMODULE_A),
    ]);
    assert_eq!(
        repo.git(&["rev-parse", &format!("{commit}^{{tree}}")]),
        same
    );

    // Importing the exported commit gives a changeset with the same root.
    let again = import_all(&repo, &objects, &mut map);
    assert_eq!(again.commits, 1);
    let reimported = changeset_of(&objects, &map, &commit);
    assert_eq!(reimported.root, root);
    assert!(reimported.git_origin.is_some());
}

fn changeset_of(objects: &MemoryObjects, map: &GitMap, commit: &GitOid) -> Changeset {
    changeset(objects, map, &commit.to_hex())
}

/// Stores the directories holding exactly `files`, regular files given by
/// path and content, and returns the root.
fn store_files(objects: &MemoryObjects, files: &[(&str, &str)]) -> ObjectId {
    fn store_dir(objects: &MemoryObjects, dir: &Dir) -> ObjectId {
        let entries = dir
            .iter()
            .map(|(name, node)| Entry {
                name: name.clone(),
                kind: match node {
                    Node::Leaf(kind) => kind.clone(),
                    Node::Dir(below) => EntryKind::Tree {
                        tree: store_dir(objects, below),
                    },
                },
            })
            .collect();
        store_tree(objects, entries)
    }
    let mut dir = Dir::new();
    let changes: Vec<Change> = files
        .iter()
        .map(|(path, content)| file_change(objects, path, content))
        .collect();
    apply(&mut dir, &changes);
    store_dir(objects, &dir)
}

fn file_change(objects: &MemoryObjects, path: &str, content: &str) -> Change {
    let (blob, size) = store_blob(objects, content.as_bytes());
    Change {
        path: path.as_bytes().to_vec(),
        op: ChangeOp::File { blob, size },
    }
}

/// A changeset made in braid on `parent`, whose tree holds exactly `files`
/// and whose change list is `changes`.
fn braid_changeset(
    objects: &MemoryObjects,
    parent: ObjectId,
    files: &[(&str, &str)],
    changes: Vec<Change>,
    message: &str,
) -> ObjectId {
    let grace = Signature {
        name: b"Grace Hopper".to_vec(),
        email: b"grace@example.org".to_vec(),
        seconds: 1_700_000_500,
        offset_minutes: -300,
    };
    let assembled = ChangesetParts {
        parents: vec![parent],
        root: store_files(objects, files),
        changes,
        author: grace.clone(),
        committer: grace,
        message: message.as_bytes().to_vec(),
        extras: Vec::new(),
        git_origin: None,
    }
    .assemble()
    .unwrap();
    objects.put(Kind::Changeset, &assembled.encoded).unwrap()
}

#[test]
fn exported_commits_import_as_the_changesets_they_came_from() {
    let mut repo = Repo::new();
    let imported = repo.snapshot(
        "main",
        &[file("README", "hi\n"), file("src/a.rs", "a\n")],
        "imported",
    );
    let objects = MemoryObjects::new();
    let mut map = GitMap::new();
    import_all(&repo, &objects, &mut map);

    // Two changesets made in braid, one on top of the other.
    let base = map.changeset(&oid(&imported)).unwrap();
    let first_files = [("README", "hello from braid\n"), ("src/a.rs", "a\n")];
    let first = braid_changeset(
        &objects,
        base,
        &first_files,
        vec![file_change(&objects, "README", "hello from braid\n")],
        "one\n",
    );
    let second_files = [
        ("README", "hello from braid\n"),
        ("docs/b.md", "# B\n"),
        ("src/a.rs", "a\n"),
    ];
    let second = braid_changeset(
        &objects,
        first,
        &second_files,
        vec![file_change(&objects, "docs/b.md", "# B\n")],
        "two\n",
    );

    // Exporting them records their commits, trees and blobs in the map.
    let before = map.counts();
    let report = write_to_repo_recording(
        repo.path(),
        &objects,
        &mut map,
        &[RefUpdate {
            name: b"refs/heads/main".to_vec(),
            target: RefTarget::Changeset(second),
        }],
    )
    .unwrap();
    assert_eq!((report.commits, report.trees, report.blobs), (2, 3, 2));
    let tip = report.refs[0].1;
    assert_eq!(map.changeset(&tip), Some(second));
    assert_eq!(map.commit(&second), Some(tip));
    let first_commit = map.commit(&first).unwrap();
    assert_eq!(repo.git(&["rev-parse", "main~1"]), first_commit.to_hex());
    let after = map.counts();
    assert_eq!(after.commits, before.commits + 2);
    assert_eq!(after.trees, before.trees + 3);
    assert_eq!(after.blobs, before.blobs + 2);
    // The plain export of the same changesets records nothing, and agrees.
    let plain = write_to_repo(
        repo.path(),
        &objects,
        &map,
        &[RefUpdate {
            name: b"refs/heads/again".to_vec(),
            target: RefTarget::Changeset(second),
        }],
    )
    .unwrap();
    assert_eq!(plain.refs[0].1, tip);
    assert_eq!((plain.commits, plain.trees, plain.blobs), (0, 0, 0));

    // Importing the exported commits finds every object in the map: nothing
    // new, and the branch is the changeset made in braid.
    let again = import_all(&repo, &objects, &mut map);
    assert_eq!(
        (again.commits, again.trees, again.blobs, again.raw_trees),
        (0, 0, 0, 0)
    );
    let main = again
        .refs
        .iter()
        .find(|r| r.name == b"refs/heads/main")
        .unwrap();
    assert_eq!(main.braid, second);

    // A commit made in git on top of them builds on the braid changeset.
    let mut next_files: Vec<_> = second_files.iter().map(|(p, c)| file(p, c)).collect();
    next_files.push(file("c", "from git\n"));
    let next = repo.snapshot("main", &next_files, "made in git");
    let later = import_all(&repo, &objects, &mut map);
    assert_eq!(later.commits, 1);
    let next_changeset = changeset(&objects, &map, &next);
    assert_eq!(next_changeset.parents, [second]);
    assert_eq!(changes(&objects, &map, &next), pairs(&[("c", "file")]));

    // The map still encodes, and everything round-trips.
    assert_eq!(GitMap::from_bytes(&map.to_bytes()).unwrap(), map);
    assert_admissible(&objects, &map);
    round_trip(&repo);
}

#[test]
fn a_changeset_that_disagrees_with_its_origin_is_not_exported() {
    let mut repo = Repo::new();
    let first = repo.snapshot("main", &[file("a", "1")], "one");
    let second = repo.snapshot("main", &[file("a", "2")], "two");
    let objects = MemoryObjects::new();
    let mut map = GitMap::new();
    import_all(&repo, &objects, &mut map);
    let genuine = changeset(&objects, &map, &second);
    let first_changeset = changeset(&objects, &map, &first);

    let mut wrong_root = genuine.clone();
    wrong_root.root = first_changeset.root;
    let mut wrong_parents = genuine.clone();
    wrong_parents.parents = Vec::new();
    for forged in [wrong_root, wrong_parents] {
        let id = objects.put(Kind::Changeset, &forged.encode()).unwrap();
        let err = export_commit(&objects, &map, &id).unwrap_err();
        assert!(matches!(err, Error::OriginMismatch { .. }), "{err}");
    }
    let genuine_id = map.changeset(&oid(&second)).unwrap();
    assert_eq!(
        export_commit(&objects, &map, &genuine_id).unwrap().0,
        oid(&second)
    );
}

/// The git id of the empty tree.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[test]
fn the_empty_tree_need_not_be_stored() {
    // Git treats the empty tree as always present and does not store it when
    // a commit uses it; the import and the round trip must cope.
    let mut repo = Repo::new();
    let empty = repo.commit(EMPTY_TREE, &[], "nothing yet");
    repo.set_ref("refs/heads/main", &empty);
    let stored = repo
        .path()
        .join("objects/4b/825dc642cb6eb9a060e54bf8d69288fbee4904");
    assert!(!stored.exists(), "git did not store the empty tree");
    let (objects, map) = check(&mut repo, |repo| {
        repo.snapshot("main", &[file("a", "1")], "something");
        1
    });
    assert!(changes(&objects, &map, &empty).is_empty());
    assert!(map.raw_tree(&oid(EMPTY_TREE)).is_none());
}

#[test]
fn empty_directories_are_left_out() {
    let mut repo = Repo::new();
    let f = repo.blob(b"f\n");
    let g = repo.blob(b"g\n");
    // An empty directory beside a file.
    let single = repo.mktree(&[
        ("100644", "blob", &f, b"f"),
        ("40000", "tree", EMPTY_TREE, b"empty"),
    ]);
    // A chain of them: a/b/c, where c is the empty tree.
    let b = repo.mktree(&[("40000", "tree", EMPTY_TREE, b"c")]);
    let a = repo.mktree(&[("40000", "tree", &b, b"b")]);
    // A directory with a file and an empty directory.
    let sub = repo.mktree(&[
        ("100644", "blob", &g, b"g"),
        ("40000", "tree", EMPTY_TREE, b"hollow"),
    ]);
    let root = repo.mktree(&[
        ("100644", "blob", &f, b"f"),
        ("40000", "tree", &single, b"single"),
        ("40000", "tree", &a, b"a"),
        ("40000", "tree", &sub, b"sub"),
    ]);
    let first = repo.commit(&root, &[], "empty directories");
    // A root holding nothing but empty directories is the empty tree.
    let hollow_root = repo.mktree(&[
        ("40000", "tree", &a, b"a"),
        ("40000", "tree", EMPTY_TREE, b"e"),
    ]);
    let hollow = repo.commit(&hollow_root, &[&first], "only empty directories");
    repo.set_ref("refs/heads/main", &hollow);

    let (objects, map) = check(&mut repo, |repo| {
        let again = repo.mktree(&[("100644", "blob", &g, b"g"), ("40000", "tree", &b, b"b")]);
        let next = repo.commit(&again, &[&hollow], "a file again");
        repo.set_ref("refs/heads/main", &next);
        1
    });
    assert_eq!(
        changes(&objects, &map, &first),
        pairs(&[("f", "file"), ("single/f", "file"), ("sub/g", "file")])
    );
    assert_eq!(
        changes(&objects, &map, &hollow),
        pairs(&[("f", "delete"), ("single/f", "delete"), ("sub/g", "delete")])
    );
    let empty = map.tree(&oid(EMPTY_TREE)).unwrap();
    assert_eq!(changeset(&objects, &map, &hollow).root, empty);
    assert_eq!(map.tree(&oid(&b)), Some(empty));
    assert_eq!(map.tree(&oid(&a)), Some(empty));
    // Every git tree that lost an entry keeps its bytes; the empty tree
    // itself is canonical.
    for tree in [&single, &b, &a, &sub, &root, &hollow_root] {
        assert!(map.raw_tree(&oid(tree)).is_some(), "{tree}");
    }
    assert!(map.raw_tree(&oid(EMPTY_TREE)).is_none());
}

#[test]
fn a_commit_that_only_adds_or_removes_an_empty_directory() {
    let mut repo = Repo::new();
    let f = repo.blob(b"f\n");
    let plain = repo.mktree(&[("100644", "blob", &f, b"f")]);
    let base = repo.commit(&plain, &[], "a file");
    let with_empty = repo.mktree(&[
        ("100644", "blob", &f, b"f"),
        ("40000", "tree", EMPTY_TREE, b"empty"),
    ]);
    let add = repo.commit(&with_empty, &[&base], "add an empty directory");
    let inner = repo.mktree(&[("40000", "tree", EMPTY_TREE, b"deeper")]);
    let nested = repo.mktree(&[
        ("100644", "blob", &f, b"f"),
        ("40000", "tree", &inner, b"empty"),
    ]);
    let deepen = repo.commit(&nested, &[&add], "nest another one inside");
    let remove = repo.commit(&plain, &[&deepen], "remove it");
    repo.set_ref("refs/heads/main", &remove);

    let (objects, map) = check(&mut repo, |repo| {
        let again = repo.commit(&with_empty, &[&remove], "add it again");
        repo.set_ref("refs/heads/main", &again);
        1
    });
    let root = changeset(&objects, &map, &base).root;
    for (commit, parent) in [(&add, &base), (&deepen, &add), (&remove, &deepen)] {
        assert!(changes(&objects, &map, commit).is_empty(), "{commit}");
        let changeset = changeset(&objects, &map, commit);
        assert_eq!(changeset.root, root, "{commit}");
        assert_eq!(changeset.parents, [map.changeset(&oid(parent)).unwrap()]);
        // Still bit for bit.
        let (exported, bytes) =
            export_commit(&objects, &map, &map.changeset(&oid(commit)).unwrap()).unwrap();
        assert_eq!(exported, oid(commit));
        assert_eq!(bytes, repo.git_bytes(&["cat-file", "commit", commit]));
    }
}
