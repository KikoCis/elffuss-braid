// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Change requests: reviews, comments and landing (section 19): the bodies
//! of the routes under `/v1/r/{owner}/{name}/requests`.
//!
//! A change request proposes the work of a branch, its head, for another
//! branch of the repository, its base: from the same repository, or from
//! another one of its fork network. What it proposes is pinned: its
//! snapshot is the changeset its head held when it was opened or last
//! updated, each update is a new revision, and landing names the snapshot
//! it lands, so that nothing lands that was not shown. Change requests and
//! issues share one numbering in their repository. Ids are 64 lowercase
//! hex digits; times are RFC 3339, in UTC; logins are lowercase.

use braid_format::ObjectId;
use serde::{Deserialize, Serialize};

use crate::api::{LandMode, LandResponse};
use crate::cards::RepoRef;
use crate::hex;
use crate::history::ChangesetSummary;
use crate::issues::Comment;

/// A change request that is open.
pub const OPEN: &str = "open";

/// A change request closed without landing.
pub const CLOSED: &str = "closed";

/// A change request whose work landed on its base.
pub const LANDED: &str = "landed";

/// A review that approves the snapshot it reviewed.
pub const APPROVE: &str = "approve";

/// A review that asks for changes before it lands.
pub const REQUEST_CHANGES: &str = "request_changes";

/// A review that only comments.
pub const COMMENT: &str = "comment";

/// The verdicts of a review.
pub const VERDICTS: [&str; 3] = [APPROVE, REQUEST_CHANGES, COMMENT];

/// Whether `verdict` is one a review may give.
pub fn is_verdict(verdict: &str) -> bool {
    VERDICTS.contains(&verdict)
}

/// A request that lands on its base as it stands: no conflict.
pub const CLEAN: &str = "clean";

/// A request whose work conflicts with its base as it stands.
pub const CONFLICTS: &str = "conflicts";

/// A request whose stack is no longer built on its base, which moved to a
/// history without the changeset the stack starts from: its author updates
/// it before it lands.
pub const NEEDS_UPDATE: &str = "needs_update";

/// A request whose mergeability is not worked out yet: ask again in a
/// moment, or `POST …/check`.
pub const UNKNOWN: &str = "unknown";

/// The side of a diff a line comment is on: the version before.
pub const OLD: &str = "old";

/// The side of a diff a line comment is on: the version after, the
/// default.
pub const NEW: &str = "new";

/// What a new request proposes: a branch, in the target repository or in
/// another repository of its fork network.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadSpec {
    /// The repository the branch is in, `owner/name`; the target itself
    /// when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The branch.
    #[serde(rename = "ref")]
    pub reference: String,
}

/// `POST …/requests`: a new change request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewRequest {
    /// Its title: 1 to 256 characters, one line.
    pub title: String,
    /// Its text, Markdown, at most 64 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// What it proposes.
    pub head: HeadSpec,
    /// The branch of the target it proposes it for; its default branch
    /// when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// For a head in a private repository other than the target: that its
    /// admin publishes the request's work to everyone who reads the target.
    #[serde(default, skip_serializing_if = "is_false")]
    pub expose: bool,
    /// Whether it opens as a draft, which does not land until it is marked
    /// ready.
    #[serde(default, skip_serializing_if = "is_false")]
    pub draft: bool,
    /// Accounts asked to review it: the target's writers only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<String>,
    /// The label of the agent that opens it, recorded with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The head of a change request, as its snapshot pins it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestHead {
    /// The repository it comes from: `null` for a caller who may not read
    /// that repository, or once it is gone.
    #[serde(default)]
    pub repo: Option<RepoRef>,
    /// The branch.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The changeset the branch held at the current revision: what the
    /// request proposes, and what landing names.
    #[serde(with = "hex::id")]
    pub snapshot: ObjectId,
    /// Whether the hub still holds what the snapshot brings, to show,
    /// fetch and land: false once a closed request's work was let go.
    #[serde(default)]
    pub available: bool,
}

/// The base of a change request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestBase {
    /// The branch.
    #[serde(rename = "ref")]
    pub reference: String,
    /// Its head when the current revision was made.
    #[serde(default, with = "hex::opt_id")]
    pub head_at_snapshot: Option<ObjectId>,
    /// The best common ancestor of that head and the snapshot.
    #[serde(default, with = "hex::opt_id")]
    pub merge_base: Option<ObjectId>,
}

