// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Workspaces through the library: refusals before anything is written,
//! recovery of an interrupted checkout, rebases of stacks, and the errors
//! agents get when they ask for the wrong thing.

mod common;

use std::fs;

use braid_format::{ChangeOp, Kind, ObjectId, Objects};
use braid_wc::{
    Blocker, ChangeKind, CommitRequest, Error, Landed, RebaseProgress, Updated, Workspace,
};
use common::{Fixture, ada};

fn commit_all(ws: &Workspace<'_>, message: &str) -> braid_wc::CommitOutcome {
    ws.commit(&CommitRequest {
        message: Some(message.as_bytes().to_vec()),
        paths: Vec::new(),
        author: None,
        committer: ada(),
        agent: "tester".to_owned(),
        force: false,
    })
    .unwrap()
}

/// Bytes that do not compress.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

#[test]
fn commits_keep_edits_as_deltas_and_large_files_in_chunks() {
    let f = Fixture::new();
    let mut state = 7u64;
    let text: String = (0..3000)
        .map(|i| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            format!("line {i}: {state:016x}\n")
        })
        .collect();
    let first = f.commit(&[], vec![("src/lib.rs", f.file(&text))]);
    f.set_branch("main", first);
    let dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &dir, Some(first), "main").unwrap();

    let edited = text.replacen("line 1500:", "line 1500, edited:", 1);
    fs::write(dir.join("src/lib.rs"), &edited).unwrap();
    // New files over 1 MiB wait for `braid track`.
    let big = noise((6 << 20) + 123);
    fs::write(dir.join("model.bin"), &big).unwrap();
    ws.track(&[b"model.bin".to_vec()]).unwrap();
    let changeset = commit_all(&ws, "edit, and add a model").changeset.unwrap();

    let store = f.repo.store();
    let report = store.fsck().unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    // The edited file is a delta against the one it replaces.
    assert!(report.delta_records >= 1, "{report:?}");
    assert_eq!(report.chunked_records, 1, "{report:?}");
    let blob = ObjectId::hash(Kind::Blob, &big);
    assert_eq!(
        store.stat(&blob).unwrap(),
        Some((Kind::Blob, big.len() as u64))
    );

    // Another workspace gets both back.
    let other = f.path("other");
    Workspace::create(&f.repo, "other", &other, Some(changeset), "main").unwrap();
    assert!(fs::read(other.join("model.bin")).unwrap() == big);
    assert_eq!(
        fs::read_to_string(other.join("src/lib.rs")).unwrap(),
        edited
    );
}

