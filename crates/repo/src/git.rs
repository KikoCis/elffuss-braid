// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Importing from and exporting to git repositories.
//!
//! The map between git and braid ids is a derived object in the store, and the
//! ref [`MAP_REF`] points at its current version. An import moves that ref in
//! the same operation as the refs it brings in, so `braid undo` reverts both.
//! An export records in the map the commits, trees and blobs it generated for
//! changesets made in braid, so that importing them back gives the same
//! changesets.
//!
//! Braid's own refs live in namespaces no git branch maps to: `braid/` for
//! braid's bookkeeping, `remote/` for where the branches of git remotes were
//! last seen (see `git sync`), `tags/` for tags and `git/` for other git refs.

use std::convert::Infallible;
use std::path::Path;

use braid_format::{Kind, ObjectId, Objects};
use braid_git::{GitMap, ImportOptions, ImportReport, MapCounts, RefTarget, WriteReport};
use braid_store::{Operation, RefUpdate, TransactError};

use crate::{Error, Repo};

/// The ref that points at the current git map.
pub const MAP_REF: &str = "braid/git-map";

/// The prefix of the braid refs that record where the branches of git
/// remotes were last seen: `remote/<label>/<branch>`. They are never
/// exported.
pub const REMOTE_PREFIX: &str = "remote/";

/// The agent label of the operations [`Repo::git_export`] records.
pub const GIT_EXPORT_AGENT: &str = "git-export";

/// Braid's own namespaces. A git branch whose name starts with one of them
/// keeps its full name, under `git/`, so that it never lands in them.
pub(crate) const RESERVED: [&str; 4] = ["braid/", "git/", REMOTE_PREFIX, "tags/"];

/// How many times saving the git map merges with a map another process saved
/// meanwhile before giving up.
const MAP_ATTEMPTS: usize = 16;

/// The braid ref for a git ref: branches keep their short name, tags go under
/// `tags/`, and any other ref, or a branch whose short name starts like one
/// of braid's own namespaces (`braid/`, `git/`, `remote/`, `tags/`), keeps
/// its full name under `git/`.
pub fn braid_ref_for_git(name: &[u8]) -> Option<String> {
    let name = std::str::from_utf8(name).ok()?;
    let branch = name
        .strip_prefix("refs/heads/")
        .filter(|branch| !RESERVED.iter().any(|prefix| branch.starts_with(prefix)));
    Some(if let Some(branch) = branch {
        branch.to_owned()
    } else if let Some(tag) = name.strip_prefix("refs/tags/") {
        format!("tags/{tag}")
    } else {
        format!("git/{name}")
    })
}

/// The git ref for a braid ref, or `None` for braid's own refs and for the
/// refs that record the branches of remotes.
pub fn git_ref_for_braid(name: &str) -> Option<String> {
    if name.starts_with("braid/") || name.starts_with(REMOTE_PREFIX) {
        None
    } else if let Some(tag) = name.strip_prefix("tags/") {
        Some(format!("refs/tags/{tag}"))
    } else if let Some(full) = name.strip_prefix("git/") {
        Some(full.to_owned())
    } else {
        Some(format!("refs/heads/{name}"))
    }
}

/// The braid ref that records where `branch` of the remote labelled `label`
/// was last seen: `remote/<label>/<branch>`.
pub fn remote_ref(label: &str, branch: &str) -> String {
    format!("{REMOTE_PREFIX}{label}/{branch}")
}

/// What `git_import` did.
#[derive(Debug)]
pub struct GitImport {
    /// What the bridge imported.
    pub report: ImportReport,
    /// The operation that moved the refs, or `None` when nothing changed.
    pub operation: Option<Operation>,
}

fn store_error(err: TransactError<Infallible>) -> Error {
    match err {
        TransactError::Store(err) => Error::Store(err),
        TransactError::Aborted(never) => match never {},
    }
}

/// A git map being added to, and the stored map it started from.
#[derive(Debug)]
pub(crate) struct WorkingMap {
    /// The map.
    pub(crate) map: GitMap,
    /// What [`MAP_REF`] pointed at when the map was read.
    base: Option<ObjectId>,
    /// What that map held.
    counts: MapCounts,
}

impl WorkingMap {
    /// Whether the map holds more than the stored map it started from.
    pub(crate) fn changed(&self) -> bool {
        self.map.counts() != self.counts
    }
}

/// What an operation that saves the git map expects of a ref.
#[derive(Clone, Debug)]
pub(crate) struct Move {
    /// The ref.
    pub(crate) name: String,
    /// The value it must hold, `None` for a ref that must not exist; or, when
    /// this is `None` itself, whatever it holds.
    pub(crate) expect: Option<Option<ObjectId>>,
    /// Its new value.
    pub(crate) new: ObjectId,
}