/// Whether a change request lands cleanly on its base now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mergeable {
    /// `clean`, `conflicts`, `needs_update` or `unknown`.
    pub state: String,
    /// The base's head it was worked out against.
    #[serde(default, with = "hex::opt_id")]
    pub against: Option<ObjectId>,
    /// With `conflicts`: the conflicts of the first changeset that does
    /// not land, as `braid land --json` prints them.
    #[serde(default)]
    pub conflicts: Vec<serde_json::Value>,
}

impl Default for Mergeable {
    fn default() -> Self {
        Mergeable {
            state: UNKNOWN.to_owned(),
            against: None,
            conflicts: Vec::new(),
        }
    }
}

/// The approvals a change request has at its current revision.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approvals {
    /// How many writers approved it, besides its author.
    pub count: u64,
    /// How many it needs before it lands.
    pub required: u32,
    /// Who approved it.
    #[serde(default)]
    pub by: Vec<String>,
}

/// The last landing of a change request that did not land it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    /// `conflicts` or `refused`, as the landing answered.
    pub outcome: String,
    /// Who tried.
    pub by: String,
    /// The label of the agent that tried, if one said.
    #[serde(default)]
    pub agent: Option<String>,
    /// When.
    pub at: String,
    /// The base's head then.
    #[serde(default, with = "hex::opt_id")]
    pub head: Option<ObjectId>,
    /// With `conflicts`: what conflicted, as `braid land --json` prints
    /// it.
    #[serde(default)]
    pub conflicts: Vec<serde_json::Value>,
}

/// How a change request landed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Landing {
    /// The base's head once it landed.
    #[serde(with = "hex::id")]
    pub head: ObjectId,
    /// The op log operation that moved the base, `None` when the base had
    /// the work already.
    #[serde(default)]
    pub operation: Option<u64>,
    /// Who landed it.
    pub by: String,
    /// The label of the agent that landed it, if one said.
    #[serde(default)]
    pub agent: Option<String>,
    /// When.
    pub at: String,
}

/// A change request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Its number in its repository.
    pub number: u64,
    /// Its title.
    pub title: String,
    /// Its text, Markdown.
    #[serde(default)]
    pub body: String,
    /// `open`, `closed` or `landed`.
    pub state: String,
    /// Whether it is a draft, which does not land.
    #[serde(default)]
    pub draft: bool,
    /// Who opened it.
    pub author: String,
    /// The label of the agent that opened it, if one said.
    #[serde(default)]
    pub agent: Option<String>,
    /// What it proposes.
    pub head: RequestHead,
    /// Where it lands.
    pub base: RequestBase,
    /// Its revision: 1 when it opens, one more at each update.
    pub revision: u64,
    /// The changesets it lands, oldest first: the snapshot's first-parent
    /// chain down to an ancestor of the base. Given by the routes that
    /// answer one request.
    #[serde(default)]
    pub stack: Vec<ChangesetSummary>,
    /// What the stack's merges bring in besides, parents before children.
    #[serde(default)]
    pub brings: Vec<ChangesetSummary>,
    /// Whether it lands cleanly now.
    #[serde(default)]
    pub mergeable: Mergeable,
    /// Its approvals.
    #[serde(default)]
    pub approvals: Approvals,
    /// The accounts asked to review it.
    #[serde(default)]
    pub review_requests: Vec<String>,
    /// The last landing that did not land it, while it is open.
    #[serde(default)]
    pub last_attempt: Option<Attempt>,
    /// How it landed, once it did.
    #[serde(default)]
    pub landed: Option<Landing>,
    /// The issues of the repository its text closes (`closes #4`): those
    /// that landing it on the default branch closes, and once it landed,
    /// those it closed.
    #[serde(default)]
    pub closes: Vec<u64>,
    /// How many comments its conversation has.
    #[serde(default)]
    pub comments: u64,
    /// Whether only the repository's writers may comment on it.
    #[serde(default)]
    pub locked: bool,
    /// When it was opened.
    pub created: String,
    /// When it last changed.
    pub updated: String,
    /// When it was closed or landed, while it is.
    #[serde(default)]
    pub closed: Option<String>,
    /// Who closed or landed it.
    #[serde(default)]
    pub closed_by: Option<String>,
    /// Its text as HTML, as the caller's pages show it: given by the
    /// routes that answer one request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
}

