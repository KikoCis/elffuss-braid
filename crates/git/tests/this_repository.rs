// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The round trip on a real history: the repository these tests live in.

use std::path::Path;

use braid_git::{Error, ImportOptions, RoundTripReport, verify_round_trip, verify_round_trip_with};

/// Runs a round trip of this repository, or says why it cannot.
fn round_trip(options: &ImportOptions) -> Option<RoundTripReport> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !root.join(".git").exists() {
        eprintln!("skipped: the sources are not in a git checkout");
        return None;
    }
    let tmp = tempfile::tempdir().unwrap();
    match verify_round_trip_with(&root, tmp.path(), options) {
        Ok(report) => Some(report),
        // A shallow checkout cannot be imported, by design.
        Err(Error::Shallow) => {
            eprintln!("skipped: this checkout is shallow");
            None
        }
        Err(err) => panic!("{err}"),
    }
}

#[test]
fn the_history_of_head_round_trips() {
    // A checkout always has HEAD, even when it has no local branch.
    let options = ImportOptions {
        refs: vec![b"HEAD".to_vec()],
    };
    if let Some(report) = round_trip(&options) {
        assert!(report.commits >= 3, "{report:?}");
        assert!(report.trees > 0 && report.blobs > 0, "{report:?}");
        assert_eq!(report.refs, 1);
    }
}

#[test]
fn every_branch_and_tag_round_trips() {
    if let Some(report) = round_trip(&ImportOptions::default()) {
        eprintln!("{report:?}");
    }
}

/// The round trip of any repository, on request:
/// `BRAID_ROUND_TRIP_REPO=<path> cargo test -p elffuss-braid-git --release --test this_repository -- --ignored`
#[test]
#[ignore = "needs BRAID_ROUND_TRIP_REPO"]
fn another_repository_round_trips() {
    let Some(path) = std::env::var_os("BRAID_ROUND_TRIP_REPO") else {
        eprintln!("set BRAID_ROUND_TRIP_REPO to the repository to check");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let report =
        verify_round_trip(Path::new(&path), tmp.path()).unwrap_or_else(|err| panic!("{err}"));
    eprintln!("{report:?}");
}
