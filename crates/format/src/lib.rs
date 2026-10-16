// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The object model and canonical on-disk format of elffuss-braid.
//!
//! This crate does no I/O and must keep compiling for `wasm32-unknown-unknown`.
//! Everything that gets hashed is encoded by hand: those bytes are the format,
//! so they are specified in `docs/spec/format-v1.md` and pinned by golden
//! vectors in the tests.

#![forbid(unsafe_code)]

pub mod changeset;
pub mod codec;
pub mod id;
pub mod name;
pub mod objects;
pub mod tree;

pub use changeset::{
    Change, ChangeOp, Changes, Changeset, ChangesetError, ChangesetParts, Signature,
};
pub use codec::DecodeError;
pub use id::{IdHasher, Kind, ObjectId, ParseIdError};
pub use name::{NameError, PathError, validate_name, validate_path};
pub use objects::{MemoryObjects, Objects};
pub use tree::{
    Directory, Entry, EntryEdit, EntryKind, GitId, NewNode, Node, NodeCache, TreeError, Updated,
    build_directory, check_edits, find_entry, read_directory, update_directory,
};