/// A page of change requests, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requests {
    /// The requests, without their stacks.
    pub requests: Vec<Request>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// `PATCH …/requests/{n}`: the fields to change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestPatch {
    /// A new title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A new text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// `open` or `closed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Whether it is a draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
}

/// `POST …/requests/{n}/update`: a new revision, from the head's branch as
/// it is now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUpdate {
    /// The changeset the new revision proposes, which the head's branch
    /// must hold on its first-parent chain; its head when left out.
    #[serde(default, with = "hex::opt_id", skip_serializing_if = "Option::is_none")]
    pub head: Option<ObjectId>,
}

/// `POST …/requests/{n}/comments`: a comment on the conversation or, with
/// `path` and `line`, on a line of a revision's diff.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewRequestComment {
    /// Its text, Markdown: 1 byte to 64 KiB.
    pub body: String,
    /// The file it is about, as the diff names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The line it is about, counted from 1 in the version `side` names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// `old` or `new`, the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    /// The revision whose diff it is about; the current one when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// The label of the agent that writes it, recorded with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// A comment of a change request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestComment {
    /// The comment.
    #[serde(flatten)]
    pub comment: Comment,
    /// For a line comment: the file.
    #[serde(default)]
    pub path: Option<String>,
    /// For a line comment: the line.
    #[serde(default)]
    pub line: Option<u64>,
    /// For a line comment: `old` or `new`.
    #[serde(default)]
    pub side: Option<String>,
    /// For a line comment: the revision whose diff it is about.
    #[serde(default)]
    pub revision: Option<u64>,
    /// Whether it is about a revision before the current one.
    #[serde(default)]
    pub outdated: bool,
    /// The review it belongs to, if any.
    #[serde(default)]
    pub review: Option<u64>,
}

/// A page of the comments of a change request's conversation, oldest
/// first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestComments {
    /// The comments.
    pub comments: Vec<RequestComment>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// A comment on a line, as a review carries it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineComment {
    /// The file, as the diff names it.
    pub path: String,
    /// The line, counted from 1 in the version `side` names.
    pub line: u64,
    /// `old` or `new`, the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    /// Its text, Markdown: 1 byte to 64 KiB.
    pub body: String,
}

/// `POST …/requests/{n}/reviews`: a review of the current revision.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewReview {
    /// `approve`, `request_changes` or `comment`.
    pub verdict: String,
    /// Its text, Markdown, at most 64 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Comments on lines of the revision's diff.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<LineComment>,
    /// The revision reviewed, which must still be the current one: what
    /// was read is what is approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// The label of the agent that writes it, recorded with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// A review.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// Its id in its repository.
    pub id: u64,
    /// Who wrote it.
    pub author: String,
    /// The label of the agent that wrote it, if one said.
    #[serde(default)]
    pub agent: Option<String>,
    /// `approve`, `request_changes` or `comment`.
    pub verdict: String,
    /// Whether its verdict counts: an approval or a request for changes
    /// from a writer of the repository other than the request's author.
    #[serde(default)]
    pub binding: bool,
    /// Whether it reviewed a revision before the current one: then it no
    /// longer counts.
    #[serde(default)]
    pub stale: bool,
    /// The revision it reviewed.
    pub revision: u64,
    /// Its text, Markdown; empty when it is hidden from the caller.
    #[serde(default)]
    pub body: String,
    /// When it was written.
    pub created: String,
    /// Its comments on lines.
    #[serde(default)]
    pub comments: Vec<RequestComment>,
}

/// A page of the reviews of a change request, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reviews {
    /// The reviews.
    pub reviews: Vec<Review>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// `PUT …/requests/{n}/reviewers`, and its answer: the accounts asked to
/// review a change request, all of them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reviewers {
    /// Their logins.
    pub logins: Vec<String>,
}

/// `POST …/requests/{n}/land`: lands the snapshot named.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestLand {
    /// The snapshot to land, which must still be the request's: what was
    /// reviewed is what lands.
    #[serde(with = "hex::id")]
    pub snapshot: ObjectId,
    /// How: fast-forward or rebase onto the base (the default), or
    /// fast-forward only.
    #[serde(default)]
    pub mode: LandMode,
    /// The label of the agent that lands it, for the op log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// The answer of `POST …/requests/{n}/land`: the request as it is now, and
