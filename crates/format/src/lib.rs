// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The object model and canonical on-disk format of elffuss-braid.
//!
//! This crate does no I/O and must keep compiling for `wasm32-unknown-unknown`.
//! Everything that gets hashed is encoded by hand: those bytes are the format,
//! so they are specified in `docs/spec/format-v1.md` and pinned by golden
//! vectors in the tests.

#![forbid(unsafe_code)]

pub mod id;

pub use id::{IdHasher, Kind, ObjectId, ParseIdError};
