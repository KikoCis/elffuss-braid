// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Checkouts of hostile trees: nothing may be written outside the
//! workspace, and names that collide are refused rather than both written.

mod common;

use std::fs;

use braid_wc::{Error, FileSystem, Updated, Workspace};
use common::{Fixture, ada, listing};

/// The bytes of an absolute path, as a symlink target.
fn target(path: &std::path::Path) -> Vec<u8> {
    path.as_os_str().as_encoded_bytes().to_vec()
}

#[test]
fn a_symlink_replaced_by_a_directory_is_never_gone_through() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let first = f.commit(
        &[],
        vec![
            ("dir", f.symlink(&target(outside.path()))),
            ("keep", f.file("k")),
        ],
    );
    f.set_branch("main", first);
    let ws_dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &ws_dir, Some(first), "main").unwrap();
    // The symlink is there, as a symlink.
    assert_eq!(fs::read_link(ws_dir.join("dir")).unwrap(), outside.path());

    // A later tree makes `dir` a directory with a file in it.
    let second = f.commit(
        &[first],
        vec![
            ("dir", braid_format::ChangeOp::Delete),
            ("dir/x", f.file("pwned")),
        ],
    );
    f.set_branch("main", second);
    assert_eq!(
        ws.update(&ada()).unwrap().updated,
        Updated::Moved { head: second }
    );
    assert!(
        listing(outside.path()).is_empty(),
        "wrote outside the workspace"
    );
    assert!(fs::symlink_metadata(ws_dir.join("dir")).unwrap().is_dir());
    assert_eq!(fs::read(ws_dir.join("dir/x")).unwrap(), b"pwned");
    assert!(ws.status().unwrap().status.is_clean());
}

#[test]
fn a_symlink_and_a_case_twin_directory_in_one_tree() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let hostile = f.commit(
        &[],
        vec![
            ("dir", f.symlink(&target(outside.path()))),
            ("DIR/x", f.file("pwned")),
        ],
    );
    let ws_dir = f.path("ws");
    fs::create_dir(&ws_dir).unwrap();
    let probe = FileSystem::probe(
        &cap_std::fs::Dir::open_ambient_dir(&ws_dir, cap_std::ambient_authority()).unwrap(),
    );
    fs::remove_dir(&ws_dir).unwrap();

    let created = Workspace::create(&f.repo, "ws", &ws_dir, Some(hostile), "main");
    if probe.ignores_case {
        // One file on this file system: refused, and nothing written.
        assert!(
            matches!(created, Err(Error::Collision { .. })),
            "{created:?}"
        );
        assert!(!ws_dir.exists());
        assert!(Workspace::list(&f.repo).unwrap().is_empty());
    } else {
        // Two names here: both written, the directory a real one.
        created.unwrap();
        assert!(fs::symlink_metadata(ws_dir.join("DIR")).unwrap().is_dir());
        assert_eq!(fs::read(ws_dir.join("DIR/x")).unwrap(), b"pwned");
    }
    assert!(
        listing(outside.path()).is_empty(),
        "wrote outside the workspace"
    );
}

#[test]
fn case_twins_are_refused_where_they_would_collide() {
    let f = Fixture::new();
    let twins = f.commit(
        &[],
        vec![("README", f.file("upper")), ("readme", f.file("lower"))],
    );
    let ws_dir = f.path("ws");
    fs::create_dir(&ws_dir).unwrap();
    let probe = FileSystem::probe(
        &cap_std::fs::Dir::open_ambient_dir(&ws_dir, cap_std::ambient_authority()).unwrap(),
    );
    let created = Workspace::create(&f.repo, "ws", &ws_dir, Some(twins), "main");
    if probe.ignores_case {
        let Err(Error::Collision { first, second }) = created else {
            panic!("expected a collision, got {created:?}");
        };
        assert_eq!(
            (first.as_slice(), second.as_slice()),
            (&b"README"[..], &b"readme"[..])
        );
        // The directory was there before and stays, empty.
        assert!(listing(&ws_dir).is_empty());
    } else {
        created.unwrap();
        assert_eq!(fs::read(ws_dir.join("README")).unwrap(), b"upper");
        assert_eq!(fs::read(ws_dir.join("readme")).unwrap(), b"lower");
    }
}

