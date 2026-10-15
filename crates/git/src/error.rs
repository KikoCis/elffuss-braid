// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Errors of the git bridge.

use core::fmt;

use braid_format::{Kind, ObjectId};

use crate::oid::{GitKind, GitOid};

/// An error from a lower layer: gitoxide or an object store.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Why an import, an export or a round trip failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// gitoxide could not open, read or write a repository.
    Git {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: BoxError,
    },
    /// The braid object store failed.
    Objects(BoxError),
    /// The repository is a shallow clone, so part of its history is missing.
    Shallow,
    /// The repository uses SHA-256 object ids, which the bridge does not
    /// support yet.
    Sha256,
    /// A ref that was asked for does not exist.
    MissingRef(Vec<u8>),
    /// A git object is not in the repository.
    MissingObject(GitOid),
    /// A git object is of another kind than the object naming it says.
    WrongKind {
        /// The object.
        oid: GitOid,
        /// The kind it should have.
        expected: GitKind,
        /// The kind it has.
        found: GitKind,
    },
    /// A git object's bytes do not hash to its id.
    CorruptObject(GitOid),
    /// SHA-1 collision detection found a known attack in an object.
    Collision {
        /// The kind of the object.
        kind: GitKind,
    },
    /// A git object cannot be parsed.
    Malformed {
        /// The object.
        oid: GitOid,
        /// Its kind.
        kind: GitKind,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A tree holds a name that braid refuses, such as an alias of `.git`.
    /// These rules protect every checkout made from the repository.
    RefusedPath {
        /// The commit whose tree holds the path, or the object a ref points
        /// to when no commit is involved.
        commit: GitOid,
        /// The full path from the root of that tree.
        path: Vec<u8>,
        /// Why the name is refused.
        reason: String,
    },
    /// A tree entry has a mode that is not a file, an executable, a symlink,
    /// a directory or a submodule.
    UnsupportedMode {
        /// The commit whose tree holds the entry, or the object a ref points
        /// to when no commit is involved.
        commit: GitOid,
        /// The full path of the entry.
        path: Vec<u8>,
        /// The mode as written in the tree.
        mode: Vec<u8>,
    },
    /// An imported object, exported again, did not give back its id.
    Verification {
        /// The object.
        oid: GitOid,
        /// Its kind.
        kind: GitKind,
        /// What differed.
        detail: String,
    },
    /// A braid object is missing from the store.
    MissingBraidObject {
        /// The object.
        id: ObjectId,
        /// The kind it should have.
        expected: Kind,
    },
    /// A braid object is not what the object naming it expects, or cannot be
    /// decoded.
    BadBraidObject {
        /// The object.
        id: ObjectId,
        /// What is wrong with it.
        detail: String,
    },
    /// The map does not know an object that an export needs.
    NotMapped {
        /// What kind of object.
        what: &'static str,
        /// Its id, as text.
        id: String,
    },
    /// The git commit a changeset was imported from disagrees with the
    /// changeset or with the map.
    OriginMismatch {
        /// The changeset.
        changeset: ObjectId,
        /// What disagrees.
        detail: String,
    },
    /// A changeset cannot be written as a git commit.
    Unrepresentable {
        /// The changeset.
        changeset: ObjectId,
        /// Why.
        detail: String,
    },
    /// A round trip found a difference between the source and the copy.
    RoundTrip(String),
}

/// Shows a path or a name, which are bytes, as text.
pub(crate) struct ShowBytes<'a>(pub &'a [u8]);

impl fmt::Display for ShowBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(self.0))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Git { context, source } => write!(f, "{context}: {source}"),
            Error::Objects(source) => write!(f, "object store: {source}"),
            Error::Shallow => {
                f.write_str("the repository is a shallow clone; importing needs its whole history")
            }
            Error::Sha256 => f.write_str(
                "the repository uses SHA-256 object ids, which the git bridge does not support yet",
            ),
            Error::MissingRef(name) => write!(f, "ref {} does not exist", ShowBytes(name)),
            Error::MissingObject(oid) => write!(f, "git object {oid} is missing"),
            Error::WrongKind {
                oid,
                expected,
                found,
            } => write!(f, "git object {oid} is a {found}, expected a {expected}"),
            Error::CorruptObject(oid) => write!(f, "git object {oid} does not hash to its id"),
            Error::Collision { kind } => {
                write!(f, "SHA-1 collision attack detected in a git {kind}")
            }
            Error::Malformed { oid, kind, reason } => {
                write!(f, "malformed git {kind} {oid}: {reason}")
            }
            Error::RefusedPath {
                commit,
                path,
                reason,
            } => write!(
                f,
                "commit {commit}: path {} is refused: {reason}",
                ShowBytes(path)
            ),
            Error::UnsupportedMode { commit, path, mode } => write!(
                f,
                "commit {commit}: path {} has the unsupported mode {}",
                ShowBytes(path),
                ShowBytes(mode)
            ),
            Error::Verification { oid, kind, detail } => {
                write!(f, "{kind} {oid} does not survive export: {detail}")
            }
            Error::MissingBraidObject { id, expected } => {
                write!(f, "braid {expected} {id} is missing")
            }
            Error::BadBraidObject { id, detail } => write!(f, "braid object {id}: {detail}"),
            Error::NotMapped { what, id } => write!(f, "the git map does not know {what} {id}"),
            Error::OriginMismatch { changeset, detail } => {
                write!(f, "changeset {changeset}: {detail}")
            }
            Error::Unrepresentable { changeset, detail } => write!(
                f,
                "changeset {changeset} cannot be written as a git commit: {detail}"
            ),
            Error::RoundTrip(detail) => write!(f, "round trip: {detail}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Git { source, .. } | Error::Objects(source) => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl Error {
    /// Wraps a gitoxide error with what was being done.
    pub(crate) fn git(context: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Error::Git {
            context: context.into(),
            source: source.into(),
        }
    }

    /// Wraps an object store error.
    pub(crate) fn objects(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Error::Objects(Box::new(source))
    }
}
