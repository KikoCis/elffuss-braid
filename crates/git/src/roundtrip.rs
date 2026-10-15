// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The round trip: git to braid and back, compared object by object.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use braid_format::MemoryObjects;

use crate::error::Error;
use crate::export::{RefTarget, RefUpdate, reached, write_to_repo};
use crate::import::{ImportOptions, import};
use crate::map::GitMap;
use crate::oid::{GitKind, GitOid};
use crate::repo::GitRepo;

/// What a round trip compared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoundTripReport {
    /// Commits, identical on both sides.
    pub commits: usize,
    /// Trees, identical on both sides.
    pub trees: usize,
    /// Of those trees, the ones the export kept as raw bytes.
    pub raw_trees: usize,
    /// Blobs, identical on both sides.
    pub blobs: usize,
    /// Annotated tags, identical on both sides.
    pub tags: usize,
    /// Refs pointing to the same objects on both sides.
    pub refs: usize,
}

/// Imports every branch and tag of `src_repo` into memory, exports
/// everything into a fresh bare repository under `tmp_dir`, and checks that
/// both repositories hold exactly the same commits, trees, blobs and tags,
/// with the same ids, and the same refs.
///
/// The refs are read once, at import, so commits added to the source while
/// the round trip runs do not disturb it. Every object read on either side is
/// checked to hash to its id.
pub fn verify_round_trip(src_repo: &Path, tmp_dir: &Path) -> Result<RoundTripReport, Error> {
    verify_round_trip_with(src_repo, tmp_dir, &ImportOptions::default())
}

/// [`verify_round_trip`] for the refs that `options` selects, such as `HEAD`
/// in a checkout without branches.
pub fn verify_round_trip_with(
    src_repo: &Path,
    tmp_dir: &Path,
    options: &ImportOptions,
) -> Result<RoundTripReport, Error> {
    let objects = MemoryObjects::new();
    let mut map = GitMap::new();
    let imported = import(src_repo, &objects, &mut map, options)?;

    let copy_path = tmp_dir.join("round-trip.git");
    GitRepo::init_bare(&copy_path)?;
    let updates: Vec<RefUpdate> = imported
        .refs
        .iter()
        .map(|r| RefUpdate {
            name: r.name.clone(),
            target: RefTarget::Git(r.oid),
        })
        .collect();
    write_to_repo(&copy_path, &objects, &map, &updates)?;

    let source = GitRepo::open(src_repo)?;
    let copy = GitRepo::open(&copy_path)?;
    let tips: Vec<GitOid> = imported.refs.iter().map(|r| r.oid).collect();
    let expected = reachable(&source, &tips)?;
    let found = reachable(&copy, &tips)
        .map_err(|err| Error::RoundTrip(format!("the copy is incomplete: {err}")))?;
    if expected != found {
        return Err(Error::RoundTrip(describe_difference(&expected, &found)));
    }
    for r in &imported.refs {
        let target = copy.resolve_ref(&r.name)?;
        if target != Some(r.oid) {
            return Err(Error::RoundTrip(format!(
                "ref {} points to {target:?} in the copy, {} in the source",
                String::from_utf8_lossy(&r.name),
                r.oid
            )));
        }
    }

    let mut report = RoundTripReport {
        refs: imported.refs.len(),
        raw_trees: map.counts().raw_trees,
        ..RoundTripReport::default()
    };
    for kind in expected.values() {
        match kind {
            GitKind::Commit => report.commits += 1,
            GitKind::Tree => report.trees += 1,
            GitKind::Blob => report.blobs += 1,
            GitKind::Tag => report.tags += 1,
        }
    }
    Ok(report)
}

/// Every object reachable from `tips`, with its kind. Submodule commits are
/// not followed: they live in another repository.
fn reachable(repo: &GitRepo, tips: &[GitOid]) -> Result<BTreeMap<GitOid, GitKind>, Error> {
    let mut found = BTreeMap::new();
    let mut queued: HashSet<GitOid> = tips.iter().copied().collect();
    let mut pending: Vec<GitOid> = tips.to_vec();
    while let Some(oid) = pending.pop() {
        let (kind, bytes) = repo.read(&oid)?;
        for next in reached(oid, kind, &bytes)? {
            if queued.insert(next) {
                pending.push(next);
            }
        }
        found.insert(oid, kind);
    }
    Ok(found)
}

fn describe_difference(
    expected: &BTreeMap<GitOid, GitKind>,
    found: &BTreeMap<GitOid, GitKind>,
) -> String {
    let missing: Vec<String> = expected
        .iter()
        .filter(|(oid, kind)| found.get(oid) != Some(kind))
        .take(5)
        .map(|(oid, kind)| format!("{kind} {oid}"))
        .collect();
    let extra: Vec<String> = found
        .iter()
        .filter(|(oid, kind)| expected.get(oid) != Some(kind))
        .take(5)
        .map(|(oid, kind)| format!("{kind} {oid}"))
        .collect();
    format!(
        "{} objects in the source, {} in the copy; missing from the copy: [{}]; only in the copy: [{}]",
        expected.len(),
        found.len(),
        missing.join(", "),
        extra.join(", ")
    )
}