#[test]
fn a_large_file_that_does_not_read_back_is_never_checked_out() {
    let f = Fixture::new();
    let first = f.commit(&[], vec![("README", f.file("hello\n"))]);
    f.set_branch("main", first);
    let dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &dir, Some(first), "main").unwrap();

    let big = noise((5 << 20) + 9);
    let blob = f.repo.store().put(Kind::Blob, &big).unwrap();
    let second = f.commit(
        &[first],
        vec![(
            "model.bin",
            ChangeOp::File {
                blob,
                size: big.len() as u64,
            },
        )],
    );
    f.set_branch("main", second);

    // Damage one chunk: chunks of random bytes are stored raw.
    let needle = &big[3 << 20..(3 << 20) + 64];
    let mut damaged = false;
    for entry in fs::read_dir(f.repo.store().root().join("seg")).unwrap() {
        let path = entry.unwrap().path();
        let mut bytes = fs::read(&path).unwrap();
        if let Some(at) = bytes.windows(64).position(|window| window == needle) {
            bytes[at + 7] ^= 0x20;
            fs::write(&path, bytes).unwrap();
            damaged = true;
        }
    }
    assert!(damaged);

    let result = ws.update(&ada());
    assert!(
        matches!(
            result,
            Err(Error::Store(braid_store::Error::Corrupt { .. }))
        ),
        "{result:?}"
    );
    // Neither the file nor a half-written scratch copy of it is left.
    assert!(!dir.join("model.bin").exists());
    assert_eq!(fs::read(dir.join("README")).unwrap(), b"hello\n");
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().contains("tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_file_replacing_a_directory_with_untracked_files_is_refused() {
    let f = Fixture::new();
    let first = f.commit(
        &[],
        vec![(".gitignore", f.file("d/junk\n")), ("d/a", f.file("a"))],
    );
    f.set_branch("main", first);
    let dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &dir, Some(first), "main").unwrap();
    fs::write(dir.join("d/junk"), "mine").unwrap();
    assert!(ws.status().unwrap().status.is_clean());

    let second = f.commit(
        &[first],
        vec![("d/a", ChangeOp::Delete), ("d", f.file("now a file"))],
    );
    f.set_branch("main", second);
    let updated = ws.update(&ada());
    assert!(
        matches!(&updated, Err(Error::Unsafe { path, .. }) if path == b"d/junk"),
        "{updated:?}"
    );
    // Refused before anything was written.
    assert_eq!(fs::read(dir.join("d/a")).unwrap(), b"a");
    assert_eq!(fs::read(dir.join("d/junk")).unwrap(), b"mine");
    assert_eq!(ws.state().unwrap().base, Some(first));

    // Once the file is out of the way, the update goes through.
    fs::remove_file(dir.join("d/junk")).unwrap();
    assert_eq!(
        ws.update(&ada()).unwrap().updated,
        Updated::Moved { head: second }
    );
    assert_eq!(fs::read(dir.join("d")).unwrap(), b"now a file");
}

#[test]
fn an_interrupted_checkout_is_finished_by_the_next_command() {
    let f = Fixture::new();
    let first = f.commit(&[], vec![("a", f.file("1")), ("gone", f.file("x"))]);
    let second = f.commit(
        &[first],
        vec![
            ("a", f.file("2")),
            ("gone", ChangeOp::Delete),
            ("new/b", f.file("b")),
        ],
    );
    f.set_branch("main", second);
    let dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &dir, Some(first), "main").unwrap();

    // As if a command had moved the base, written part of the checkout
    // and died: `a` is written, the rest is not.
    let state_file = f
        .repo
        .root()
        .join(".braid")
        .join("ws")
        .join("ws")
        .join("state");
    let text = fs::read_to_string(&state_file).unwrap();
    let root = |id| {
        braid_engine::read_changeset(f.repo.store(), id)
            .unwrap()
            .root
    };
    let text = text.replace(
        &format!("base {first}"),
        &format!("base {second}\ncheckout {} {}", root(first), root(second)),
    );
    fs::write(&state_file, text).unwrap();
    fs::write(dir.join("a"), "2").unwrap();

    let report = ws.status().unwrap();
    assert!(report.status.is_clean(), "{:?}", report.status);
    assert_eq!(report.state.checkout, None);
    assert_eq!(report.state.base, Some(second));
    assert!(!dir.join("gone").exists());
    assert_eq!(fs::read(dir.join("new/b")).unwrap(), b"b");
}

