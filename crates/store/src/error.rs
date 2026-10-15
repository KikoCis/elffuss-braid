// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Errors returned by the store.

use std::io;
use std::path::{Path, PathBuf};

use braid_format::ObjectId;

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A file system call failed.
    #[error("cannot {action} {}: {source}", .path.display())]
    Io {
        /// What was being done, as a verb: "open", "read", "sync".
        action: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The error from the operating system.
        #[source]
        source: io::Error,
    },

    /// The directory has no `format` file, so it is not a store.
    #[error("{} is not a braid store", .path.display())]
    NotAStore {
        /// The directory that was opened.
        path: PathBuf,
    },

    /// The `format` file names a format this version does not understand.
    #[error("unsupported store format {found:?}")]
    UnsupportedFormat {
        /// The contents of the `format` file.
        found: String,
    },

    /// Stored data failed a check: a checksum, a length, a sequence, an id.
    /// The store never returns data that failed a check.
    #[error("corrupt data in {} at byte {offset}: {detail}", .path.display())]
    Corrupt {
        /// The damaged file.
        path: PathBuf,
        /// Where in the file the damage was found.
        offset: u64,
        /// What failed.
        detail: String,
    },

    /// An object whose stored form does not fit in one record.
    #[error("an object of {size} bytes does not fit in one record")]
    ObjectTooLarge {
        /// The size of the object's payload.
        size: u64,
    },

    /// A ref name the op log does not accept.
    #[error("invalid ref name {name:?}: {reason}")]
    InvalidRefName {
        /// The rejected name.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// An agent label, a description or a whole operation over its size limit.
    #[error("{what} is {len} bytes long, the limit is {max}")]
    TooLong {
        /// What was too long.
        what: &'static str,
        /// Its length in bytes.
        len: usize,
        /// The limit in bytes.
        max: usize,
    },

    /// The same ref appears twice in one transaction.
    #[error("ref {name:?} is updated twice in one transaction")]
    DuplicateRef {
        /// The repeated name.
        name: String,
    },

    /// A ref did not hold the value an update expected, so the transaction
    /// was not recorded. Read the refs again and retry.
    #[error("ref {name:?} is {}, expected {}", show(.found), show(.expected))]
    Conflict {
        /// The ref that moved.
        name: String,
        /// The value the update expected.
        expected: Option<ObjectId>,
        /// The value the ref actually had.
        found: Option<ObjectId>,
    },

    /// An update would point a ref at an object the store does not have.
    #[error("ref {name:?} cannot point at {id}: the object is not in the store")]
    MissingObject {
        /// The ref being updated.
        name: String,
        /// The object it would point at.
        id: ObjectId,
    },

    /// `undo_last` found no operation left to undo.
    #[error("no operation left to undo")]
    NothingToUndo,

    /// The op log was used from inside a transaction's closure, which would
    /// wait for the lock the transaction itself holds.
    #[error("the op log cannot be used from inside a transaction")]
    Reentrant,
}

impl Error {
    /// Whether this is a [`Error::Conflict`]: the refs moved, and a retry with
    /// fresh values may succeed.
    pub fn is_conflict(&self) -> bool {
        matches!(self, Error::Conflict { .. })
    }

    pub(crate) fn corrupt(path: &Path, offset: u64, detail: impl Into<String>) -> Self {
        Error::Corrupt {
            path: path.to_path_buf(),
            offset,
            detail: detail.into(),
        }
    }
}

/// Builds the error for a failed file system call on `path`.
pub(crate) fn io_error<'a>(
    action: &'static str,
    path: &'a Path,
) -> impl FnOnce(io::Error) -> Error + 'a {
    move |source| Error::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

fn show(id: &Option<ObjectId>) -> String {
    match id {
        Some(id) => id.to_hex(),
        None => "absent".to_owned(),
    }
}

/// Why a transaction did not record an operation.
#[derive(Debug, thiserror::Error)]
pub enum TransactError<E> {
    /// The closure returned this error.
    #[error("transaction aborted: {0}")]
    Aborted(E),
    /// The store refused the updates or failed: a conflict, a missing object,
    /// an I/O error.
    #[error(transparent)]
    Store(#[from] Error),
}

impl<E> TransactError<E> {
    /// Whether the transaction failed because a ref moved. Retrying with fresh
    /// values may succeed.
    pub fn is_conflict(&self) -> bool {
        matches!(self, TransactError::Store(err) if err.is_conflict())
    }
}
