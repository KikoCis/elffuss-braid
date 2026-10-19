// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Why an engine operation failed.

use braid_format::{ChangesetError, Kind, ObjectId, PathError, TreeError};

use crate::path::show;

/// An error reported by an object store, boxed so that [`Error`] does not
/// depend on the store type.
pub type StoreError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Why an engine operation failed.
///
/// [`Error::Store`] is the only variant that says nothing about the data: the
/// store could not be read or written, and trying again may work. Every other
/// variant means that an object or an argument is not valid, and is how
/// [`admit`](crate::admit) explains a rejection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The object store failed.
    #[error("object store error: {0}")]
    Store(#[source] StoreError),

    /// An object that must be present is not.
    #[error("{kind} {id} is missing")]
    Missing {
        /// The missing object.
        id: ObjectId,
        /// What it was expected to be.
        kind: Kind,
    },

    /// An object is stored as another kind than the one it is used as.
    #[error("object {id} is a {found}, not a {expected}")]
    WrongKind {
        /// The object.
        id: ObjectId,
        /// The kind it is used as.
        expected: Kind,
        /// The kind it is stored as.
        found: Kind,
    },

    /// The bytes stored under an id do not hash to that id.
    #[error("object {id} does not match its id")]
    Corrupt {
        /// The object.
        id: ObjectId,
    },

    /// A stored tree is not valid, or not the canonical split of its entries.
    #[error("tree {id} is not valid: {error}")]
    Tree {
        /// The root of the directory.
        id: ObjectId,
        /// What is wrong with it.
        #[source]
        error: TreeError,
    },

    /// Entries do not form a valid directory.
    #[error("cannot build a directory: {0}")]
    Entries(#[source] TreeError),

    /// A stored changeset, or its change list, is not valid.
    #[error("changeset {id} is not valid: {error}")]
    Changeset {
        /// The changeset.
        id: ObjectId,
        /// What is wrong with it.
        #[source]
        error: ChangesetError,
    },

    /// The parts of a new changeset do not form a valid one.
    #[error("cannot assemble the changeset: {0}")]
    Assemble(#[source] ChangesetError),

    /// An external change list does not hold as many changes as its changeset
    /// declares.
    #[error("changeset {changeset} declares {declared} changes, its list holds {found}")]
    ChangeCount {
        /// The changeset.
        changeset: ObjectId,
        /// The count in the changeset.
        declared: u64,
        /// The number of changes in the list.
        found: usize,
    },

    /// A change list is stored in its own blob although its encoding is short
    /// enough to be stored inline, which is not canonical.
    #[error(
        "changeset {changeset} keeps a change list of {len} bytes in a blob, short enough to be inline"
    )]
    ExternalListFitsInline {
        /// The changeset.
        changeset: ObjectId,
        /// Length of the encoded list.
        len: usize,
    },

    /// A change list does not apply to its base tree.
    #[error("cannot apply the change to {:?}: {problem}", show(.path))]
    Apply {
        /// Where the problem is: the path of a change, or the prefix of one
        /// that is in the way.
        path: Vec<u8>,
        /// What the problem is.
        problem: ApplyProblem,
    },

    /// A parent of a changeset has not been admitted.
    #[error("parent {parent} of changeset {changeset} has not been admitted")]
    ParentNotAdmitted {
        /// The changeset.
        changeset: ObjectId,
        /// The parent that is not admitted.
        parent: ObjectId,
    },

    /// A changeset's change list, applied to its first parent, does not give
    /// the root tree the changeset declares.
    #[error("changeset {changeset} declares root {declared}, its changes give {computed}")]
    RootMismatch {
        /// The changeset.
        changeset: ObjectId,
        /// The root the changeset names.
        declared: ObjectId,
        /// The root its change list produces.
        computed: ObjectId,
    },

    /// A blob does not have the size a change gives it.
    #[error("blob {blob} holds {actual} bytes, not {declared}")]
    BlobSize {
        /// The blob.
        blob: ObjectId,
        /// The size in the change.
        declared: u64,
        /// The size of the stored blob.
        actual: u64,
    },

    /// The base of a change is not on the first-parent chain of the branch
    /// head it is landing on.
    #[error("the base of the change ({}) is not on the branch (head {})", show_id(.base), show_id(.head))]
    BaseNotOnBranch {
        /// The change's first parent, if it has one.
        base: Option<ObjectId>,
        /// The branch head, if the branch has one.
        head: Option<ObjectId>,
    },

    /// A path has more names than [`MAX_PATH_DEPTH`](crate::MAX_PATH_DEPTH),
    /// the limit a store sets on how deep a tree may be.
    #[error("{:?} has more than {} names", show(.path), crate::MAX_PATH_DEPTH)]
    TooDeep {
        /// The path of a change, or the first path deeper than the limit
        /// that a walk over a tree met.
        path: Vec<u8>,
    },

    /// Two changesets to merge have no common ancestor.
    #[error("changesets {} and {} have no common ancestor", .ours.short(), .theirs.short())]
    UnrelatedHistories {
        /// The changeset merged into.
        ours: ObjectId,
        /// The changeset merged.
        theirs: ObjectId,
    },

    /// Derived data about a changeset contradicts the history it was derived
    /// from: a record made by a faulty version, or tampered with.
    #[error("the derived data of {id} contradicts its history")]
    DerivedData {
        /// The changeset.
        id: ObjectId,
    },

    /// A walk would list more objects than its caller allows (see
    /// [`Reach::max_objects`](crate::Reach::max_objects)).
    #[error("the walk reaches more than {limit} objects")]
    TooManyObjects {
        /// The most objects the walk could list.
        limit: usize,
    },

    /// Two changesets to merge have several best common ancestors, as after
    /// criss-cross merges, with different trees: the merge would depend on
    /// which one it started from.
    #[error(
        "changesets {} and {} have {} best common ancestors with different trees",
        .ours.short(),
        .theirs.short(),
        .bases.len()
    )]
    SeveralMergeBases {
        /// The changeset merged into.
        ours: ObjectId,
        /// The changeset merged.
        theirs: ObjectId,
        /// The best common ancestors, sorted by id.
        bases: Vec<ObjectId>,
    },
}

