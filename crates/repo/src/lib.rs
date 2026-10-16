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
//!
//! A revision is a ref name, a full object id, or a short id: at least
//! [`MIN_SHORT_ID`] hex digits that start the id of exactly one stored
//! object (see [`Repo::resolve`]).
//!
//! [`Repo::history`] answers history queries (ancestry, the changes to a
//! path, blame) with the derived data the store keeps, computing what is
//! missing.

#![forbid(unsafe_code)]

mod clock;
mod config;
mod git;
mod history;
mod link;
mod remote;
mod sync;
pub mod text;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use braid_format::{Kind, ObjectId, Objects};
use braid_store::{DerivedIndexFile, Options, Store};

pub use braid_engine::{Conflict, ConflictHunk, ConflictKind, HunkSide};
pub use clock::{local_now, signature_now};
pub use config::{CONFIG_FILE, Config};
pub use git::{
    GIT_EXPORT_AGENT, GitImport, KeptReason, KeptRef, MAP_REF, REMOTE_PREFIX, braid_ref_for_git,
    git_ref_for_braid, remote_ref,
};
pub use history::{RepoHistory, StoreIndex};
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
    /// A short id that several stored objects start with.
    #[error("{}", ambiguous(.prefix, .candidates, *.more))]
    AmbiguousRevision {
        /// The short id, as given.
        prefix: String,
        /// The first few objects it could name, sorted, with their kinds.
        candidates: Vec<(ObjectId, Kind)>,
        /// Whether more objects start with it than `candidates` lists.
        more: bool,
    },
    /// A revision that names an object of another kind than the one needed.
    #[error("{revision:?} is a {found}, not a {expected}")]
    WrongKind {
        /// The revision, as given.
        revision: String,
        /// The kind needed.
        expected: Kind,
        /// The kind of the object it names.
        found: Kind,
    },
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
    /// The store's derived index, read when first needed.
    derived: DerivedIndexFile,
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
            derived: DerivedIndexFile::new(&store_dir),
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
            derived: DerivedIndexFile::new(&store_dir),
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

    /// Resolves a revision to an object id: a ref name, a full id, or a
    /// short id, the first [`MIN_SHORT_ID`] or more hex digits of an id
    /// that no other stored object starts with. A ref wins over a short id
    /// that is spelled the same. Hex digits may be given in either case.
    pub fn resolve(&self, revision: &str) -> Result<ObjectId, Error> {
        self.lookup(revision, None)
    }

    /// Resolves a revision that must name an object of `kind`, as
    /// [`resolve`](Self::resolve) does, except that a short id only has to
    /// be unique among the objects of that kind.
    pub fn resolve_kind(&self, revision: &str, kind: Kind) -> Result<ObjectId, Error> {
        let id = self.lookup(revision, Some(kind))?;
        match self.store.stat(&id)? {
            Some((found, _)) if found == kind => Ok(id),
            Some((found, _)) => Err(Error::WrongKind {
                revision: revision.to_owned(),
                expected: kind,
                found,
            }),
            None => Err(Error::UnknownRevision(revision.to_owned())),
        }
    }

    fn lookup(&self, revision: &str, kind: Option<Kind>) -> Result<ObjectId, Error> {
        if let Some(id) = self.store.refs()?.get(revision) {
            return Ok(id);
        }
        let unknown = || Error::UnknownRevision(revision.to_owned());
        let digits = revision.len();
        if !(MIN_SHORT_ID..=2 * ObjectId::LEN).contains(&digits)
            || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(unknown());
        }
        let hex = revision.to_ascii_lowercase();
        if digits == 2 * ObjectId::LEN {
            let id: ObjectId = hex.parse().map_err(|_| unknown())?;
            return if self.store.contains(&id)? {
                Ok(id)
            } else {
                Err(unknown())
            };
        }
        let mut candidates = Vec::new();
        for id in self.store.ids_with_prefix(&hex, CANDIDATE_LIMIT)? {
            if let Some((found, _)) = self.store.stat(&id)? {
                candidates.push((id, found));
            }
        }
        // Objects of the kind wanted settle it, if there are any.
        if let Some(kind) = kind
            && candidates.iter().any(|(_, found)| *found == kind)
        {
            candidates.retain(|(_, found)| *found == kind);
        }
        match candidates.as_slice() {
            [] => Err(unknown()),
            [(id, _)] => Ok(*id),
            _ => {
                let more = candidates.len() > CANDIDATES_SHOWN;
                candidates.truncate(CANDIDATES_SHOWN);
                Err(Error::AmbiguousRevision {
                    prefix: revision.to_owned(),
                    candidates,
                    more,
                })
            }
        }
    }
}

/// The fewest hex digits a short id may have.
pub const MIN_SHORT_ID: usize = 6;

/// How many objects starting with a short id are looked at, at most.
const CANDIDATE_LIMIT: usize = 64;

/// How many of them an ambiguous short id lists.
const CANDIDATES_SHOWN: usize = 5;

