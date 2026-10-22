// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The BraidHub protocol, version 1: what the hub and the client (`braid
//! login`, `braid hub …`, and landing in a linked repository) say to each
//! other. `docs/hub.md` is the prose of the same contract.
//!
//! - [`names`]: owners, repository names and ref globs.
//! - [`token`]: the `brd_<id>_<secret>` token format.
//! - [`scope`]: what a token's scopes reach and allow.
//! - [`stream`]: the `application/x-braid-objects` object stream.
//! - [`api`]: the JSON bodies of the routes of sections 1 to 14.
//! - [`cards`]: repositories and accounts as lists hold them.
//! - [`browse`], [`history`], [`explore`], [`social`], [`issues`],
//!   [`requests`], [`forks`], [`notifications`], [`moderation`] and
//!   [`onboard`]: the bodies of sections 15 to 23.
//! - [`verify`]: keeping a default branch green, section 25.
//! - [`json`]: conflicts and changes as the CLI prints them, so a landing on
//!   the hub reports exactly what a local landing does.
//! - [`limits`]: what one request may carry or ask for.

#![forbid(unsafe_code)]

pub mod api;
pub mod browse;
pub mod cards;
pub mod explore;
pub mod forks;
pub mod history;
pub mod issues;
pub mod json;
pub mod limits;
pub mod moderation;
pub mod names;
pub mod notifications;
pub mod onboard;
pub mod requests;
pub mod scope;
pub mod social;
pub mod stream;
pub mod token;
pub mod verify;

mod hex;

pub use names::{NameError, RepoPath};
pub use scope::{ParsedScope, Right, ScopeError, ScopeRepo};
pub use token::{Token, TokenError};

/// The protocol version this crate speaks: the `v1` of every route.
pub const VERSION: u32 = 1;

/// The media type of an object stream.
pub const OBJECTS_MEDIA_TYPE: &str = "application/x-braid-objects";

/// The header naming an object's kind on `GET` and `PUT` of `o/{id}`.
pub const KIND_HEADER: &str = "Braid-Kind";

/// The header naming a git object's kind on `GET git/o/{sha1}`.
pub const GIT_KIND_HEADER: &str = "Git-Kind";

/// The media type of the ref watch.
pub const EVENTS_MEDIA_TYPE: &str = "text/event-stream";

/// The public hub, used when nothing else is configured.
pub const DEFAULT_HUB_URL: &str = "https://braidhub.elffuss.com";
