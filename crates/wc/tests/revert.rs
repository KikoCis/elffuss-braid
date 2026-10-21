// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Reverting a changeset: the revert undoes it whole, modes and links
//! included, and lands like any change, rebased onto a head that moved on,
//! with conflicts where later work touched the same lines.

mod common;

use braid_engine::{AGENT, REBASED_FROM, REVERTS, read_changeset};
use braid_format::{ChangeOp, GitId, ObjectId};
use braid_wc::{Error, Landed, RevertRequest, create_revert, land_change};
use common::{Fixture, ada};

fn revert(f: &Fixture, culprit: ObjectId, allow_merge: bool) -> Result<ObjectId, Error> {
    create_revert(
        f.repo.store(),
        &RevertRequest {
            culprit,
            allow_merge,
            author: ada(),
            agent: "reverter".to_owned(),
        },
    )
}

fn land(f: &Fixture, change: ObjectId) -> Landed {
    land_change(f.repo.store(), "main", change, "reverter", &ada())
        .unwrap()
        .landed
}

fn root(f: &Fixture, id: ObjectId) -> ObjectId {
    read_changeset(f.repo.store(), id).unwrap().root
}

fn gitlink(byte: u8) -> ChangeOp {
    ChangeOp::Gitlink {
        commit: GitId::Sha1([byte; 20]),
    }
}

#[test]
fn a_revert_undoes_adds_edits_deletes_and_modes() {
    let f = Fixture::new();
    let base = f.commit(
        &[],
        vec![
            ("edit", f.file("1\n2\n")),
            ("gone", f.file("bye\n")),
            ("run", f.file("echo\n")),
            ("link", f.symlink(b"edit")),
            ("sub", gitlink(1)),
            ("dir/file", f.file("d\n")),
        ],
    );
    f.set_branch("main", base);
    let culprit = f.commit(
        &[base],
        vec![
            ("dir", f.file("now a file\n")),
            ("dir/file", ChangeOp::Delete),
            ("edit", f.file("1\nbad\n")),
            ("gone", ChangeOp::Delete),
            ("link", f.symlink(b"run")),
            ("new/deep", f.file("added\n")),
            ("run", f.exec("echo\n")),
            ("sub", gitlink(2)),
        ],
    );
    assert!(matches!(land(&f, culprit), Landed::FastForward(_)));

    let id = revert(&f, culprit, false).unwrap();
    let made = read_changeset(f.repo.store(), id).unwrap();
    assert_eq!(made.parents, [culprit]);
    assert_eq!(made.root, root(&f, base));
    assert_eq!(
        String::from_utf8(made.message).unwrap(),
        format!("Revert \"test change\"\n\nThis reverts changeset {culprit}.\n")
    );
    assert_eq!(
        made.extras,
        [
            (AGENT.to_vec(), b"reverter".to_vec()),
            (REVERTS.to_vec(), culprit.to_hex().into_bytes()),
        ]
    );
    // Built on the head: the branch fast-forwards to it.
    assert_eq!(land(&f, id), Landed::FastForward(id));
    assert_eq!(root(&f, f.head("main").unwrap()), root(&f, base));
}

#[test]
fn a_revert_rebases_over_later_unrelated_work() {
    let f = Fixture::new();
    let base = f.commit(
        &[],
        vec![("a", f.file("1\n2\n3\n4\n5\n")), ("b", f.file("b\n"))],
    );
    let culprit = f.commit(&[base], vec![("a", f.file("1\n2\nbad\n4\n5\n"))]);
    let later = f.commit(
        &[culprit],
        vec![("a", f.file("one\n2\nbad\n4\n5\n")), ("c", f.file("c\n"))],
    );
    f.set_branch("main", later);

    let id = revert(&f, culprit, false).unwrap();
    let Landed::Rebased(landed) = land(&f, id) else {
        panic!("expected a rebase");
    };
    assert_eq!(f.head("main"), Some(landed));
    let expected = f.commit(&[later], vec![("a", f.file("one\n2\n3\n4\n5\n"))]);
    assert_eq!(root(&f, landed), root(&f, expected));
    // The rebased revert still says what it reverts, and where it came from.
    let extras = read_changeset(f.repo.store(), landed).unwrap().extras;
    assert_eq!(
        extras,
        [
            (AGENT.to_vec(), b"reverter".to_vec()),
            (REVERTS.to_vec(), culprit.to_hex().into_bytes()),
            (REBASED_FROM.to_vec(), id.to_hex().into_bytes()),
        ]
    );
}

#[test]
fn a_revert_that_later_work_conflicts_with_does_not_move_the_branch() {
    let f = Fixture::new();
    let base = f.commit(&[], vec![("a", f.file("1\n2\n3\n"))]);
    let culprit = f.commit(&[base], vec![("a", f.file("1\nbad\n3\n"))]);
    let later = f.commit(&[culprit], vec![("a", f.file("1\nworse\n3\n"))]);
    f.set_branch("main", later);

    let id = revert(&f, culprit, false).unwrap();
    let Landed::Conflicts { change, conflicts } = land(&f, id) else {
        panic!("expected conflicts");
    };
    assert_eq!(change, id);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path, b"a");
    assert_eq!(conflicts[0].kind.code(), "CONTENT");
    assert_eq!(f.head("main"), Some(later));
}

#[test]
fn the_reverted_change_landed_again_is_already_applied() {
    // As the simulation's regression test has it: a change that landed
    // once never lands a second time over its revert.
    let f = Fixture::new();
    let base = f.commit(&[], vec![("f", f.file("1\n")), ("g", f.file("1\n"))]);
    let ours = f.commit(&[base], vec![("g", f.file("2\n"))]);
    let theirs = f.commit(&[base], vec![("f", f.file("2\n"))]);
    f.set_branch("main", ours);
    let Landed::Rebased(rebased) = land(&f, theirs) else {
        panic!("expected a rebase");
    };
    let id = revert(&f, rebased, false).unwrap();
    assert_eq!(land(&f, id), Landed::FastForward(id));
    assert_eq!(land(&f, theirs), Landed::AlreadyApplied);
    assert_eq!(land(&f, rebased), Landed::AlreadyApplied);
    assert_eq!(f.head("main"), Some(id));
    // And reverting the revert puts the change back.
    let again = revert(&f, id, false).unwrap();
    assert_eq!(land(&f, again), Landed::FastForward(again));
    assert_eq!(root(&f, again), root(&f, rebased));
}

#[test]
fn roots_empty_changes_and_merges_are_refused() {
    let f = Fixture::new();
    let base = f.commit(&[], vec![("a", f.file("a\n"))]);
    assert!(matches!(revert(&f, base, false), Err(Error::RevertRoot(id)) if id == base));
    let empty = f.commit(&[base], Vec::new());
    assert!(matches!(
        revert(&f, empty, false),
        Err(Error::NothingToRevert(id)) if id == empty
    ));
    let side = f.commit(&[base], vec![("s", f.file("s\n"))]);
    let merge = f.commit(&[base, side], vec![("s", f.file("s\n"))]);
    assert!(matches!(
        revert(&f, merge, false),
        Err(Error::RevertMerge { parents: 2, .. })
    ));
    // Asked for, it undoes what the merge brought in against its first
    // parent.
    let id = revert(&f, merge, true).unwrap();
    assert_eq!(root(&f, id), root(&f, base));
}
