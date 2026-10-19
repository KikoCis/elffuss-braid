// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Landing a stack of changesets made somewhere else: what a hub does when
//! a client asks it to land a workspace's changesets.

use braid_engine::read_changeset;
use braid_format::ObjectId;
use braid_repo::Repo;
use braid_store::{RefUpdate, TransactError};

use crate::error::Error;
use crate::identity::Identity;
use crate::rebase::{First, Replayed, replay};
use crate::workspace::{LandReport, Landed, Placed, describe_landing, unplaced};

/// How [`land_stack`] puts a stack on a branch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StackMode {
    /// Fast-forward when the first changeset's base is the head, and rebase
    /// the stack onto the head otherwise, as a workspace's landing does.
    #[default]
    Rebase,
    /// Fast-forward only: the branch moves when the head is the first
    /// changeset's first parent, or one of the changesets; otherwise the
    /// landing is refused and the branch stays where it is.
    FastForwardOnly,
}

/// Lands `stack`, oldest first, on `branch` of `repo` in one op log
/// transaction, as [`Workspace::land`](crate::Workspace::land) lands a
/// workspace's changesets. Each changeset's first parent must be the one
/// before it.
///
/// On a branch that does not exist yet, the branch is created at the last
/// changeset. Otherwise [`StackMode::Rebase`] fast-forwards or rebases the
/// stack onto the head, failing with [`Error::NotOnBranch`] when the first
/// changeset's base is not on the head's first-parent chain, and
/// [`StackMode::FastForwardOnly`] fast-forwards or answers
/// [`Landed::Refused`]. A stack the branch has already is
/// [`Landed::AlreadyApplied`] in both modes, so that landing it again after
/// a lost answer is harmless. With conflicts, a refusal, or nothing new,
/// the branch does not move and no operation is recorded.
///
/// The changesets and everything they name must be stored, and should have
/// been admitted (see [`braid_engine::admit`]): this only lands them.
pub fn land_stack(
    repo: &Repo,
    branch: &str,
    stack: &[ObjectId],
    mode: StackMode,
    agent: &str,
    committer: &Identity,
) -> Result<LandReport, Error> {
    let store = repo.store();
    let Some(&last) = stack.last() else {
        return Err(Error::EmptyStack);
    };
    for (index, pair) in stack.windows(2).enumerate() {
        if read_changeset(store, pair[1])?.parents.first() != Some(&pair[0]) {
            return Err(Error::NotAStack { index: index + 1 });
        }
    }
    let first_parent = read_changeset(store, stack[0])?.parents.first().copied();
    let signature = committer.now();
    let mut history = repo.history();
    let mut head_before = None;
    let mut landed = Landed::Nothing;
    let mut placed = Vec::new();
    let mut merged = Vec::new();
    let prefix = format!("land stack on {branch}");
    let itself = |ids: &[ObjectId]| -> Vec<Placed> {
        ids.iter()
            .map(|&id| Placed {
                from: id,
                to: Some(id),
            })
            .collect()
    };
    let result = store.transact_with(agent, |refs| -> Result<_, Option<Error>> {
        let head = refs.get(branch);
        head_before = head;
        let (tip, outcome) = match head {
            None => {
                placed = itself(stack);
                landed = Landed::FastForward(last);
                (last, "fast-forward")
            }
            Some(head) if mode == StackMode::FastForwardOnly => {
                let from = if first_parent == Some(head) {
                    Some(0)
                } else {
                    stack.iter().position(|&id| id == head).map(|at| at + 1)
                };
                match from {
                    Some(from) if from < stack.len() => {
                        placed = unplaced(&stack[..from]);
                        placed.extend(itself(&stack[from..]));
                        landed = Landed::FastForward(last);
                        (last, "fast-forward")
                    }
                    Some(_) => {
                        placed = unplaced(stack);
                        landed = Landed::AlreadyApplied;
                        return Err(None);
                    }
                    None => {
                        let has_it = history
                            .first_parent_chain_contains(head, last, usize::MAX)
                            .map_err(|err| Some(err.into()))?;
                        placed = unplaced(stack);
                        landed = if has_it {
                            Landed::AlreadyApplied
                        } else {
                            Landed::Refused
                        };
                        return Err(None);
                    }
                }
            }
            Some(head) => {
                match replay(
                    store,
                    branch,
                    Some(head),
                    stack,
                    &signature,
                    First::MustBeOnBranch,
                )
                .map_err(Some)?
                {
                    Replayed::Done {
                        tip, placed: went, ..
                    } if tip == head => {
                        placed = went;
                        landed = Landed::AlreadyApplied;
                        return Err(None);
                    }
                    Replayed::Done {
                        tip,
                        rebased,
                        placed: went,
                        merged_paths,
                    } => {
                        placed = went;
                        merged = merged_paths;
                        if rebased {
                            landed = Landed::Rebased(tip);
                            (tip, "rebased")
                        } else {
                            landed = Landed::FastForward(tip);
                            (tip, "fast-forward")
                        }
                    }
                    Replayed::Stopped {
                        index, conflicts, ..
                    } => {
                        placed = unplaced(stack);
                        landed = Landed::Conflicts {
                            change: stack[index],
                            conflicts,
                        };
                        return Err(None);
                    }
                }
            }
        };
        let description = describe_landing(&prefix, outcome, &placed, merged.len());
        Ok((vec![RefUpdate::new(branch, head, Some(tip))], description))
    });
    let operation = match result {
        Ok(op) => Some(op.seq),
        // The branch stays where it is.
        Err(TransactError::Aborted(None)) => None,
        Err(TransactError::Aborted(Some(err))) => return Err(err),
        Err(TransactError::Store(err)) => return Err(err.into()),
    };
    history.flush()?;
    Ok(LandReport {
        branch: branch.to_owned(),
        head_before,
        landed,
        operation,
        placed,
        merged_paths: merged,
        updated_paths: Vec::new(),
    })
}
