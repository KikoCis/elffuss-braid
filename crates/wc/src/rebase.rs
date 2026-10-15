// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Putting a workspace's own changesets on top of a branch head.
//!
//! A workspace's unlanded changesets form a stack: each one's first parent
//! is the one before, and the first one's is on the branch. The first is
//! landed with [`braid_engine::land`], which needs its base on the head's
//! first-parent chain. The ones after it are based on a changeset that the
//! landing may have just rewritten, so each is rebased with the changeset it
//! was built on as the merge base, onto the rewritten one: the engine is
//! asked to land it on a temporary changeset whose first parent is its
//! original base and whose tree is the rewritten one's, and the result is
//! given the rewritten one as its parent. The temporary changesets stay in
//! the store, unreferenced.

use braid_engine::{
    AGENT, Conflict, LandOutcome, REBASED_FROM, commit, diff, entry_at, land, read_blob,
    read_changes, read_changeset,
};
use braid_format::{
    Change, ChangeOp, ChangesetParts, EntryKind, Kind, ObjectId, Objects, Signature,
};

use crate::error::Error;
use crate::markers;
use crate::paths::is_within;
use crate::state::RecordedConflict;
use crate::tree::root_of;
use crate::workspace::Placed;

/// The extra field a temporary base carries, so that it can be told apart.
pub const TEMPORARY: &[u8] = b"braid-temporary-base";

/// How a replay ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Replayed {
    /// Every changeset is on top of the head; `tip` is the last of them, or
    /// the head itself when it had all of them already.
    Done {
        tip: ObjectId,
        rebased: bool,
        /// Where each changeset of the stack went, in order.
        placed: Vec<Placed>,
        /// The paths both sides changed, over the whole stack, sorted.
        merged_paths: Vec<Vec<u8>>,
    },
    /// The changeset at `index` of the stack conflicts with `onto`, the head
    /// or the last changeset replayed before it.
    Stopped {
        index: usize,
        onto: ObjectId,
        conflicts: Vec<Conflict>,
        /// Where the changesets before `index` went.
        placed: Vec<Placed>,
    },
}

/// How the first changeset of a stack goes onto the head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum First {
    /// Landed: its base must be on the head's first-parent chain.
    MustBeOnBranch,
    /// Landed if its base is on the chain, and otherwise rebased from its
    /// own base wherever that is, as an update allows.
    Anywhere,
    /// Rebased from its own base, which is known not to be on the chain:
    /// what continuing a rebase after a resolution does.
    Pick,
}

/// Replays `stack`, oldest first, onto `head` (`None` for an empty
/// branch). Fails with [`Error::NotOnBranch`] when the first changeset's
/// base is not on the branch and `first` requires it.
pub(crate) fn replay<O: Objects>(
    objects: &O,
    branch: &str,
    head: Option<ObjectId>,
    stack: &[ObjectId],
    committer: &Signature,
    first: First,
) -> Result<Replayed, Error> {
    let mut tip = head;
    let mut rebased = false;
    let mut placed = Vec::with_capacity(stack.len());
    let mut merged = Vec::new();
    for (index, &change) in stack.iter().enumerate() {
        let outcome = match tip {
            Some(onto) if index > 0 || first == First::Pick => {
                replay_one(objects, onto, change, committer)?
            }
            _ => match land(objects, tip, change, committer.clone()) {
                Err(braid_engine::Error::BaseNotOnBranch { .. }) => match (first, tip) {
                    (First::Anywhere, Some(onto)) => replay_one(objects, onto, change, committer)?,
                    // A first changeset without a parent is based on the
                    // empty tree, where every branch starts: rebase it from
                    // there rather than asking for an update. A path the
                    // branch added too is a conflict, never overwritten.
                    (First::MustBeOnBranch, Some(onto))
                        if read_changeset(objects, change)?.parents.is_empty() =>
                    {
                        replay_one(objects, onto, change, committer)?
                    }
                    _ => {
                        return Err(Error::NotOnBranch {
                            branch: branch.to_owned(),
                        });
                    }
                },
                other => other?,
            },
        };
        let to = match outcome {
            LandOutcome::FastForward(id) => Some(id),
            LandOutcome::Rebased {
                changeset,
                merged_paths,
            } => {
                rebased = true;
                merged.extend(merged_paths);
                Some(changeset)
            }
            LandOutcome::AlreadyApplied => None,
            LandOutcome::Conflicts(conflicts) => {
                let onto = tip.ok_or(Error::NotOnBranch {
                    branch: branch.to_owned(),
                })?;
                return Ok(Replayed::Stopped {
                    index,
                    onto,
                    conflicts,
                    placed,
                });
            }
        };
        tip = to.or(tip);
        placed.push(Placed { from: change, to });
    }
    merged.sort();
    merged.dedup();
    match tip {
        Some(tip) => Ok(Replayed::Done {
            tip,
            rebased,
            placed,
            merged_paths: merged,
        }),
        None => Err(Error::NotOnBranch {
            branch: branch.to_owned(),
        }),
    }
}