/// what the landing did, as `POST …/land` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestLanded {
    /// The request: `landed`, or still open with its `last_attempt`.
    pub request: Request,
    /// The landing.
    pub land: LandResponse,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> ObjectId {
        ObjectId::from_bytes([byte; 32])
    }

    #[test]
    fn new_requests_as_clients_send_them() {
        let new: NewRequest =
            serde_json::from_str(r#"{"title":"x","head":{"ref":"feature"}}"#).unwrap();
        assert_eq!(new.head.repo, None);
        assert_eq!(new.head.reference, "feature");
        assert!(!new.expose && !new.draft && new.reviewers.is_empty());
        assert_eq!(
            serde_json::to_string(&new).unwrap(),
            r#"{"title":"x","head":{"ref":"feature"}}"#
        );
        let other: NewRequest = serde_json::from_str(
            r#"{"title":"x","head":{"repo":"bob/demo","ref":"f"},"base":"main","expose":true}"#,
        )
        .unwrap();
        assert_eq!(other.head.repo.as_deref(), Some("bob/demo"));
        assert!(other.expose);
        let update: RequestUpdate = serde_json::from_str("{}").unwrap();
        assert_eq!(update.head, None);
        assert_eq!(serde_json::to_string(&update).unwrap(), "{}");
        let land: RequestLand =
            serde_json::from_str(&format!(r#"{{"snapshot":"{}"}}"#, id(1).to_hex())).unwrap();
        assert_eq!((land.snapshot, land.mode), (id(1), LandMode::Rebase));
        assert!(serde_json::from_str::<RequestLand>(r#"{"snapshot":"x"}"#).is_err());
    }

    #[test]
    fn requests_on_the_wire() {
        let request = Request {
            number: 2,
            title: "Faster".into(),
            body: String::new(),
            state: OPEN.into(),
            draft: false,
            author: "bob".into(),
            agent: None,
            head: RequestHead {
                repo: None,
                reference: "feature".into(),
                snapshot: id(2),
                available: true,
            },
            base: RequestBase {
                reference: "main".into(),
                head_at_snapshot: Some(id(3)),
                merge_base: Some(id(3)),
            },
            revision: 1,
            stack: Vec::new(),
            brings: Vec::new(),
            mergeable: Mergeable::default(),
            approvals: Approvals::default(),
            review_requests: Vec::new(),
            last_attempt: None,
            landed: None,
            closes: vec![1],
            comments: 0,
            locked: false,
            created: "2026-10-18T12:00:00Z".into(),
            updated: "2026-10-18T12:00:00Z".into(),
            closed: None,
            closed_by: None,
            html: None,
        };
        let text = serde_json::to_string(&request).unwrap();
        // An unreadable head names no repository, and what is unknown is
        // null, never missing: but the HTML.
        assert!(text.contains(r#""repo":null"#), "{text}");
        assert!(text.contains(r#""ref":"feature""#), "{text}");
        assert!(text.contains(r#""state":"unknown""#), "{text}");
        assert!(text.contains(r#""landed":null"#), "{text}");
        assert!(!text.contains("html"), "{text}");
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), request);
    }

    #[test]
    fn comments_and_reviews_on_the_wire() {
        let comment = RequestComment {
            comment: Comment {
                id: 7,
                author: "ada".into(),
                body: "here".into(),
                created: "t".into(),
                ..Comment::default()
            },
            path: Some("src/lib.rs".into()),
            line: Some(3),
            side: Some(NEW.into()),
            revision: Some(1),
            outdated: true,
            review: None,
        };
        let value = serde_json::to_value(&comment).unwrap();
        // The comment's fields sit beside those of its line.
        assert_eq!(value["id"], 7);
        assert_eq!(value["path"], "src/lib.rs");
        assert_eq!(value["outdated"], true);
        assert_eq!(
            serde_json::from_value::<RequestComment>(value).unwrap(),
            comment
        );
        let review: NewReview =
            serde_json::from_str(r#"{"verdict":"approve","revision":2}"#).unwrap();
        assert_eq!(
            (review.verdict.as_str(), review.revision),
            (APPROVE, Some(2))
        );
        assert!(review.comments.is_empty());
        for verdict in VERDICTS {
            assert!(is_verdict(verdict));
        }
        assert!(!is_verdict("lgtm"));
    }
}