#[test]
fn a_stack_that_conflicts_half_way_continues_after_the_commit() {
    let f = Fixture::new();
    let seed = f.commit(&[], vec![("x", f.file("1\n2\n3\n")), ("y", f.file("y\n"))]);
    f.set_branch("main", seed);
    let dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &dir, Some(seed), "main").unwrap();
    // Two changesets of our own: the first is clean against main, the
    // second conflicts with it.
    fs::write(dir.join("y"), "y, ours\n").unwrap();
    let first = commit_all(&ws, "first").changeset.unwrap();
    fs::write(dir.join("x"), "1\nours\n3\n").unwrap();
    let second = commit_all(&ws, "second").changeset.unwrap();
    fs::write(dir.join("z"), "z\n").unwrap();
    let third = commit_all(&ws, "third").changeset.unwrap();
    // Main moves meanwhile.
    let theirs = f.commit(&[seed], vec![("x", f.file("1\ntheirs\n3\n"))]);
    f.set_branch("main", theirs);

    let report = ws.update(&ada()).unwrap();
    let Updated::Conflicts {
        change, conflicts, ..
    } = report.updated
    else {
        panic!("expected conflicts");
    };
    assert_eq!(change, second);
    assert_eq!(conflicts[0].kind.code(), "CONTENT");
    let state = ws.state().unwrap();
    let rebase = state.rebase.unwrap();
    assert_eq!(rebase.pending, [third]);
    // The first changeset went onto main's head, the others nowhere yet;
    // the working copy got main's x with markers, and lost z for now.
    assert_eq!(report.head, Some(theirs));
    assert_eq!(report.base_before, Some(third));
    assert_eq!(report.base, Some(rebase.onto));
    let placed: Vec<(ObjectId, Option<ObjectId>)> =
        report.placed.iter().map(|p| (p.from, p.to)).collect();
    assert_eq!(
        placed,
        [(first, Some(rebase.onto)), (second, None), (third, None)]
    );
    assert_eq!(report.updated_paths, [b"x".to_vec(), b"z".to_vec()]);
    // The working copy has the first changeset rebased, and the markers;
    // the third one waits.
    assert_eq!(fs::read(dir.join("y")).unwrap(), b"y, ours\n");
    assert!(!dir.join("z").exists());
    let merged = fs::read_to_string(dir.join("x")).unwrap();
    assert!(merged.contains("<<<<<<< "), "{merged}");

    // Neither landing nor updating while the rebase lasts.
    assert!(matches!(
        ws.land(None, "t", &ada()),
        Err(Error::RebaseInProgress)
    ));
    assert!(matches!(ws.update(&ada()), Err(Error::RebaseInProgress)));
    // Paths are refused: the resolution is recorded whole.
    let partial = ws.commit(&CommitRequest {
        message: None,
        paths: vec![b"x".to_vec()],
        author: None,
        committer: ada(),
        agent: "tester".to_owned(),
        force: false,
    });
    assert!(
        matches!(partial, Err(Error::PathsDuringRebase)),
        "{partial:?}"
    );

    fs::write(dir.join("x"), "1\nboth\n3\n").unwrap();
    let outcome = ws
        .commit(&CommitRequest {
            message: None,
            paths: Vec::new(),
            author: None,
            committer: ada(),
            agent: "tester".to_owned(),
            force: false,
        })
        .unwrap();
    let RebaseProgress::Finished { tip } = outcome.rebase else {
        panic!("expected the rebase to finish, got {:?}", outcome.rebase);
    };
    // The third changeset followed, and the workspace is clean on top.
    assert_eq!(fs::read(dir.join("z")).unwrap(), b"z\n");
    assert_eq!(ws.state().unwrap().base, Some(tip));
    let report = ws.status().unwrap();
    assert!(report.status.is_clean());
    assert_eq!(report.unlanded.len(), 3);

    let landed = ws.land(None, "t", &ada()).unwrap();
    assert_eq!(landed.landed, Landed::FastForward(tip));
    // Main has, in order: theirs, then first, second and third, each
    // naming the changeset it came from; the resolution keeps the
    // message of the change it resolves.
    let mut messages = Vec::new();
    let mut origins = Vec::new();
    let mut current = f.head("main");
    while let Some(id) = current {
        let changeset = braid_engine::read_changeset(f.repo.store(), id).unwrap();
        messages.push(String::from_utf8(changeset.message.clone()).unwrap());
        origins.push(
            changeset
                .extras
                .iter()
                .find(|(key, _)| key.as_slice() == braid_engine::REBASED_FROM)
                .map(|(_, value)| String::from_utf8(value.clone()).unwrap()),
        );
        current = changeset.parents.first().copied();
    }
    assert_eq!(
        messages,
        ["third", "second", "first", "test change\n", "test change\n"]
    );
    let hex = |id: braid_format::ObjectId| Some(id.to_hex());
    assert_eq!(origins[..3], [hex(third), hex(second), hex(first)]);
}

