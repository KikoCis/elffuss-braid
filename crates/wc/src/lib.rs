// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Workspaces of elffuss-braid: working copies that many agents use at once
//! on one repository, each landing on a branch without waiting for the
//! others.
//!
//! # Layout
//!
//! A workspace is any directory holding a `.braid` file. The file is local
//! and never tracked; it names the repository and the workspace (see
//! [`braid_repo::WorkspaceLink`]). The workspace's state lives in the
//! repository, in `.braid/ws/<name>/`:
//!
//! ```text
//! state    the base changeset, the branch, the directory, the file system's
//!          name rules, marks made by `braid track`, a rebase in progress
//! stat     the stat cache
//! lock     locked while a command changes the workspace
//! ```
//!
//! # Operations
//!
//! - [`Workspace::create`] writes a tree into a new directory.
//! - [`Workspace::status`] compares the working copy with the base, using
//!   the stat cache so that unchanged files are not read again, and
//!   directories that did not change are neither listed nor compared with
//!   their trees again.
//! - [`Workspace::commit`] records the working copy as a changeset on the
//!   base. There is no staging area; given paths, it reads only what lies at
//!   or below them.
//! - [`Workspace::land`] puts the workspace's changesets on its branch in
//!   one short op log transaction, fast-forwarding or rebasing them with the
//!   engine; conflicts come back as data and the branch does not move.
//! - [`Workspace::update`] brings the workspace onto the branch's head,
//!   rebasing its own changesets; conflicts are written with markers.
//! - [`create_change`] and [`land_change`] make and land a changeset
//!   without any working copy.
//!
//! # Safety of checkouts
//!
//! Every write into a working copy goes through directory handles that
//! never follow a symlink (see the `safe` module), through a scratch file
//! and a rename, with modes 0644 or 0755 only. Names this platform cannot
//! hold, and names that would collide on the workspace's file system, are
//! refused before anything is written. Nothing found in the repository is
//! ever run: there are no hooks, filters or attributes.
//!
//! New files that look like secrets (`.env*`, `*.pem`, `id_*`) or are
//! larger than 1 MiB are not committed until [`Workspace::track`] marks
//! them: [`Workspace::commit`] refuses while they wait, listing them with
//! the other reasons it has (see [`Blocker`]), unless it is forced, and then
//! leaves them out.

#![forbid(unsafe_code)]

mod blocker;
mod cache;
mod change;
mod checkout;
mod error;
mod fsprobe;
mod identity;
mod ignores;
mod markers;
mod paths;
mod rebase;
mod safe;
mod scan;
mod state;
mod status;
mod tree;
mod workspace;

pub use blocker::Blocker;
pub use change::{ChangeRequest, Expect, create_change, land_change};
pub use error::Error;
pub use fsprobe::FileSystem;
pub use identity::Identity;
pub use paths::workspace_path;
pub use state::{Checkout, Rebase, RecordedConflict, State};
pub use status::{ChangeKind, GUARD_SIZE, PathChange, Status, is_guarded};
pub use workspace::{
    BEHIND_LIMIT, CommitOutcome, CommitRequest, LandReport, Landed, Placed, RebaseProgress,
    StatusReport, UpdateReport, Updated, Workspace,
};
