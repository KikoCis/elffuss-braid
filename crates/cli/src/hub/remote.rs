// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! One repository on a hub, as the transfer logic sees it: its refs, its
//! objects and its landings, behind a small trait. Fetching, publishing and
//! landing are written against [`Remote`], so that they run over HTTP
//! ([`super::http`]) and, in tests, against a hub held in memory.

use std::fmt;
use std::io::{self, Read};

use braid_format::{Kind, ObjectId};
use braid_hubproto::api::{ErrorBody, LandRequest, LandResponse, MissingRequest, Refs, RepoInfo};
use braid_hubproto::limits;

use super::creds::{TokenSource, redact};

/// At most this many ids go in one `lacks` request.
pub(crate) const LACKS_BATCH: usize = limits::MAX_IDS;

/// The largest request body the hub takes: 256 MiB.
pub(crate) const MAX_BODY: u64 = limits::MAX_BODY;

/// One repository on a hub: the routes of sections 4, 6 and 8 of
/// `docs/hub.md` that moving history needs.
pub(crate) trait Remote {
    /// `GET …/refs`: every ref, with the op log's sequence number.
    fn refs(&self) -> Result<Refs, HubError>;

    /// `GET /v1/r/{owner}/{name}`: the repository itself.
    fn info(&self) -> Result<RepoInfo, HubError>;

    /// `HEAD …/o/{id}`: the kind of an object, or `None` when the hub does
    /// not hold it.
    fn kind(&self, id: &ObjectId) -> Result<Option<Kind>, HubError>;

    /// `POST …/lacks`: the ids among `ids`, at most [`LACKS_BATCH`] of them,
    /// that the hub does not store.
    fn lacks(&self, ids: &[ObjectId]) -> Result<Vec<ObjectId>, HubError>;

    /// `POST …/objects`: an object stream of exactly `len` bytes, read from
    /// `body`. Returns how many records the hub stored.
    fn send_objects(&self, body: &mut dyn Read, len: u64) -> Result<u64, HubError>;

    /// `POST …/missing`, answered as an object stream: what a client needs
    /// to hold `request.want`, less what `request.have` reaches.
    fn missing(&self, request: &MissingRequest) -> Result<Box<dyn Read + '_>, HubError>;

    /// `POST …/land`. Refusals the protocol names (`missing`,
    /// `not_on_branch`) come back as [`HubError::Api`].
    fn land(&self, request: &LandRequest) -> Result<LandResponse, HubError>;

    /// The largest body an upload sends: [`MAX_BODY`], unless the client
    /// was told to send less or the hub takes less.
    fn max_body(&self) -> u64 {
        MAX_BODY
    }

    /// The most ids one list of a request holds (`lacks`, `want`, `have`,
    /// a stack): [`LACKS_BATCH`], unless the hub takes fewer.
    fn max_ids(&self) -> usize {
        LACKS_BATCH
    }
}

/// Why a request to a hub failed. No variant ever holds a token, and what
/// a hub says is shown with [`shown`].
#[derive(Debug)]
pub(crate) enum HubError {
    /// The hub answered with an error: its HTTP status and its body.
    Api {
        /// The HTTP status.
        status: u16,
        /// `{"error", "detail"}`, with `ids` or `retry_after` where the
        /// protocol adds them.
        body: ErrorBody,
    },
    /// The hub refused the token the request carried (401): expired or
    /// revoked. Where the token came from says what to do.
    Refused {
        /// The hub's URL.
        hub: String,
        /// Where the token came from.
        source: TokenSource,
        /// What the hub said.
        detail: String,
    },
    /// The hub could not be reached, or the connection broke.
    Transport {
        /// The hub's URL.
        hub: String,
        /// What went wrong.
        detail: String,
    },
    /// The hub answered something the protocol does not allow.
    Protocol(String),
    /// Reading or writing a body failed.
    Io(io::Error),
}