/// Why an operation that saves the git map recorded nothing.
enum Abort {
    /// [`MAP_REF`] moved since the map was read.
    MapMoved(Option<ObjectId>),
    /// A ref did not hold the value the operation expected.
    Moved(String),
    /// Nothing would change.
    Nothing,
}

impl Repo {
    /// The current git map, empty if nothing was imported yet.
    pub fn git_map(&self) -> Result<GitMap, Error> {
        self.git_map_at(self.store.refs()?.get(MAP_REF))
    }

    /// The git map stored as `id`, or the empty map.
    fn git_map_at(&self, id: Option<ObjectId>) -> Result<GitMap, Error> {
        let Some(id) = id else {
            return Ok(GitMap::new());
        };
        let Some((_, bytes)) = self.store.get(&id)? else {
            return Err(Error::UnknownRevision(MAP_REF.to_owned()));
        };
        GitMap::from_bytes(&bytes).map_err(Error::GitMap)
    }

    /// The git map stored as `base`, the value of [`MAP_REF`], to add to.
    pub(crate) fn working_map(&self, base: Option<ObjectId>) -> Result<WorkingMap, Error> {
        let map = self.git_map_at(base)?;
        let counts = map.counts();
        Ok(WorkingMap { map, base, counts })
    }

    /// Records one operation by `agent` that makes the `moves` and, if the
    /// map grew, points [`MAP_REF`] at it. Returns `None` when nothing would
    /// change.
    ///
    /// If another operation saved a map meanwhile, the two are merged (see
    /// [`GitMap::absorb`]) and the operation is tried again, so that no
    /// pairing is lost. If a ref of `moves` does not hold what it should,
    /// nothing is recorded and the error is [`Error::BranchMoved`].
    pub(crate) fn save_with_map(
        &self,
        agent: &str,
        description: &str,
        map: &mut WorkingMap,
        moves: &[Move],
    ) -> Result<Option<Operation>, Error> {
        for _ in 0..MAP_ATTEMPTS {
            let map_id = if map.changed() {
                let id = self.put_map(&map.map, map.base)?;
                self.store.sync()?;
                Some(id)
            } else {
                None
            };
            let base = map.base;
            let result = self.store.transact(agent, description, |refs| {
                let mut updates = Vec::new();
                for wanted in moves {
                    let found = refs.get(&wanted.name);
                    if wanted.expect.is_some_and(|expected| expected != found) {
                        return Err(Abort::Moved(wanted.name.clone()));
                    }
                    if found != Some(wanted.new) {
                        updates.push(RefUpdate::new(wanted.name.clone(), found, Some(wanted.new)));
                    }
                }
                if let Some(id) = map_id {
                    let found = refs.get(MAP_REF);
                    if found != base {
                        return Err(Abort::MapMoved(found));
                    }
                    updates.push(RefUpdate::new(MAP_REF, found, Some(id)));
                }
                if updates.is_empty() {
                    return Err(Abort::Nothing);
                }
                Ok(updates)
            });
            match result {
                Ok(operation) => {
                    if let Some(id) = map_id {
                        map.base = Some(id);
                        map.counts = map.map.counts();
                    }
                    return Ok(Some(operation));
                }
                Err(TransactError::Aborted(Abort::Nothing)) => return Ok(None),
                Err(TransactError::Aborted(Abort::MapMoved(found))) => {
                    let mut current = self.git_map_at(found)?;
                    let counts = current.counts();
                    current.absorb(&map.map);
                    *map = WorkingMap {
                        map: current,
                        base: found,
                        counts,
                    };
                }
                Err(TransactError::Aborted(Abort::Moved(name))) => {
                    return Err(Error::BranchMoved { branch: name });
                }
                Err(TransactError::Store(err)) => return Err(Error::Store(err)),
            }
        }
        Err(Error::Busy {
            name: MAP_REF.to_owned(),
        })
    }

    /// Stores a git map, with the map it grew from, if any, as a hint: the
    /// store may keep it as a delta against it instead of whole again.
    fn put_map(&self, map: &GitMap, previous: Option<ObjectId>) -> Result<ObjectId, Error> {
        let bytes = map.to_bytes();
        Ok(match previous {
            Some(previous) => self.store.put_similar(Kind::Derived, &bytes, &previous)?,
            None => self.store.put(Kind::Derived, &bytes)?,
        })
    }