#[test]
fn files_a_rebased_change_tracked_stay_in_its_resolution() {
    let f = Fixture::new();
    let seed = f.commit(&[], vec![("x", f.file("1\n2\n3\n"))]);
    f.set_branch("main", seed);
    let dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &dir, Some(seed), "main").unwrap();
    // One changeset adds a file the secrets guard holds back, once tracked,
    // and edits a line that main edits too.
    fs::write(dir.join(".env.example"), "KEY=\n").unwrap();
    ws.track(&[b".env.example".to_vec()]).unwrap();
    fs::write(dir.join("x"), "1\nours\n3\n").unwrap();
    commit_all(&ws, "env and x");
    assert!(ws.state().unwrap().tracked.is_empty());
    let theirs = f.commit(&[seed], vec![("x", f.file("1\ntheirs\n3\n"))]);
    f.set_branch("main", theirs);

    let report = ws.update(&ada()).unwrap();
    assert!(matches!(report.updated, Updated::Conflicts { .. }));
    // The file is back, and does not wait for `braid track` again: only the
    // markers stop the resolution.
    assert_eq!(fs::read(dir.join(".env.example")).unwrap(), b"KEY=\n");
    let status = ws.status().unwrap();
    assert!(status.status.needs_track.is_empty());
    assert_eq!(status.blocking, [Blocker::Markers(b"x".to_vec())]);
    fs::write(dir.join("x"), "1\nboth\n3\n").unwrap();
    let resolution = ws
        .commit(&CommitRequest {
            message: None,
            paths: Vec::new(),
            author: None,
            committer: ada(),
            agent: "tester".to_owned(),
            force: false,
        })
        .unwrap()
        .changeset
        .unwrap();
    let root = braid_engine::read_changeset(f.repo.store(), resolution)
        .unwrap()
        .root;
    let kept = braid_engine::entry_at(f.repo.store(), root, b".env.example").unwrap();
    assert!(kept.is_some());
    assert!(ws.state().unwrap().tracked.is_empty());
}

#[test]
fn tracked_files_count_even_when_ignored() {
    let f = Fixture::new();
    let first = f.commit(
        &[],
        vec![(".gitignore", f.file("*.log\n")), ("kept.log", f.file("1"))],
    );
    let dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &dir, Some(first), "main").unwrap();
    fs::write(dir.join("kept.log"), "2").unwrap();
    fs::write(dir.join("new.log"), "ignored").unwrap();
    let status = ws.status().unwrap().status;
    let changes: Vec<(&[u8], ChangeKind)> = status
        .changes
        .iter()
        .map(|c| (c.path.as_slice(), c.kind))
        .collect();
    assert_eq!(changes, [(&b"kept.log"[..], ChangeKind::Modified)]);
}