/// Rebases `change` onto `onto`, with `change`'s first parent as the merge
/// base, whatever history connects them. A first changeset, which has no
/// parent, is rebased from the empty tree.
pub(crate) fn replay_one<O: Objects>(
    objects: &O,
    onto: ObjectId,
    change: ObjectId,
    committer: &Signature,
) -> Result<LandOutcome, Error> {
    let theirs = read_changeset(objects, change)?;
    let (stand_in, base) = match theirs.parents.first() {
        Some(&base) if base == onto => return Ok(LandOutcome::FastForward(change)),
        Some(&base) => (change, base),
        None => {
            // The same change on an empty first changeset: its tree and
            // its change list stay what they are.
            let empty = commit(
                objects,
                Vec::new(),
                Vec::new(),
                committer.clone(),
                committer.clone(),
                b"braid: empty base of a rebase\n".to_vec(),
                vec![(TEMPORARY.to_vec(), Vec::new())],
            )?;
            let changes = read_changes(objects, change, &theirs)?.into_owned();
            let copy = store_parts(
                objects,
                ChangesetParts {
                    parents: vec![empty],
                    root: theirs.root,
                    changes,
                    author: theirs.author.clone(),
                    committer: theirs.committer.clone(),
                    message: theirs.message.clone(),
                    extras: theirs.extras.clone(),
                    git_origin: None,
                },
            )?;
            (copy, empty)
        }
    };
    let onto_root = read_changeset(objects, onto)?.root;
    let base_root = read_changeset(objects, base)?.root;
    let temporary = commit(
        objects,
        vec![base],
        diff(objects, base_root, onto_root)?,
        committer.clone(),
        committer.clone(),
        b"braid: temporary base of a rebase\n".to_vec(),
        vec![(TEMPORARY.to_vec(), onto.to_hex().into_bytes())],
    )?;
    let outcome = land(objects, Some(temporary), stand_in, committer.clone())?;
    let LandOutcome::Rebased {
        changeset,
        merged_paths,
    } = outcome
    else {
        return Ok(outcome);
    };
    // The same tree and change list, with `onto` as the first parent (the
    // temporary base has `onto`'s tree), naming the original change. The
    // other extra fields are the original's, its agent among them.
    let landed = read_changeset(objects, changeset)?;
    let changes = read_changes(objects, changeset, &landed)?.into_owned();
    let parents = std::iter::once(onto)
        .chain(landed.parents.iter().skip(1).copied())
        .collect();
    let mut extras: Vec<(Vec<u8>, Vec<u8>)> = landed
        .extras
        .into_iter()
        .filter(|(key, _)| key.as_slice() != REBASED_FROM)
        .collect();
    extras.push((REBASED_FROM.to_vec(), change.to_hex().into_bytes()));
    let rebased = store_parts(
        objects,
        ChangesetParts {
            parents,
            root: landed.root,
            changes,
            author: landed.author,
            committer: landed.committer,
            message: landed.message,
            extras,
            git_origin: None,
        },
    )?;
    Ok(LandOutcome::Rebased {
        changeset: rebased,
        merged_paths,
    })
}

/// Assembles and stores a changeset whose tree is stored already.
fn store_parts<O: Objects>(objects: &O, parts: ChangesetParts) -> Result<ObjectId, Error> {
    let assembled = parts.assemble()?;
    if let Some((_, list)) = &assembled.external_list {
        objects.put(Kind::Blob, list).map_err(store_error)?;
    }
    objects
        .put(Kind::Changeset, &assembled.encoded)
        .map_err(store_error)?;
    Ok(assembled.id)
}

fn store_error<E: std::error::Error + Send + Sync + 'static>(error: E) -> Error {
    Error::Engine(braid_engine::Error::Store(Box::new(error)))
}

/// A regular file's blob and executable bit.
fn regular(kind: Option<&ChangeOp>) -> Option<(ObjectId, bool)> {
    match kind {
        Some(ChangeOp::File { blob, .. }) => Some((*blob, false)),
        Some(ChangeOp::Executable { blob, .. }) => Some((*blob, true)),
        _ => None,
    }
}

/// The file at `path` of the tree `root`, if there is one there.
fn file_at<O: Objects>(
    objects: &O,
    root: ObjectId,
    path: &[u8],
) -> Result<Option<ChangeOp>, Error> {
    Ok(entry_at(objects, root, path)?
        .filter(|kind| !matches!(kind, EntryKind::Tree { .. }))
        .and_then(|kind| ChangeOp::from_entry(&kind)))
}

