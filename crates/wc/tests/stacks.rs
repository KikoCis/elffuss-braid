// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Stacks landed the way a hub lands them, and a workspace landing in two
//! steps: what to land, then moving onto the head that holds it.

mod common;

use std::fs;

use braid_format::ObjectId;
use braid_wc::{CommitRequest, Error, Landed, StackMode, Workspace, land_stack};
use common::{Fixture, ada};

fn commit_all(ws: &Workspace<'_>, message: &str) -> ObjectId {
    ws.commit(&CommitRequest {
        message: Some(message.as_bytes().to_vec()),
        paths: Vec::new(),
        author: None,
        committer: ada(),
        agent: "tester".to_owned(),
        force: false,
    })
    .unwrap()
    .changeset
    .unwrap()
}

fn land(f: &Fixture, stack: &[ObjectId], mode: StackMode) -> braid_wc::LandReport {
    land_stack(&f.repo, "main", stack, mode, "hub", &ada()).unwrap()
}

/// The first parents from `id` down, `id` first.
fn chain(f: &Fixture, id: ObjectId) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let mut current = Some(id);
    while let Some(id) = current {
        out.push(id);
        current = braid_engine::read_changeset(f.repo.store(), id)
            .unwrap()
            .parents
            .first()
            .copied();
    }
    out
}

#[test]
fn a_stack_rebases_whole_or_stops_whole() {
    let f = Fixture::new();
    let seed = f.commit(&[], vec![("a", f.file("a\n")), ("x", f.file("1\n2\n3\n"))]);
    f.set_branch("main", seed);
    let one = f.commit(&[seed], vec![("b", f.file("b\n"))]);
    let two = f.commit(&[one], vec![("c", f.file("c\n"))]);
    let three = f.commit(&[two], vec![("d", f.file("d\n"))]);
    // Someone else landed meanwhile.
    let theirs = f.commit(&[seed], vec![("a", f.file("a, theirs\n"))]);
    f.set_branch("main", theirs);

    let report = land(&f, &[one, two, three], StackMode::Rebase);
    let Landed::Rebased(tip) = report.landed else {
        panic!("expected a rebase, got {:?}", report.landed);
    };
    assert_eq!(report.head_before, Some(theirs));
    assert_eq!(f.head("main"), Some(tip));
    assert!(report.operation.is_some());
    assert_eq!(report.placed.len(), 3);
    assert!(report.placed.iter().all(|p| p.to.is_some()));
    assert_eq!(report.placed[2].to, Some(tip));
    // The three, rebased in order, on top of theirs.
    let main = chain(&f, tip);
    assert_eq!(main[3], theirs);
    assert_eq!(main[4], seed);

    // The same stack again, as after a lost answer: nothing moves.
    let again = land(&f, &[one, two, three], StackMode::Rebase);
    assert_eq!(again.landed, Landed::AlreadyApplied);
    assert_eq!(again.operation, None);
    assert_eq!(f.head("main"), Some(tip));

    // A stack whose second changeset conflicts with main: the first is
    // clean, but nothing lands.
    let clean = f.commit(&[seed], vec![("e", f.file("e\n"))]);
    let clash = f.commit(&[clean], vec![("a", f.file("a, ours\n"))]);
    let report = land(&f, &[clean, clash], StackMode::Rebase);
    let Landed::Conflicts { change, conflicts } = &report.landed else {
        panic!("expected conflicts, got {:?}", report.landed);
    };
    assert_eq!(*change, clash);
    assert_eq!(conflicts[0].path, b"a");
    assert_eq!(report.operation, None);
    assert!(report.placed.iter().all(|p| p.to.is_none()));
    assert_eq!(f.head("main"), Some(tip));
}

