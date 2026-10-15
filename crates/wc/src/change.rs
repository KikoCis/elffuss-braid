// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Making a changeset without a working copy, and landing one.
//!
//! An agent that knows which files it wants to change does not need a
//! checkout: it names the base, the new contents and the deletions, and can
//! state what it expects the base to hold at each path, so that it never
//! overwrites an edit it did not see.

use braid_engine::{EMPTY_TREE, LandOutcome, entry_at, land, read_changeset};
use braid_format::{Change, ChangeOp, EntryKind, Kind, ObjectId, Objects, validate_path};
use braid_store::{RefUpdate, Store, TransactError};

use crate::error::Error;
use crate::identity::Identity;
use crate::workspace::{LandReport, Landed, Placed, describe_landing};

/// What the base must hold at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// A file, executable or symlink with this blob.
    Blob(ObjectId),
    /// Nothing: no file and no directory.
    Absent,
}

/// A changeset to make.
#[derive(Clone, Debug)]
pub struct ChangeRequest {
    /// The changeset to build on; `None` for a first changeset.
    pub base: Option<ObjectId>,
    /// Paths to set, with their new contents. A path that is an executable
    /// in the base stays executable.
    pub sets: Vec<(Vec<u8>, Vec<u8>)>,
    /// Paths to delete.
    pub deletes: Vec<Vec<u8>>,
    /// What the base must hold, checked before anything is stored.
    pub expects: Vec<(Vec<u8>, Expect)>,
    /// The message.
    pub message: Vec<u8>,
    /// The author, who also commits it.
    pub author: Identity,
    /// The label of the agent making it, recorded as its
    /// [`AGENT`](braid_engine::AGENT) field.
    pub agent: String,
}

/// Checks the expectations against the base, stores the contents and the
/// changeset, makes it durable, and returns its id.
pub fn create_change(store: &Store, request: &ChangeRequest) -> Result<ObjectId, Error> {
    let base_root = match request.base {
        Some(base) => read_changeset(store, base)?.root,
        None => EMPTY_TREE,
    };
    for path in request
        .sets
        .iter()
        .map(|(path, _)| path)
        .chain(&request.deletes)
        .chain(request.expects.iter().map(|(path, _)| path))
    {
        validate_path(path).map_err(|error| Error::InvalidPath {
            path: path.clone(),
            error,
        })?;
    }
    for (path, expect) in &request.expects {
        let found = entry_at(store, base_root, path)?;
        let matches = match (expect, &found) {
            (Expect::Absent, None) => true,
            (
                Expect::Blob(want),
                Some(
                    EntryKind::File { blob, .. }
                    | EntryKind::Executable { blob, .. }
                    | EntryKind::Symlink { blob, .. },
                ),
            ) => want == blob,
            _ => false,
        };
        if !matches {
            return Err(Error::ExpectationFailed {
                path: path.clone(),
                expected: match expect {
                    Expect::Blob(id) => id.to_hex(),
                    Expect::Absent => "absent".to_owned(),
                },
                found: describe(found.as_ref()),
            });
        }
    }
    let mut changes = Vec::with_capacity(request.sets.len() + request.deletes.len());
    for (path, content) in &request.sets {
        let blob = store.put(Kind::Blob, content)?;
        let size = content.len() as u64;
        let executable = matches!(
            entry_at(store, base_root, path)?,
            Some(EntryKind::Executable { .. })
        );
        changes.push(Change {
            path: path.clone(),
            op: if executable {
                ChangeOp::Executable { blob, size }
            } else {
                ChangeOp::File { blob, size }
            },
        });
    }
    for path in &request.deletes {
        changes.push(Change {
            path: path.clone(),
            op: ChangeOp::Delete,
        });
    }
    let signature = request.author.now();
    let id = braid_engine::commit(
        store,
        request.base.into_iter().collect(),
        changes,
        signature.clone(),
        signature,
        request.message.clone(),
        crate::rebase::agent_extras(&request.agent),
    )?;
    store.sync()?;
    Ok(id)
}

fn describe(entry: Option<&EntryKind>) -> String {
    match entry {
        None => "absent".to_owned(),
        Some(EntryKind::Tree { .. }) => "a directory".to_owned(),
        Some(EntryKind::Gitlink { .. }) => "a gitlink".to_owned(),
        Some(
            EntryKind::File { blob, .. }
            | EntryKind::Executable { blob, .. }
            | EntryKind::Symlink { blob, .. },
        ) => blob.to_hex(),
    }
}

/// Lands `change` on `branch` in one op log transaction: the engine
/// fast-forwards or rebases it onto the head, and the branch moves with a
/// compare-and-swap. The operation says how it went, as a workspace's
/// landing does. With conflicts, or when the branch already has it, the
/// branch does not move and no operation is recorded.
pub fn land_change(
    store: &Store,
    branch: &str,
    change: ObjectId,
    agent: &str,
    committer: &Identity,
) -> Result<LandReport, Error> {
    let signature = committer.now();
    let mut head_before = None;
    let mut landed = Landed::Nothing;
    let mut merged = Vec::new();
    let prefix = format!("land changeset on {branch}");
    let result = store.transact_with(agent, |refs| {
        let head = refs.get(branch);
        head_before = head;
        let (new, outcome) =
            match land(store, head, change, signature).map_err(|err| Some(err.into()))? {
                LandOutcome::FastForward(id) => {
                    landed = Landed::FastForward(id);
                    (id, "fast-forward")
                }
                LandOutcome::Rebased {
                    changeset,
                    merged_paths,
                } => {
                    landed = Landed::Rebased(changeset);
                    merged = merged_paths;
                    (changeset, "rebased")
                }
                LandOutcome::AlreadyApplied => {
                    landed = Landed::AlreadyApplied;
                    return Err(None);
                }
                LandOutcome::Conflicts(conflicts) => {
                    landed = Landed::Conflicts { change, conflicts };
                    return Err(None);
                }
            };
        let placed = [Placed {
            from: change,
            to: Some(new),
        }];
        let description = describe_landing(&prefix, outcome, &placed, merged.len());
        Ok((vec![RefUpdate::new(branch, head, Some(new))], description))
    });
    let operation = match result {
        Ok(op) => Some(op.seq),
        // The branch stays where it is.
        Err(TransactError::Aborted(None)) => None,
        Err(TransactError::Aborted(Some(err))) => return Err(err),
        Err(TransactError::Store(err)) => return Err(err.into()),
    };
    let to = match &landed {
        Landed::FastForward(id) | Landed::Rebased(id) => Some(*id),
        _ => None,
    };
    Ok(LandReport {
        branch: branch.to_owned(),
        head_before,
        landed,
        operation,
        placed: vec![Placed { from: change, to }],
        merged_paths: merged,
        updated_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describing_entries() {
        assert_eq!(describe(None), "absent");
        let blob = ObjectId::hash(Kind::Blob, b"x");
        assert_eq!(
            describe(Some(&EntryKind::File { blob, size: 1 })),
            blob.to_hex()
        );
    }
}