/// What to write when `change` stopped on `conflicts` against `onto`: the
/// tree of `onto`, with every path of the change that merges cleanly, and,
/// for each content conflict between two regular files, the merge with
/// conflict markers. Other conflicts keep `onto`'s version. Returns the
/// tree, stored, and the conflicts as the workspace records them.
pub(crate) fn conflict_tree<O: Objects>(
    objects: &O,
    onto: ObjectId,
    change: ObjectId,
    conflicts: &[Conflict],
    labels: (&str, &str),
) -> Result<(ObjectId, Vec<RecordedConflict>), Error> {
    let theirs = read_changeset(objects, change)?;
    let base_root = root_of(objects, theirs.parents.first().copied())?;
    let onto_root = read_changeset(objects, onto)?.root;
    let changes = read_changes(objects, change, &theirs)?.into_owned();
    let conflicting = |path: &[u8]| {
        conflicts
            .iter()
            .any(|c| is_within(path, &c.path) || is_within(&c.path, path))
    };

    let mut writes: Vec<Change> = Vec::new();
    for item in &changes {
        if conflicting(&item.path) {
            continue;
        }
        let ours = file_at(objects, onto_root, &item.path)?;
        let base = file_at(objects, base_root, &item.path)?;
        let theirs_op = (!matches!(item.op, ChangeOp::Delete)).then(|| item.op.clone());
        if theirs_op == ours {
            continue;
        }
        if base == ours {
            writes.push(item.clone());
            continue;
        }
        // Both changed it and the engine found no conflict: two regular
        // files whose lines merge.
        let (Some((o, o_exec)), Some((t, t_exec))) =
            (regular(ours.as_ref()), regular(theirs_op.as_ref()))
        else {
            continue;
        };
        let base_regular = regular(base.as_ref());
        let base_bytes = match base_regular {
            Some((b, _)) => read_blob(objects, b)?,
            None => Vec::new(),
        };
        let braid_engine::MergeResult::Clean(merged) = braid_engine::merge3(
            &base_bytes,
            &read_blob(objects, o)?,
            &read_blob(objects, t)?,
        ) else {
            continue;
        };
        let executable = match base_regular {
            _ if o_exec == t_exec => o_exec,
            Some((_, b_exec)) if b_exec == o_exec => t_exec,
            _ => o_exec,
        };
        let op = put_file(objects, &merged, executable)?;
        if Some(&op) != ours.as_ref() {
            writes.push(Change {
                path: item.path.clone(),
                op,
            });
        }
    }

    let mut recorded = Vec::new();
    for conflict in conflicts {
        let mut markers_written = false;
        if let braid_engine::ConflictKind::Content { .. } = conflict.kind {
            let ours = conflict.ours.as_ref().and_then(ChangeOp::from_entry);
            let theirs_op = conflict.theirs.as_ref().and_then(ChangeOp::from_entry);
            let base = conflict.base.as_ref().and_then(ChangeOp::from_entry);
            if let (Some((o, o_exec)), Some((t, _))) =
                (regular(ours.as_ref()), regular(theirs_op.as_ref()))
            {
                let base_bytes = match regular(base.as_ref()) {
                    Some((b, _)) => read_blob(objects, b)?,
                    None => Vec::new(),
                };
                let rendered = markers::render(
                    &base_bytes,
                    &read_blob(objects, o)?,
                    &read_blob(objects, t)?,
                    labels.0,
                    labels.1,
                );
                if let Some(content) = rendered {
                    writes.push(Change {
                        path: conflict.path.clone(),
                        op: put_file(objects, &content, o_exec)?,
                    });
                    markers_written = true;
                }
            }
        }
        if !recorded
            .iter()
            .any(|r: &RecordedConflict| r.path == conflict.path && r.code == conflict.kind.code())
        {
            recorded.push(RecordedConflict {
                path: conflict.path.clone(),
                code: conflict.kind.code().to_owned(),
                markers: markers_written,
                resolved: false,
            });
        }
    }
    writes.sort_by(|a, b| a.path.cmp(&b.path));
    writes.dedup_by(|a, b| a.path == b.path);
    let tree = braid_engine::apply(objects, onto_root, &writes)?;
    Ok((tree, recorded))
}

fn put_file<O: Objects>(objects: &O, content: &[u8], executable: bool) -> Result<ChangeOp, Error> {
    let blob = objects.put(Kind::Blob, content).map_err(store_error)?;
    let size = content.len() as u64;
    Ok(if executable {
        ChangeOp::Executable { blob, size }
    } else {
        ChangeOp::File { blob, size }
    })
}

/// The extra fields of a changeset: the agent that makes it.
pub(crate) fn agent_extras(agent: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![(AGENT.to_vec(), agent.as_bytes().to_vec())]
}

/// The extra fields of the changeset that finishes a rebase of `change`,
/// whose own are `original`: the same, as landing keeps them, with
/// [`REBASED_FROM`] naming `change`. The original's [`AGENT`] stays; a change
/// without one gets `agent`, who resolved it.
pub(crate) fn rebased_extras(
    original: &[(Vec<u8>, Vec<u8>)],
    change: ObjectId,
    agent: &str,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut extras: Vec<(Vec<u8>, Vec<u8>)> = original
        .iter()
        .filter(|(key, _)| key.as_slice() != REBASED_FROM)
        .cloned()
        .collect();
    if !extras.iter().any(|(key, _)| key.as_slice() == AGENT) {
        extras.splice(0..0, agent_extras(agent));
    }
    extras.push((REBASED_FROM.to_vec(), change.to_hex().into_bytes()));
    extras
}
