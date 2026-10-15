// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Workspaces through the library: refusals before anything is written,
//! recovery of an interrupted checkout, rebases of stacks, and the errors
//! agents get when they ask for the wrong thing.

mod common;

use std::fs;

use braid_format::{ChangeOp, ObjectId};
use braid_wc::{ChangeKind, CommitRequest, Error, Landed, RebaseProgress, Updated, Workspace};
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