#[test]
fn refusals() {
    let f = Fixture::new();
    let dir = f.path("ws");
    for bad in ["", ".hidden", "a/b", "../up"] {
        assert!(matches!(
            Workspace::create(&f.repo, bad, &dir, None, "main"),
            Err(Error::InvalidName(_))
        ));
    }
    let full = f.path("full");
    fs::create_dir(&full).unwrap();
    fs::write(full.join("there"), "x").unwrap();
    assert!(matches!(
        Workspace::create(&f.repo, "ws", &full, None, "main"),
        Err(Error::DirectoryNotEmpty(_))
    ));
    // A refused creation leaves no workspace behind, so the name is free.
    let (ws, written) = Workspace::create(&f.repo, "ws", &dir, None, "main").unwrap();
    assert_eq!(written, 0);
    assert!(matches!(
        Workspace::create(&f.repo, "ws", &f.path("other"), None, "main"),
        Err(Error::WorkspaceExists(_))
    ));
    assert!(matches!(
        Workspace::open(&f.repo, "nope"),
        Err(Error::NoSuchWorkspace(_))
    ));
    assert!(matches!(ws.resolve(&[b"x".to_vec()]), Err(Error::NoRebase)));
    assert!(matches!(
        ws.commit(&CommitRequest {
            message: Some(b"m".to_vec()),
            paths: Vec::new(),
            author: None,
            committer: ada(),
            agent: "tester".to_owned(),
            force: false,
        }),
        Err(Error::NothingToCommit)
    ));
    fs::write(dir.join("f"), "x").unwrap();
    assert!(matches!(
        ws.commit(&CommitRequest {
            message: None,
            paths: Vec::new(),
            author: None,
            committer: ada(),
            agent: "tester".to_owned(),
            force: false,
        }),
        Err(Error::NoMessage)
    ));
    // Landing or updating with uncommitted changes.
    assert!(matches!(
        ws.land(None, "t", &ada()),
        Err(Error::Uncommitted { .. })
    ));
    assert!(matches!(ws.update(&ada()), Err(Error::Uncommitted { .. })));
    // Nothing to land before the first commit; then a first landing makes
    // the branch.
    let made = commit_all(&ws, "first").changeset.unwrap();
    let landed = ws.land(None, "t", &ada()).unwrap();
    assert_eq!(landed.landed, Landed::FastForward(made));
    assert_eq!(f.head("main"), Some(made));
    assert_eq!(ws.land(None, "t", &ada()).unwrap().landed, Landed::Nothing);
}

#[cfg(unix)]
#[test]
fn a_commit_of_some_paths_reads_only_those() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let first = f.commit(
        &[],
        vec![
            ("a/x", f.file("1")),
            ("a/y", f.file("2")),
            ("c/d/e", f.file("3")),
            ("locked/z", f.file("4")),
        ],
    );
    f.set_branch("main", first);
    let dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &dir, Some(first), "main").unwrap();
    fs::write(dir.join("a/x"), "edited").unwrap();
    fs::write(dir.join("a/y"), "edited too").unwrap();
    fs::remove_dir_all(dir.join("c")).unwrap();
    fs::write(dir.join("locked/new"), "n").unwrap();
    // A directory that cannot be read: a scan of everything fails on it,
    // unless the tests run with the rights to read it anyway.
    let mode = |m| fs::Permissions::from_mode(m);
    fs::set_permissions(dir.join("locked"), mode(0o000)).unwrap();
    if fs::read_dir(dir.join("locked")).is_err() {
        assert!(matches!(ws.status(), Err(Error::Io { .. })));
    }
    let outcome = ws.commit(&CommitRequest {
        message: Some(b"some paths".to_vec()),
        paths: vec![b"a/x".to_vec(), b"c/d".to_vec()],
        author: None,
        committer: ada(),
        agent: "tester".to_owned(),
        force: false,
    });
    fs::set_permissions(dir.join("locked"), mode(0o755)).unwrap();
    let outcome = outcome.unwrap();
    assert_eq!(outcome.changes, 2);
    let changeset =
        braid_engine::read_changeset(f.repo.store(), outcome.changeset.unwrap()).unwrap();
    let changes =
        braid_engine::read_changes(f.repo.store(), outcome.changeset.unwrap(), &changeset)
            .unwrap()
            .iter()
            .map(|change| {
                (
                    String::from_utf8(change.path.clone()).unwrap(),
                    change.op == ChangeOp::Delete,
                )
            })
            .collect::<Vec<_>>();
    assert_eq!(
        changes,
        [("a/x".to_owned(), false), ("c/d/e".to_owned(), true)]
    );
    // What lay outside the paths is still to commit.
    let status = ws.status().unwrap().status;
    let left: Vec<(&[u8], ChangeKind)> = status
        .changes
        .iter()
        .map(|c| (c.path.as_slice(), c.kind))
        .collect();
    assert_eq!(
        left,
        [
            (&b"a/y"[..], ChangeKind::Modified),
            (b"locked/new", ChangeKind::Added)
        ]
    );
}