    /// Imports every branch and tag of the git repository at `git_repo`,
    /// incrementally, and points the matching braid refs at them.
    pub fn git_import(&self, git_repo: &Path, agent: &str) -> Result<GitImport, Error> {
        let previous = self.store.refs()?.get(MAP_REF);
        let mut map = self.git_map_at(previous)?;
        let report = braid_git::import(git_repo, &self.store, &mut map, &ImportOptions::default())?;
        let map_id = self.put_map(&map, previous)?;
        self.store.sync()?;

        let mut wanted: Vec<(String, ObjectId)> = report
            .refs
            .iter()
            .filter_map(|r| braid_ref_for_git(&r.name).map(|name| (name, r.braid)))
            .collect();
        wanted.push((MAP_REF.to_owned(), map_id));

        let current = self.store.refs()?;
        if wanted
            .iter()
            .all(|(name, id)| current.get(name) == Some(*id))
        {
            return Ok(GitImport {
                report,
                operation: None,
            });
        }
        let description = format!("git import of {} refs", wanted.len() - 1);
        let operation = self
            .store
            .transact(agent, &description, |refs| {
                Ok(wanted
                    .iter()
                    .filter(|(name, id)| refs.get(name) != Some(*id))
                    .map(|(name, id)| RefUpdate::new(name.clone(), refs.get(name), Some(*id)))
                    .collect())
            })
            .map_err(store_error)?;
        Ok(GitImport {
            report,
            operation: Some(operation),
        })
    }

    /// Writes the given braid refs (all of them when `names` is empty) into
    /// the git repository at `git_repo`, which must exist. Existing git refs
    /// of the same names are overwritten.
    ///
    /// The commits, trees and blobs the export generated for changesets made
    /// in braid are recorded in the git map, as an operation by
    /// [`GIT_EXPORT_AGENT`]; [`git_export_as`](Self::git_export_as) names
    /// the agent.
    pub fn git_export(&self, git_repo: &Path, names: &[String]) -> Result<WriteReport, Error> {
        self.git_export_as(git_repo, names, GIT_EXPORT_AGENT)
    }

    /// [`git_export`](Self::git_export), recording the git map as an
    /// operation by `agent`.
    pub fn git_export_as(
        &self,
        git_repo: &Path,
        names: &[String],
        agent: &str,
    ) -> Result<WriteReport, Error> {
        let refs = self.store.refs()?;
        let mut map = self.working_map(refs.get(MAP_REF))?;
        let mut updates = Vec::new();
        for (name, id) in refs.iter() {
            if !names.is_empty() && !names.iter().any(|n| n == name) {
                continue;
            }
            let Some(git_name) = git_ref_for_braid(name) else {
                continue;
            };
            let target = match self.store.stat(&id)? {
                Some((Kind::Changeset, _)) => RefTarget::Changeset(id),
                Some((Kind::Blob, _)) => match map.map.git_tag(&id) {
                    Some(tag) => RefTarget::Git(tag),
                    None => {
                        return Err(Error::NotExportable {
                            name: name.to_owned(),
                        });
                    }
                },
                _ => {
                    return Err(Error::NotExportable {
                        name: name.to_owned(),
                    });
                }
            };
            updates.push(braid_git::RefUpdate {
                name: git_name.into_bytes(),
                target,
            });
        }
        for name in names {
            if refs.get(name).is_none() {
                return Err(Error::UnknownRevision(name.clone()));
            }
        }
        let report =
            braid_git::write_to_repo_recording(git_repo, &self.store, &mut map.map, &updates)?;
        if map.changed() {
            let description = format!("git export of {} refs", updates.len());
            self.save_with_map(agent, &description, &mut map, &[])?;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_names_round_trip() {
        for (git, braid) in [
            ("refs/heads/main", "main"),
            ("refs/heads/feature/x", "feature/x"),
            ("refs/tags/v1.0", "tags/v1.0"),
            ("refs/notes/commits", "git/refs/notes/commits"),
            // Branches named like braid's own namespaces keep their full
            // names, so that none of them lands there.
            ("refs/heads/tags/v1", "git/refs/heads/tags/v1"),
            ("refs/heads/git/x", "git/refs/heads/git/x"),
            ("refs/heads/remote/a/main", "git/refs/heads/remote/a/main"),
            ("refs/heads/braid/git-map", "git/refs/heads/braid/git-map"),
            ("refs/heads/remotes", "remotes"),
            ("refs/heads/tags", "tags"),
        ] {
            assert_eq!(braid_ref_for_git(git.as_bytes()).as_deref(), Some(braid));
            assert_eq!(git_ref_for_braid(braid).as_deref(), Some(git));
        }
        assert_eq!(git_ref_for_braid(MAP_REF), None);
        assert_eq!(braid_ref_for_git(b"refs/heads/\xff"), None);
        let remote = remote_ref("origin-0123456789ab", "main");
        assert_eq!(remote, "remote/origin-0123456789ab/main");
        assert_eq!(git_ref_for_braid(&remote), None);
    }
}