/// Text from a hub, or from a proxy in front of it, as braid shows it:
/// control characters escaped, so that none reaches a terminal raw, and the
/// secret of any token in it left out.
pub(crate) fn shown(text: &str) -> String {
    let text = redact(text);
    if !text.chars().any(char::is_control) {
        return text.into_owned();
    }
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

impl HubError {
    /// Whether the request may have taken effect although it failed: the
    /// connection broke, the answer was not one, or the hub, or a proxy in
    /// front of it, failed (5xx).
    pub(crate) fn may_have_happened(&self) -> bool {
        match self {
            HubError::Api { status, .. } => *status >= 500,
            HubError::Refused { .. } => false,
            HubError::Transport { .. } | HubError::Protocol(_) | HubError::Io(_) => true,
        }
    }

    /// Whether the hub refused the credentials, or wants some (401).
    pub(crate) fn unauthorized(&self) -> bool {
        match self {
            HubError::Api { status, body } => *status == 401 || body.error == "unauthorized",
            HubError::Refused { .. } => true,
            _ => false,
        }
    }

    /// Whether the hub asked to wait (429).
    pub(crate) fn rate_limited(&self) -> bool {
        matches!(self, HubError::Api { status, body } if *status == 429 || body.error == "rate_limited")
    }
    /// An error answer, as a hub gives it.
    #[cfg(test)]
    pub(crate) fn api(status: u16, error: &str, detail: impl Into<String>) -> HubError {
        HubError::Api {
            status,
            body: ErrorBody {
                error: error.to_owned(),
                detail: detail.into(),
                ids: Vec::new(),
                retry_after: None,
                head: None,
            },
        }
    }

    /// The protocol's error code (`missing`, `not_on_branch`, …), for an
    /// error answer.
    pub(crate) fn code(&self) -> Option<&str> {
        match self {
            HubError::Api { body, .. } => Some(&body.error),
            HubError::Refused { .. } => Some("unauthorized"),
            _ => None,
        }
    }

    /// With `missing`: the objects the hub says it needs.
    pub(crate) fn missing_ids(&self) -> &[ObjectId] {
        match self {
            HubError::Api { body, .. } if body.error == "missing" => &body.ids,
            _ => &[],
        }
    }
}

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HubError::Refused {
                hub,
                source,
                detail,
            } => {
                let detail = shown(detail);
                let flag = if hub == braid_hubproto::DEFAULT_HUB_URL {
                    String::new()
                } else {
                    format!(" --hub {hub}")
                };
                match source {
                    TokenSource::Environment => write!(
                        f,
                        "the hub needs valid credentials ({detail}): {hub} refused BRAID_TOKEN, which expired or was revoked; set another, or unset it and run `braid login{flag}`"
                    ),
                    _ => write!(
                        f,
                        "the hub needs valid credentials ({detail}): {hub} refused the token saved for it, which expired or was revoked; run `braid login{flag}`"
                    ),
                }
            }
            HubError::Api { status, body } => {
                let detail = shown(&body.detail);
                match (*status, body.error.as_str()) {
                    (401, _) | (_, "unauthorized") => write!(
                        f,
                        "the hub needs valid credentials ({detail}): sign in with `braid login`, or set BRAID_TOKEN"
                    ),
                    (403, _) | (_, "forbidden") => {
                        write!(f, "the hub does not allow this: {detail}")
                    }
                    (404, _) | (_, "not_found") => write!(f, "not found on the hub: {detail}"),
                    (413, _) | (_, "too_large") => write!(f, "too large for the hub: {detail}"),
                    (429, _) | (_, "rate_limited") => {
                        write!(f, "the hub is limiting requests: {detail}")?;
                        if let Some(seconds) = body.retry_after {
                            write!(f, " (try again in {seconds} seconds)")?;
                        }
                        Ok(())
                    }
                    (status, code) => {
                        write!(f, "the hub answered {status} {}: {detail}", shown(code))
                    }
                }
            }
            HubError::Transport { hub, detail } => {
                write!(f, "cannot reach the hub at {hub}: {}", shown(detail))
            }
            HubError::Protocol(what) => {
                write!(f, "the hub's answer is not valid: {}", shown(what))
            }
            HubError::Io(err) => write!(
                f,
                "the transfer with the hub failed: {}",
                shown(&err.to_string())
            ),
        }
    }
}

impl std::error::Error for HubError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HubError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for HubError {
    fn from(err: io::Error) -> Self {
        HubError::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_say_what_to_do() {
        let unauthorized = HubError::api(401, "unauthorized", "the token expired");
        let text = unauthorized.to_string();
        assert!(text.contains("the token expired"), "{text}");
        assert!(text.contains("braid login"), "{text}");
        let mut limited = HubError::api(429, "rate_limited", "slow down");
        if let HubError::Api { body, .. } = &mut limited {
            body.retry_after = Some(30);
        }
        assert!(limited.to_string().ends_with("(try again in 30 seconds)"));
        let other = HubError::api(409, "exists", "kikocis/demo exists");
        assert_eq!(
            other.to_string(),
            "the hub answered 409 exists: kikocis/demo exists"
        );
        assert_eq!(other.code(), Some("exists"));
        assert!(other.missing_ids().is_empty());
    }
}