#[test]
fn fast_forward_only_moves_or_refuses() {
    let f = Fixture::new();
    let seed = f.commit(&[], vec![("a", f.file("a\n"))]);
    let one = f.commit(&[seed], vec![("b", f.file("b\n"))]);
    let two = f.commit(&[one], vec![("c", f.file("c\n"))]);

    // A branch that does not exist is created at the last changeset, whatever
    // the first one's parent.
    let report = land(&f, &[one, two], StackMode::FastForwardOnly);
    assert_eq!(report.landed, Landed::FastForward(two));
    assert_eq!(report.head_before, None);
    assert_eq!(f.head("main"), Some(two));

    // On the head: a fast-forward.
    let three = f.commit(&[two], vec![("d", f.file("d\n"))]);
    let four = f.commit(&[three], vec![("e", f.file("e\n"))]);
    let report = land(&f, &[three], StackMode::FastForwardOnly);
    assert_eq!(report.landed, Landed::FastForward(three));
    // A stack holding the head goes on from it.
    let report = land(&f, &[two, three, four], StackMode::FastForwardOnly);
    assert_eq!(report.landed, Landed::FastForward(four));
    let went: Vec<Option<ObjectId>> = report.placed.iter().map(|p| p.to).collect();
    assert_eq!(went, [None, None, Some(four)]);
    assert_eq!(f.head("main"), Some(four));

    // Again, and an older stack: the branch has them.
    for stack in [&[three, four][..], &[one, two][..]] {
        let report = land(&f, stack, StackMode::FastForwardOnly);
        assert_eq!(report.landed, Landed::AlreadyApplied, "{stack:?}");
        assert_eq!(report.operation, None);
    }

    // Built on an older changeset: refused, and main stays.
    let late = f.commit(&[two], vec![("f", f.file("f\n"))]);
    let report = land(&f, &[late], StackMode::FastForwardOnly);
    assert_eq!(report.landed, Landed::Refused);
    assert_eq!(report.head(), Some(four));
    assert_eq!(report.operation, None);
    assert_eq!(f.head("main"), Some(four));
    // The same in rebase mode lands it.
    let report = land(&f, &[late], StackMode::Rebase);
    assert!(matches!(report.landed, Landed::Rebased(_)), "{report:?}");
}

#[test]
fn what_is_not_a_stack_is_refused() {
    let f = Fixture::new();
    let seed = f.commit(&[], vec![("a", f.file("a\n"))]);
    f.set_branch("main", seed);
    let one = f.commit(&[seed], vec![("b", f.file("b\n"))]);
    let other = f.commit(&[seed], vec![("c", f.file("c\n"))]);
    for mode in [StackMode::Rebase, StackMode::FastForwardOnly] {
        assert!(matches!(
            land_stack(&f.repo, "main", &[], mode, "hub", &ada()),
            Err(Error::EmptyStack)
        ));
        assert!(matches!(
            land_stack(&f.repo, "main", &[one, other], mode, "hub", &ada()),
            Err(Error::NotAStack { index: 1 })
        ));
    }
    // Rebasing needs the base on the branch's first-parent chain.
    let elsewhere = f.commit(&[], vec![("z", f.file("z\n"))]);
    let on_it = f.commit(&[elsewhere], vec![("y", f.file("y\n"))]);
    assert!(matches!(
        land_stack(&f.repo, "main", &[on_it], StackMode::Rebase, "hub", &ada()),
        Err(Error::NotOnBranch { .. })
    ));
    assert_eq!(f.head("main"), Some(seed));
}

#[test]
fn a_workspace_lands_in_two_steps() {
    let f = Fixture::new();
    let seed = f.commit(&[], vec![("a", f.file("a\n"))]);
    f.set_branch("main", seed);
    let dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &dir, Some(seed), "main").unwrap();
    fs::write(dir.join("b"), "b\n").unwrap();
    let first = commit_all(&ws, "first");
    fs::write(dir.join("c"), "c\n").unwrap();
    let second = commit_all(&ws, "second");

    let stack = ws.stack(None).unwrap();
    assert_eq!(stack.branch, "main");
    assert_eq!(stack.head, Some(seed));
    assert_eq!(stack.base, Some(second));
    assert_eq!(stack.changes, [first, second]);

    // Elsewhere, main moves and the stack lands on it, rebased.
    let theirs = f.commit(&[seed], vec![("t", f.file("t\n"))]);
    f.set_branch("main", theirs);
    let report = land(&f, &stack.changes, StackMode::Rebase);
    let Landed::Rebased(tip) = report.landed else {
        panic!("expected a rebase, got {:?}", report.landed);
    };

    // Not while the working copy has changes, nor from another base.
    fs::write(dir.join("b"), "b, edited\n").unwrap();
    assert!(matches!(
        ws.adopt(stack.base, tip),
        Err(Error::Uncommitted { .. })
    ));
    assert!(matches!(ws.stack(None), Err(Error::Uncommitted { .. })));
    fs::write(dir.join("b"), "b\n").unwrap();
    assert!(matches!(
        ws.adopt(Some(first), tip),
        Err(Error::BaseMoved { .. })
    ));

    let written = ws.adopt(stack.base, tip).unwrap();
    assert_eq!(written, [b"t".to_vec()]);
    assert_eq!(fs::read(dir.join("t")).unwrap(), b"t\n");
    let state = ws.state().unwrap();
    assert_eq!(state.base, Some(tip));
    assert_eq!(state.upstream, Some(tip));
    let status = ws.status().unwrap();
    assert!(status.status.is_clean());
    assert!(status.unlanded.is_empty());
    assert!(!status.behind());
    // Adopting the head it is on does nothing.
    assert!(ws.adopt(Some(tip), tip).unwrap().is_empty());
}
