// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! A braid repository on disk: where it is, how to open it, how names
//! resolve to objects, and how it trades history with git repositories.
//!
//! A repository is a directory holding a `.braid` directory. Inside it,
//! `store/` is the object store and op log shared by every process and every
//! workspace of the repository, `git/` holds the mirrors of the git remotes
//! that [`Repo::git_sync`] keeps in step with, and the optional `config` file
//! holds settings (see [`Config`]).
//!
//! A workspace is any directory holding a `.braid` *file* instead: a
//! [`WorkspaceLink`] naming the repository and the workspace. [`locate`] and
//! [`Repo::discover`] find the repository from inside either.

#![forbid(unsafe_code)]

mod config;
mod git;
mod link;
mod remote;
mod sync;
pub mod text;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use braid_format::{ObjectId, Objects};
use braid_store::{Options, Store};

pub use braid_engine::{Conflict, ConflictHunk, ConflictKind, HunkSide};
pub use config::{CONFIG_FILE, Config};
pub use git::{
    GIT_EXPORT_AGENT, GitImport, MAP_REF, REMOTE_PREFIX, braid_ref_for_git, git_ref_for_braid,
    remote_ref,
};
pub use link::{Location, WORKSPACES_DIR, WorkspaceLink, locate};
pub use remote::Remote;
pub use sync::{MIRRORS_DIR, SyncOptions, SyncOutcome, SyncReport};

/// Name of the directory that holds a repository's data.
pub const DIR: &str = ".braid";

/// Name of the object store inside [`DIR`].
pub const STORE_DIR: &str = "store";