#[test]
fn an_untracked_symlink_in_the_way_is_refused() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    // `dir` is ignored, so a symlink there does not count as a change.
    let first = f.commit(&[], vec![(".gitignore", f.file("dir\n"))]);
    f.set_branch("main", first);
    let ws_dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &ws_dir, Some(first), "main").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), ws_dir.join("dir")).unwrap();
    assert!(ws.status().unwrap().status.is_clean());

    let second = f.commit(&[first], vec![("dir/x", f.file("pwned"))]);
    f.set_branch("main", second);
    let updated = ws.update(&ada());
    assert!(
        matches!(&updated, Err(Error::Unsafe { path, .. }) if path == b"dir"),
        "{updated:?}"
    );
    assert!(
        listing(outside.path()).is_empty(),
        "wrote outside the workspace"
    );
    // Nothing moved: the workspace is still at the first changeset.
    assert_eq!(ws.state().unwrap().base, Some(first));
}

#[test]
fn a_symlink_replaced_by_a_file_is_replaced_not_written_through() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret");
    fs::write(&secret, "keep me").unwrap();
    let first = f.commit(&[], vec![("link", f.symlink(&target(&secret)))]);
    f.set_branch("main", first);
    let ws_dir = f.path("ws");
    let (ws, _) = Workspace::create(&f.repo, "ws", &ws_dir, Some(first), "main").unwrap();
    let second = f.commit(&[first], vec![("link", f.file("replaced"))]);
    f.set_branch("main", second);
    ws.update(&ada()).unwrap();
    assert_eq!(fs::read(&secret).unwrap(), b"keep me");
    assert!(fs::symlink_metadata(ws_dir.join("link")).unwrap().is_file());
    assert_eq!(fs::read(ws_dir.join("link")).unwrap(), b"replaced");
}

#[cfg(unix)]
#[test]
fn modes_targets_and_gitlinks_are_written_as_data() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let gitlink = braid_format::ChangeOp::Gitlink {
        commit: braid_format::GitId::Sha1([7; 20]),
    };
    let first = f.commit(
        &[],
        vec![
            ("plain", f.file("p")),
            ("run.sh", f.exec("#!/bin/sh\n")),
            ("weird", f.symlink(b"../../../../etc/passwd")),
            ("vendor/lib", gitlink),
        ],
    );
    let ws_dir = f.path("ws");
    let (ws, written) = Workspace::create(&f.repo, "ws", &ws_dir, Some(first), "main").unwrap();
    assert_eq!(written, 4);
    let mode = |name: &str| {
        fs::metadata(ws_dir.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("plain"), 0o644);
    assert_eq!(mode("run.sh"), 0o755);
    assert_eq!(
        fs::read_link(ws_dir.join("weird"))
            .unwrap()
            .as_os_str()
            .as_encoded_bytes(),
        b"../../../../etc/passwd"
    );
    assert!(
        fs::symlink_metadata(ws_dir.join("vendor/lib"))
            .unwrap()
            .is_dir()
    );
    assert!(listing(&ws_dir.join("vendor/lib")).is_empty());
    assert!(ws.status().unwrap().status.is_clean());
}

#[cfg(target_vendor = "apple")]
#[test]
fn names_this_platform_cannot_hold_are_refused() {
    let f = Fixture::new();
    let bad = braid_engine::commit(
        f.repo.store(),
        Vec::new(),
        vec![braid_format::Change {
            path: b"bad-\xff".to_vec(),
            op: f.file("x"),
        }],
        common::signature(),
        common::signature(),
        b"m\n".to_vec(),
        Vec::new(),
    )
    .unwrap();
    let created = Workspace::create(&f.repo, "ws", &f.path("ws"), Some(bad), "main");
    assert!(matches!(created, Err(Error::Unsafe { .. })), "{created:?}");
}