impl Error {
    /// Whether the store failed, as opposed to the data being invalid.
    pub fn is_store_failure(&self) -> bool {
        matches!(self, Error::Store(_))
    }
}

/// Wraps a store error.
pub(crate) fn store_error<E: std::error::Error + Send + Sync + 'static>(error: E) -> Error {
    Error::Store(Box::new(error))
}

fn show_id(id: &Option<ObjectId>) -> String {
    id.map_or_else(|| "none".to_owned(), |id| id.short())
}

/// Why a change list does not apply to a tree.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ApplyProblem {
    /// The path is not valid.
    #[error("the path is not valid: {0}")]
    InvalidPath(PathError),
    /// A symlink takes a name that git or braid reads.
    #[error("this name must not be a symlink")]
    SymlinkName,
    /// More than one change names the path.
    #[error("more than one change names this path")]
    Duplicate,
    /// A change below this path needs it to be a directory, and it is a file,
    /// a symlink or a gitlink that the change list does not delete.
    #[error("a change below this path needs a directory, and it is not one")]
    NotADirectory,
    /// A change sets this path, and a directory with entries remains there.
    #[error("a file is set where a directory with entries remains")]
    DirectoryNotEmpty,
    /// A change deletes this path, which is a directory: changes are per
    /// file, so a directory goes away when its files do.
    #[error("the path is a directory; delete its files instead")]
    IsADirectory,
    /// A change deletes this path, which does not exist.
    #[error("the path to delete does not exist")]
    DeleteMissing,
    /// A change sets this path to the entry it already has.
    #[error("the path already has this entry")]
    NoOp,
}