/// Why a repository operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No `.braid` directory or workspace file in the starting directory or
    /// any parent.
    #[error("not inside a braid repository")]
    NotFound,
    /// A workspace's `.braid` file cannot be read.
    #[error("{}: not a valid workspace file: {detail}", .path.display())]
    BadWorkspaceFile {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        detail: String,
    },
    /// `init` on a directory that already holds a repository.
    #[error("a braid repository already exists here")]
    AlreadyExists,
    /// A name that is neither a ref nor the id of a stored object.
    #[error("no ref or object named {0:?}")]
    UnknownRevision(String),
    /// The store failed.
    #[error(transparent)]
    Store(#[from] braid_store::Error),
    /// The git bridge failed.
    #[error(transparent)]
    Git(#[from] braid_git::Error),
    /// The stored git map does not decode.
    #[error("the stored git map is damaged: {0}")]
    GitMap(braid_format::DecodeError),
    /// A ref names an object the git bridge cannot export.
    #[error("ref {name:?} points at an object that has no git form")]
    NotExportable {
        /// The ref.
        name: String,
    },
    /// The file system failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A merge, or a walk of history, failed.
    #[error(transparent)]
    Engine(#[from] braid_engine::Error),
    /// A git remote braid does not hand to git.
    #[error("invalid remote {remote:?}: {reason}")]
    InvalidRemote {
        /// The remote as given.
        remote: String,
        /// Why it is refused.
        reason: &'static str,
    },
    /// A branch name sync cannot use.
    #[error("invalid branch name {name:?}: {reason}")]
    InvalidBranch {
        /// The name.
        name: String,
        /// Why it is refused.
        reason: &'static str,
    },
    /// An agent label that cannot name the committer of a git commit.
    #[error("the agent label {agent:?} cannot name a git committer: {reason}")]
    InvalidAgent {
        /// The label.
        agent: String,
        /// Why it cannot.
        reason: &'static str,
    },
    /// A setting of the repository's configuration that cannot be used.
    #[error("the setting {key:?} in .braid/config cannot be used: {reason}")]
    InvalidConfig {
        /// The setting.
        key: &'static str,
        /// Why.
        reason: &'static str,
    },
    /// A branch that should point at a changeset, or a remote branch that
    /// should point at a commit, does not.
    #[error("{what} does not point at a changeset")]
    NotAChangeset {
        /// Which branch.
        what: String,
    },
    /// Neither braid nor the remote has the branch to sync.
    #[error("neither braid nor the remote has the branch {branch:?}")]
    NothingToSync {
        /// The braid branch.
        branch: String,
    },
    /// Another operation moved a branch while sync was working on it.
    /// Nothing was pushed; syncing again starts from where it is now.
    #[error("branch {branch:?} moved during the sync; nothing was pushed, sync again")]
    BranchMoved {
        /// The branch.
        branch: String,
    },
    /// A ref kept moving, so an operation could not be recorded.
    #[error("ref {name:?} keeps moving; try again")]
    Busy {
        /// The ref.
        name: String,
    },
    /// A git command failed.
    #[error("git {command} failed: {detail}")]
    GitCommand {
        /// The git command.
        command: &'static str,
        /// What git said, or why it could not run.
        detail: String,
    },
}

/// An open repository.
#[derive(Debug)]
pub struct Repo {
    root: PathBuf,
    store: Store,
}

impl Repo {
    /// Creates a repository in `dir`, which is created if needed.
    pub fn init(dir: &Path) -> Result<Repo, Error> {
        let store_dir = dir.join(DIR).join(STORE_DIR);
        if store_dir.exists() {
            return Err(Error::AlreadyExists);
        }
        fs::create_dir_all(dir.join(DIR))?;
        let store = Store::init(&store_dir, Options::default())?;
        Ok(Repo {
            root: dir.to_path_buf(),
            store,
        })
    }

    /// Opens the repository whose root is `dir`.
    pub fn open(dir: &Path) -> Result<Repo, Error> {
        let store_dir = dir.join(DIR).join(STORE_DIR);
        if !store_dir.is_dir() {
            return Err(Error::NotFound);
        }
        let store = Store::open(&store_dir, Options::default())?;
        Ok(Repo {
            root: dir.to_path_buf(),
            store,
        })
    }

    /// Opens the repository that contains `start`: `start` itself or the
    /// nearest parent with a `.braid` directory, or the repository named by
    /// the `.braid` file of the workspace that contains `start`.
    pub fn discover(start: &Path) -> Result<Repo, Error> {
        match locate(start)? {
            Location::Root(dir) => Repo::open(&dir),
            Location::Workspace { link, .. } => Repo::open(&link.repository),
        }
    }

    /// The directory that holds `.braid`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The object store and op log.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Resolves a ref name or a full object id to an object id.
    pub fn resolve(&self, revision: &str) -> Result<ObjectId, Error> {
        if let Some(id) = self.store.refs()?.get(revision) {
            return Ok(id);
        }
        if let Ok(id) = revision.parse::<ObjectId>()
            && self.store.contains(&id)?
        {
            return Ok(id);
        }
        Err(Error::UnknownRevision(revision.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::Kind;
    use braid_store::RefUpdate;

    #[test]
    fn init_open_discover_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        assert!(matches!(Repo::init(dir.path()), Err(Error::AlreadyExists)));

        let id = repo.store().put(Kind::Blob, b"hello\n").unwrap();
        repo.store()
            .transact("test", "set main", |_| {
                Ok::<_, std::convert::Infallible>(vec![RefUpdate::new("main", None, Some(id))])
            })
            .unwrap();

        let nested = dir.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        let found = Repo::discover(&nested).unwrap();
        assert_eq!(found.root(), dir.path());
        assert_eq!(found.resolve("main").unwrap(), id);
        assert_eq!(found.resolve(&id.to_hex()).unwrap(), id);
        assert!(matches!(
            found.resolve("nope"),
            Err(Error::UnknownRevision(_))
        ));
        let absent = ObjectId::hash(Kind::Blob, b"absent").to_hex();
        assert!(matches!(
            found.resolve(&absent),
            Err(Error::UnknownRevision(_))
        ));

        let elsewhere = tempfile::tempdir().unwrap();
        assert!(matches!(
            Repo::discover(elsewhere.path()),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn discover_through_a_workspace_file() {
        let repo_dir = tempfile::tempdir().unwrap();
        Repo::init(repo_dir.path()).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let workspace = elsewhere.path().join("agent-1");
        fs::create_dir_all(workspace.join("src/deep")).unwrap();
        let link = WorkspaceLink {
            repository: repo_dir.path().to_path_buf(),
            name: "agent-1".to_owned(),
        };
        fs::write(workspace.join(DIR), link.encode()).unwrap();

        let found = Repo::discover(&workspace.join("src/deep")).unwrap();
        assert_eq!(found.root(), repo_dir.path());
        match locate(&workspace.join("src")).unwrap() {
            Location::Workspace { dir, link: read } => {
                assert_eq!(dir, workspace);
                assert_eq!(read, link);
            }
            other => panic!("expected a workspace, got {other:?}"),
        }

        // A damaged file is an error, not a reason to keep looking.
        fs::write(workspace.join(DIR), b"something else\n").unwrap();
        assert!(matches!(
            Repo::discover(&workspace),
            Err(Error::BadWorkspaceFile { .. })
        ));
    }
}
