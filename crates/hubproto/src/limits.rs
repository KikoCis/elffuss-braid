// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The sizes of section 12 of `docs/hub.md` that do not depend on rate:
//! what one request may carry or ask for. A request over one of them is
//! answered 413 `too_large`, so a client splits its work to stay under
//! them.

/// The largest object a `PUT …/o/{id}` takes: 64 MiB. Larger objects go
/// through the object stream.
pub const MAX_PUT: u64 = 64 << 20;

/// The largest request body, which only an object stream reaches: 256 MiB.
/// It bounds each record of the stream too.
pub const MAX_BODY: u64 = 256 << 20;

/// The largest JSON request body: 4 MiB.
pub const MAX_JSON: usize = 4 << 20;

/// The most ids one list of a request holds: `lacks`, `want` and `have` of
/// `missing`, and the stack of a landing.
pub const MAX_IDS: usize = 10_000;

/// The most path prefixes one `missing` request names.
pub const MAX_PATHS: usize = 1_000;

/// Entries in a page of `missing` when the request gives no `limit`.
pub const MISSING_PAGE: usize = 10_000;

/// The most entries in a page of `missing`: a larger `limit` gives pages of
/// this size.
pub const MISSING_PAGE_MAX: usize = 100_000;

/// The most objects one `missing` walk lists, over all its pages, or in its
/// object stream. A larger walk is refused: ask for less history, for fewer
/// paths, or give what you hold in `have`.
pub const MISSING_OBJECTS: usize = 1_000_000;

/// The most entries of one tree listing (`GET …/t/{tree}`).
pub const TREE_ENTRIES: usize = 100_000;

/// The deepest tree listing: the most levels `depth` asks for.
pub const TREE_DEPTH: u32 = 64;

/// The most items of the other lists: an account's repositories and
/// tokens, and a repository's collaborators.
pub const MAX_LIST: usize = 1_000;
