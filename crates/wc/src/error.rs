// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Why a workspace operation failed.

use std::io;
use std::path::{Path, PathBuf};

use braid_format::ObjectId;

use crate::blocker::Blocker;
use crate::paths::show;

/// Why a workspace operation failed.
///
/// Conflicts are not errors: landing, updating and committing report them as
/// outcomes. These are the failures, after which the branch has not moved
/// and the workspace is as it was, except where a variant says otherwise.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A file system call failed.
    #[error("cannot {action} {}: {source}", .path.display())]
    Io {
        /// What was being done, as a verb.
        action: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error from the operating system.
        #[source]
        source: io::Error,
    },

    /// The object store failed.
    #[error(transparent)]
    Store(#[from] braid_store::Error),

    /// The repository could not be opened or read.
    #[error(transparent)]
    Repo(#[from] braid_repo::Error),

    /// The engine refused an operation or found invalid data.
    #[error(transparent)]
    Engine(#[from] braid_engine::Error),

    /// A changeset could not be assembled.
    #[error("cannot assemble a changeset: {0}")]
    Assemble(#[from] braid_format::ChangesetError),

    /// The current directory is not inside a workspace.
    #[error("not inside a workspace; create one with `braid workspace add <name>`")]
    NotAWorkspace,

    /// A workspace name that is not allowed.
    #[error(
        "invalid workspace name {0:?}: use 1 to 64 letters, digits, '.', '_' or '-', starting with a letter or digit"
    )]
    InvalidName(String),

    /// A workspace with this name exists already.
    #[error("workspace {0:?} exists already")]
    WorkspaceExists(String),

    /// No workspace has this name.
    #[error("no workspace named {0:?}")]
    NoSuchWorkspace(String),

    /// The directory for a new workspace exists and is not empty.
    #[error("{} exists and is not empty", .0.display())]
    DirectoryNotEmpty(PathBuf),

    /// A file of the workspace's state is damaged.
    #[error("{}: {detail}", .path.display())]
    BadState {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        detail: String,
    },

    /// A checkout would be unsafe or impossible: a name this platform or
    /// file system cannot hold, a symlink in the way, untracked data that
    /// would be overwritten. Nothing was written.
    #[error("cannot write {:?}: {reason}", show(.path))]
    Unsafe {
        /// The path in the repository.
        path: Vec<u8>,
        /// Why.
        reason: String,
    },

    /// Two paths of a tree would be the same file on this file system,
    /// which ignores letter case or Unicode normalisation. Neither was
    /// written.
    #[error("{:?} and {:?} would be the same file on this file system", show(.first), show(.second))]
    Collision {
        /// One path.
        first: Vec<u8>,
        /// The other.
        second: Vec<u8>,
    },

    /// The workspace has changes that are not committed.
    #[error("the workspace has uncommitted changes ({count} paths, starting with {:?}); commit them first", show(.first))]
    Uncommitted {
        /// How many paths changed.
        count: usize,
        /// The first of them.
        first: Vec<u8>,
    },

    /// The command needs the rebase in progress to be finished first.
    #[error("a rebase is in progress: resolve the conflicts and run `braid commit` first")]
    RebaseInProgress,

    /// `commit` was given paths while a rebase is in progress: the
    /// resolution is recorded whole.
    #[error("while a rebase is in progress, commit records the whole working copy: give no paths")]
    PathsDuringRebase,

    /// The workspace has changesets that its branch does not have.
    #[error(
        "the workspace has {count} changesets that {branch} does not have; land them, or pass --force"
    )]
    Unlanded {
        /// How many.
        count: usize,
        /// The branch.
        branch: String,
    },

    /// `resolve` was used with no rebase in progress.
    #[error("no rebase is in progress")]
    NoRebase,

    /// `resolve` named a path without a conflict, or a content conflict.
    #[error("{:?}: {reason}", show(.path))]
    NotResolvable {
        /// The path.
        path: Vec<u8>,
        /// Why it cannot be marked resolved.
        reason: &'static str,
    },

    /// The working copy cannot be committed as it is: conflict markers
    /// remain, conflicts are not marked resolved, or new files wait for
    /// `braid track`. Every reason is listed, as `status` lists them.
    #[error("cannot commit yet:\n{}\nSettle these first, or commit anyway with --force: files waiting for `braid track` stay out.", reasons(.0))]
    Blocked(Vec<Blocker>),

    /// There is nothing to commit.
    #[error("nothing to commit")]
    NothingToCommit,

    /// A commit needs a message.
    #[error("a commit needs a message: pass -m")]
    NoMessage,

    /// No author is configured.
    #[error(
        "no author: pass --author \"Name <email>\", set BRAID_AUTHOR, or set name and email in .braid/config"
    )]
    NoAuthor,

    /// An author that cannot be used.
    #[error("invalid author {text:?}: {reason}")]
    InvalidAuthor {
        /// What was given.
        text: String,
        /// Why it was refused.
        reason: &'static str,
    },

    /// A path given on the command line is outside the workspace.
    #[error("{} is outside the workspace", .0.display())]
    OutsideWorkspace(PathBuf),

    /// A path in the repository that is not valid.
    #[error("invalid path {:?}: {error}", show(.path))]
    InvalidPath {
        /// The path.
        path: Vec<u8>,
        /// What is wrong with it.
        error: braid_format::PathError,
    },

    /// `change create --expect` did not match the base.
    #[error("expected {:?} to be {expected} in the base, it is {found}", show(.path))]
    ExpectationFailed {
        /// The path.
        path: Vec<u8>,
        /// What was expected: a blob id or `absent`.
        expected: String,
        /// What the base has.
        found: String,
    },

    /// The workspace's base is not on the branch, so its changes cannot be
    /// landed as they are.
    #[error("the workspace's changes are not based on {branch}; run `braid update` first")]
    NotOnBranch {
        /// The branch.
        branch: String,
    },

    /// An object of the wrong kind where a changeset was expected.
    #[error("{0} is not a changeset")]
    NotAChangeset(ObjectId),
}

impl Error {
    /// Wraps an I/O error with the action and the path it happened on.
    pub(crate) fn io(
        action: &'static str,
        path: impl AsRef<Path>,
    ) -> impl FnOnce(io::Error) -> Self {
        let path = path.as_ref().to_path_buf();
        move |source| Error::Io {
            action,
            path,
            source,
        }
    }
}

/// How many reasons a refusal lists, at most.
const REASONS_SHOWN: usize = 20;

fn reasons(blockers: &[Blocker]) -> String {
    let mut shown: Vec<String> = blockers
        .iter()
        .take(REASONS_SHOWN)
        .map(|blocker| format!("  - {blocker}"))
        .collect();
    if blockers.len() > REASONS_SHOWN {
        shown.push(format!(
            "  - and {} more: see `braid status`",
            blockers.len() - REASONS_SHOWN
        ));
    }
    shown.join("\n")
}
