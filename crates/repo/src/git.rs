// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Importing from and exporting to git repositories.
//!
//! The map between git and braid ids is a derived object in the store, and the
//! ref [`MAP_REF`] points at its current version. An import moves that ref in
//! the same operation as the refs it brings in, so `braid undo` reverts both.

use std::convert::Infallible;
use std::path::Path;

use braid_format::{Kind, ObjectId, Objects};
use braid_git::{GitMap, ImportOptions, ImportReport, RefTarget, WriteReport};
use braid_store::{Operation, RefUpdate, TransactError};

use crate::{Error, Repo};

/// The ref that points at the current git map.
pub const MAP_REF: &str = "braid/git-map";

/// The braid ref for a git ref: branches keep their short name, tags go under
/// `tags/`, and any other ref keeps its full name under `git/`.
pub fn braid_ref_for_git(name: &[u8]) -> Option<String> {
    let name = std::str::from_utf8(name).ok()?;
    Some(if let Some(branch) = name.strip_prefix("refs/heads/") {
        branch.to_owned()
    } else if let Some(tag) = name.strip_prefix("refs/tags/") {
        format!("tags/{tag}")
    } else {
        format!("git/{name}")
    })
}

/// The git ref for a braid ref, or `None` for braid's own refs.
pub fn git_ref_for_braid(name: &str) -> Option<String> {
    if name.starts_with("braid/") {
        None
    } else if let Some(tag) = name.strip_prefix("tags/") {
        Some(format!("refs/tags/{tag}"))
    } else if let Some(full) = name.strip_prefix("git/") {
        Some(full.to_owned())
    } else {
        Some(format!("refs/heads/{name}"))
    }
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

impl Repo {
    /// The current git map, empty if nothing was imported yet.
    pub fn git_map(&self) -> Result<GitMap, Error> {
        let Some(id) = self.store.refs()?.get(MAP_REF) else {
            return Ok(GitMap::new());
        };
        let Some((_, bytes)) = self.store.get(&id)? else {
            return Err(Error::UnknownRevision(MAP_REF.to_owned()));
        };
        GitMap::from_bytes(&bytes).map_err(Error::GitMap)
    }

    /// Imports every branch and tag of the git repository at `git_repo`,
    /// incrementally, and points the matching braid refs at them.
    pub fn git_import(&self, git_repo: &Path, agent: &str) -> Result<GitImport, Error> {
        let mut map = self.git_map()?;
        let report = braid_git::import(git_repo, &self.store, &mut map, &ImportOptions::default())?;
        let map_id = self.store.put(Kind::Derived, &map.to_bytes())?;
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
    pub fn git_export(&self, git_repo: &Path, names: &[String]) -> Result<WriteReport, Error> {
        let map = self.git_map()?;
        let refs = self.store.refs()?;
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
                Some((Kind::Blob, _)) => match map.git_tag(&id) {
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
        Ok(braid_git::write_to_repo(
            git_repo,
            &self.store,
            &map,
            &updates,
        )?)
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
        ] {
            assert_eq!(braid_ref_for_git(git.as_bytes()).as_deref(), Some(braid));
            assert_eq!(git_ref_for_braid(braid).as_deref(), Some(git));
        }
        assert_eq!(git_ref_for_braid(MAP_REF), None);
        assert_eq!(braid_ref_for_git(b"refs/heads/\xff"), None);
    }
}
