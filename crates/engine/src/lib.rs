// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The semantic core of elffuss-braid: what a change list means, and what
//! committing, admitting and landing a changeset do.
//!
//! This first part holds [`merge3`], the three-way merge of file contents.

#![forbid(unsafe_code)]

mod merge;

pub use merge::{BINARY_SNIFF_BYTES, ConflictHunk, HunkSide, MergeResult, is_binary, merge3};
