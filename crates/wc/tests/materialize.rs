// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Writing a changeset into a plain directory: the files only, with the
//! checkout's modes and its refusal to follow symlinks.

#![cfg(feature = "materialize")]

mod common;

use braid_format::{ChangeOp, GitId};
use braid_wc::{Error, materialize};
use common::{Fixture, listing};

#[test]
fn a_changeset_is_written_with_its_files_only() {
    let f = Fixture::new();
    let id = f.commit(
        &[],
        vec![
            ("bin/run", f.exec("#!/bin/sh\n")),
            ("docs/a.md", f.file("a\n")),
            ("link", f.symlink(b"docs/a.md")),
            (
                "sub",
                ChangeOp::Gitlink {
                    commit: GitId::Sha1([7; 20]),
                },
            ),
        ],
    );
    let dir = f.path("tree");
    assert_eq!(materialize(f.repo.store(), id, &dir).unwrap(), 4);
    assert_eq!(
        listing(&dir),
        ["bin", "bin/run", "docs", "docs/a.md", "link", "sub"]
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("docs/a.md")).unwrap(),
        "a\n"
    );
    let link = std::fs::symlink_metadata(dir.join("link")).unwrap();
    assert!(link.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(dir.join("link")).unwrap(),
        std::path::Path::new("docs/a.md")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &str| {
            std::fs::metadata(dir.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("bin/run") & 0o111, 0o111);
        assert_eq!(mode("docs/a.md") & 0o111, 0);
    }
}

#[test]
fn only_an_empty_or_missing_directory_is_written() {
    let f = Fixture::new();
    let id = f.commit(&[], vec![("a", f.file("a\n"))]);
    let empty = f.path("empty");
    std::fs::create_dir(&empty).unwrap();
    assert_eq!(materialize(f.repo.store(), id, &empty).unwrap(), 1);
    // Not into one that holds anything, such as what was just written.
    assert!(matches!(
        materialize(f.repo.store(), id, &empty),
        Err(Error::DirectoryNotEmpty(_))
    ));
    // The directory's parent must exist.
    assert!(materialize(f.repo.store(), id, &f.path("no/such")).is_err());
}
