// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Round trips of repositories built for one feature each, the import's
//! refusals, and the export of changesets made in braid.

mod common;

use braid_format::changeset::decode_change_list;
use braid_format::{
    ChangeOp, Changes, Changeset, ChangesetParts, Entry, EntryKind, GitId, Kind, MemoryObjects,
    ObjectId, Objects, Signature, build_directory,
};
use braid_git::{
    Error, GitKind, GitMap, GitOid, ImportOptions, RefTarget, RefUpdate, RoundTripReport,
    export_commit, export_object, import, verify_round_trip, write_to_repo,
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

/// The checks every fixture goes through: the round trip holds; importing
/// imports every commit; importing again with the saved map imports nothing;
/// after `extend` adds commits, importing brings in exactly those; and the
/// round trip still holds.
fn check(repo: &mut Repo, extend: impl FnOnce(&mut Repo) -> usize) -> (MemoryObjects, GitMap) {
    round_trip(repo);
    let objects = MemoryObjects::new();
    let mut map = GitMap::new();
    let first = import_all(repo, &objects, &mut map);
    assert_eq!(first.commits, repo.count_commits());
    assert_eq!(map.counts().commits, first.commits);

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
    round_trip(repo);
    (objects, map)
}

fn changeset(objects: &MemoryObjects, map: &GitMap, commit: &str) -> Changeset {
    let id = map.changeset(&oid(commit)).expect("commit imported");
    let (kind, bytes) = objects.get(&id).unwrap().unwrap();
    assert_eq!(kind, Kind::Changeset);
    Changeset::decode(&bytes).unwrap()
}

/// The change list of an imported commit, as path and operation.
fn changes(objects: &MemoryObjects, map: &GitMap, commit: &str) -> Vec<(String, &'static str)> {
    let list = match changeset(objects, map, commit).changes {
        Changes::Inline(list) => list,
        Changes::External { list, .. } => {
            decode_change_list(&objects.get(&list).unwrap().unwrap().1).unwrap()
        }
    };
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

#[test]
fn the_empty_tree_need_not_be_stored() {
    // Git treats the empty tree as always present and does not store it when
    // a commit uses it; the import and the round trip must cope.
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
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