/// The message for an ambiguous short id: each candidate with enough digits
/// to tell it from the others, and its kind.
fn ambiguous(prefix: &str, candidates: &[(ObjectId, Kind)], more: bool) -> String {
    let hexes: Vec<String> = candidates.iter().map(|(id, _)| id.to_hex()).collect();
    let mut shown = ObjectId::SHORT_HEX;
    for (i, a) in hexes.iter().enumerate() {
        for b in &hexes[i + 1..] {
            let shared = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
            shown = shown.max(shared + 1);
        }
    }
    let mut listed: Vec<String> = hexes
        .iter()
        .zip(candidates)
        .map(|(hex, (_, kind))| format!("{} ({kind})", &hex[..shown.min(hex.len())]))
        .collect();
    if more {
        listed.push("and more".to_owned());
    }
    format!(
        "the short id {prefix:?} is ambiguous: {}; give more digits",
        listed.join(", ")
    )
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

    /// Two payloads whose ids, as objects of `first` and of `second`, start
    /// with the same three bytes: six hex digits or more.
    fn twins(first: Kind, second: Kind) -> (Vec<u8>, Vec<u8>) {
        let start = |kind, payload: &[u8]| -> [u8; 3] {
            let bytes = ObjectId::hash(kind, payload);
            [
                bytes.as_bytes()[0],
                bytes.as_bytes()[1],
                bytes.as_bytes()[2],
            ]
        };
        let mut firsts = std::collections::HashMap::new();
        let mut seconds = std::collections::HashMap::new();
        for n in 0u64.. {
            let payload = format!("twin {n}").into_bytes();
            let (a, b) = (start(first, &payload), start(second, &payload));
            if let Some(other) = seconds.get(&a) {
                return (payload, Vec::clone(other));
            }
            if let Some(other) = firsts.get(&b) {
                return (Vec::clone(other), payload);
            }
            firsts.insert(a, payload.clone());
            seconds.insert(b, payload);
        }
        unreachable!("the search ends")
    }

    fn put(repo: &Repo, kind: Kind, payload: &[u8]) -> ObjectId {
        repo.store().put(kind, payload).unwrap()
    }

    #[test]
    fn short_ids() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        let (a, b) = twins(Kind::Blob, Kind::Blob);
        let (a, b) = (put(&repo, Kind::Blob, &a), put(&repo, Kind::Blob, &b));
        let (hex_a, hex_b) = (a.to_hex(), b.to_hex());
        let shared = hex_a
            .bytes()
            .zip(hex_b.bytes())
            .take_while(|(x, y)| x == y)
            .count();
        assert!(shared >= MIN_SHORT_ID);

        // Both start with six digits: an error that lists them.
        let err = repo.resolve(&hex_a[..MIN_SHORT_ID]).unwrap_err();
        let Error::AmbiguousRevision {
            candidates, more, ..
        } = &err
        else {
            panic!("expected an ambiguous short id, got {err:?}");
        };
        let mut both = vec![(a, Kind::Blob), (b, Kind::Blob)];
        both.sort();
        assert_eq!(*candidates, both);
        assert!(!more);
        let message = err.to_string();
        let digits = (shared + 1).max(ObjectId::SHORT_HEX);
        for hex in [&hex_a, &hex_b] {
            assert!(
                message.contains(&format!("{} (blob)", &hex[..digits])),
                "{message}"
            );
        }
        // One digit more tells them apart, in either case.
        assert_eq!(repo.resolve(&hex_a[..shared + 1]).unwrap(), a);
        let upper = hex_b[..shared + 1].to_ascii_uppercase();
        assert_eq!(repo.resolve(&upper).unwrap(), b);
        assert_eq!(repo.resolve(&hex_a.to_ascii_uppercase()).unwrap(), a);
        // Too short, not hex, or starting no id.
        let free = ["0", "f"]
            .into_iter()
            .find(|digit| !hex_a.starts_with(digit))
            .unwrap();
        for bad in [&hex_a[..MIN_SHORT_ID - 1], "zzzzzz", &free.repeat(8)] {
            assert!(
                matches!(repo.resolve(bad), Err(Error::UnknownRevision(_))),
                "{bad}"
            );
        }
        // A ref spelled like a short id wins.
        repo.store()
            .transact("test", "a ref that looks like an id", |_| {
                Ok::<_, std::convert::Infallible>(vec![RefUpdate::new(
                    &hex_a[..MIN_SHORT_ID],
                    None,
                    Some(b),
                )])
            })
            .unwrap();
        assert_eq!(repo.resolve(&hex_a[..MIN_SHORT_ID]).unwrap(), b);

        // A short id shared by objects of two kinds names the one of the
        // kind wanted.
        let (blob, changeset) = twins(Kind::Blob, Kind::Changeset);
        let blob = put(&repo, Kind::Blob, &blob);
        let changeset = put(&repo, Kind::Changeset, &changeset);
        let short = &blob.to_hex()[..MIN_SHORT_ID];
        assert!(matches!(
            repo.resolve(short),
            Err(Error::AmbiguousRevision { .. })
        ));
        assert_eq!(
            repo.resolve_kind(short, Kind::Changeset).unwrap(),
            changeset
        );
        assert_eq!(repo.resolve_kind(short, Kind::Blob).unwrap(), blob);
        let err = repo
            .resolve_kind(&blob.to_hex(), Kind::Changeset)
            .unwrap_err();
        assert!(
            matches!(
                err,
                Error::WrongKind {
                    expected: Kind::Changeset,
                    found: Kind::Blob,
                    ..
                }
            ),
            "{err:?}"
        );
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
