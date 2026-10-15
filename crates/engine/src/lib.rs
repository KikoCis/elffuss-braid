// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The semantic core of elffuss-braid: what a change list means, and what
//! committing, admitting and landing a changeset do.
//!
//! Everything here reads and writes through the [`Objects`] trait and does no
//! I/O of its own, so the same code runs over the local store, a remote one, or
//! memory.
//!
//! - [`read_tree`], [`write_tree`], [`read_changeset`], [`read_changes`]:
//!   typed access to stored objects.
//! - [`apply`] and [`diff`]: a change list, and the two trees it connects.
//! - [`commit`] and [`admit`]: creating a changeset, and the check a store
//!   runs before it accepts one.
//! - [`first_parent_chain_contains`], [`is_ancestor`] and [`merge_base`]:
//!   ancestry, walking history.
//! - [`History`]: the same ancestry queries with generation numbers and skip
//!   pointers, answered with derived data ([`DerivedKey`], [`DerivedIndex`])
//!   computed when missing.
//! - [`merge3`]: three-way merges of file contents.
//! - [`land`]: landing a change on a branch, with conflicts reported as data.
//! - [`merge`]: merging two changesets into a merge changeset, with the same
//!   rules and conflicts as landing.
//!
//! # Files and directories
//!
//! A change list is per file: each change sets one path to a file, an
//! executable, a symlink or a gitlink, or deletes it. Directories follow from
//! the files: a change below a missing directory creates it, and a directory
//! left without entries by a change is removed, as git has no empty
//! directories. Braid trees never hold an empty subdirectory: no change list
//! makes one, and the git bridge drops them when it converts git trees.
//!
//! A path has at most [`MAX_PATH_DEPTH`] names. This is a store policy, not
//! part of the hashed format: [`apply`] and [`admit`] refuse deeper paths, so
//! no stored tree is deeper, and every walk over a tree stops with
//! [`Error::TooDeep`] beyond it. The walks keep their own stacks rather than
//! recursing, so the limit does not depend on the size of the thread's stack.
//!
//! # Costs
//!
//! [`apply`], [`diff`], [`admit`], [`land`] and [`merge`] read only the
//! directories on the changed paths, so their cost follows the size of the
//! change times the depth of the tree, plus the size of each directory they
//! touch. The free ancestry functions walk history and are proportional to
//! the distance they cover; [`History`] reads derived data instead, so its
//! queries cost what they visit once that data exists.
//!
//! [`Objects`]: braid_format::Objects

#![forbid(unsafe_code)]

mod access;
mod ancestry;
mod apply;
mod commit;
mod derived;
mod diff;
mod error;
mod history;
mod land;
mod merge;
mod path;
mod tree_merge;

#[cfg(test)]
mod testing;

pub use access::{
    EMPTY_TREE, entry_at, read_blob, read_changes, read_changeset, read_object, read_tree,
    stat_object, write_tree,
};
pub use ancestry::{
    GraphNode, first_parent_chain_contains, is_ancestor, merge_base, merge_bases, skip_height,
};
pub use apply::apply;
pub use commit::{admit, commit};
pub use derived::{DerivedIndex, DerivedKey, MemoryIndex};
pub use diff::diff;
pub use error::{ApplyProblem, Error, StoreError};
pub use history::History;
pub use land::{AGENT, Conflict, ConflictKind, LandOutcome, REBASED_FROM, land};
pub use merge::{BINARY_SNIFF_BYTES, ConflictHunk, HunkSide, MergeResult, is_binary, merge3};
pub use path::MAX_PATH_DEPTH;
pub use tree_merge::{MergeOutcome, merge};
