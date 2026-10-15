// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Engine bugs the landing simulation found, each cut down to a minimal test.

use braid_engine::{LandOutcome, commit, land};
use braid_format::{Change, ChangeOp, Kind, MemoryObjects, ObjectId, Objects, Signature};

fn signature(name: &str) -> Signature {
    Signature {
        name: name.as_bytes().to_vec(),
        email: format!("{name}@example.org").into_bytes(),
        seconds: 1_700_000_000,
        offset_minutes: 0,
    }
}

fn set(objects: &MemoryObjects, path: &str, content: &str) -> Change {
    let blob = objects.put(Kind::Blob, content.as_bytes()).unwrap();
    Change {
        path: path.as_bytes().to_vec(),
        op: ChangeOp::File {
            blob,
            size: content.len() as u64,
        },
    }
}

fn change(objects: &MemoryObjects, parents: &[ObjectId], changes: Vec<Change>) -> ObjectId {
    commit(
        objects,
        parents.to_vec(),
        changes,
        signature("agent"),
        signature("agent"),
        b"change\n".to_vec(),
        Vec::new(),
    )
    .unwrap()
}

fn land_on(objects: &MemoryObjects, head: ObjectId, change: ObjectId) -> LandOutcome {
    land(objects, Some(head), change, signature("lander")).unwrap()
}

/// Found by the in-memory run, seed 1 (invariant I4; 30 of the first 40
/// seeds hit it): a change landed by a rebase, the answer was lost, and the
/// agent sent the same changeset again. `land` recognised a change that had
/// landed by a fast-forward, which is on the branch itself, but not one that
/// had landed rebased, which is on the branch as a new changeset whose
/// `rebased-from` names it. It rebased the change a second time: after an
/// edit of the same file the retry reported a conflict for a change that had
/// landed, and after a revert it landed the same edit twice.
///
/// This failed until `land` learned to look for `rebased-from` on its walk
/// from the head down to the base.
#[test]
fn a_change_that_landed_rebased_is_already_applied() {
    let objects = MemoryObjects::new();
    let base = change(
        &objects,
        &[],
        vec![set(&objects, "f", "1\n"), set(&objects, "g", "1\n")],
    );
    let ours = change(&objects, &[base], vec![set(&objects, "g", "2\n")]);
    let theirs = change(&objects, &[base], vec![set(&objects, "f", "2\n")]);
    assert_eq!(
        land_on(&objects, base, ours),
        LandOutcome::FastForward(ours)
    );
    let LandOutcome::Rebased {
        changeset: rebased, ..
    } = land_on(&objects, ours, theirs)
    else {
        panic!("expected a rebase");
    };

    // Sent again right away: nothing to do, as before.
    assert_eq!(
        land_on(&objects, rebased, theirs),
        LandOutcome::AlreadyApplied
    );

    // Sent again after another change edited the same file: it landed, so it
    // must not be reported as a conflict.
    let edit = change(&objects, &[rebased], vec![set(&objects, "f", "3\n")]);
    assert_eq!(
        land_on(&objects, rebased, edit),
        LandOutcome::FastForward(edit)
    );
    assert_eq!(land_on(&objects, edit, theirs), LandOutcome::AlreadyApplied);

    // Sent again after another change put the file back: it must not land a
    // second time over the revert.
    let revert = change(&objects, &[rebased], vec![set(&objects, "f", "1\n")]);
    assert_eq!(
        land_on(&objects, rebased, revert),
        LandOutcome::FastForward(revert)
    );
    assert_eq!(
        land_on(&objects, revert, theirs),
        LandOutcome::AlreadyApplied
    );
}
