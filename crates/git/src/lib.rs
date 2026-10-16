// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The git bridge of elffuss-braid: git history in, git history out, bit for
//! bit.
//!
//! - [`import`] reads a git repository and stores every blob, tree, commit and
//!   annotated tag reachable from its branches and tags as braid objects,
//!   recording each mapping in a [`GitMap`]. Every commit is exported again
//!   as it is imported, and the import stops unless its id comes back the
//!   same.
//! - The [`Exporter`] turns braid objects back into git objects: imported ones
//!   byte for byte, from the raw bytes they keep, and new ones
//!   deterministically. [`write_to_repo`] writes them into a git repository
//!   and points refs at them; [`write_to_repo_recording`] also records in the
//!   [`GitMap`] the objects it generated for changesets made in braid, so
//!   that importing them back gives the same changesets.
//! - [`verify_round_trip`] imports a repository, exports it into a fresh one,
//!   and checks that every id matches.
//!
//! Git access goes through [gitoxide](https://github.com/GitoxideLabs/gitoxide)
//! (MIT or Apache-2.0). Braid contains no code from git, which is GPLv2: the
//! few git formats parsed here are written from their documentation.
//!
//! Only SHA-1 repositories are supported for now, and every git id is
//! computed with SHA-1 collision detection.

#![forbid(unsafe_code)]

mod diff;
mod error;
mod export;
mod fetch;
mod import;
mod map;
mod oid;
mod raw;
mod repo;
mod roundtrip;

pub use error::{BoxError, Error};
pub use export::{
    Exporter, RefTarget, RefUpdate, WriteReport, export_blob, export_commit, export_object,
    export_tree, write_to_repo, write_to_repo_recording,
};
pub use import::{ImportOptions, ImportReport, ImportedRef, import, import_parallel};
pub use map::{GitMap, MapCounts, Pairings, TagEntry};
pub use oid::{GitKind, GitOid, ParseGitOidError};
pub use repo::is_branch_name;
pub use roundtrip::{RoundTripReport, verify_round_trip, verify_round_trip_with};

/// The parsers of raw git objects, for the fuzz targets in `fuzz/`. Not part
/// of the stable API.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing {
    pub use crate::raw::{
        CommitFields, EntryType, RawEntry, encode_commit, parse_commit, parse_signature, parse_tag,
        parse_tree,
    };
}
