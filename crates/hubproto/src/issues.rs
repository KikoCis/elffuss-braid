// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Issues, comments, labels and assignees (section 18): the bodies of the
//! routes under `/v1/r/{owner}/{name}/issues`, `…/comments`, `…/labels` and
//! `…/threads`, and of `GET /v1/me/issues`.
//!
//! Issues and change requests share one numbering in their repository: a
//! number names one thread, whichever kind it is. Times are RFC 3339, in
//! UTC; logins are lowercase.

use serde::{Deserialize, Serialize};

use crate::cards::RepoRef;

/// An issue, or a change request, that is open.
pub const OPEN: &str = "open";

/// An issue that is closed.
pub const CLOSED: &str = "closed";

/// What a closed issue was closed as: done.
pub const COMPLETED: &str = "completed";

/// What a closed issue was closed as: not to be done.
pub const NOT_PLANNED: &str = "not_planned";

/// The reasons a writer hides a comment for.
pub const HIDE_REASONS: [&str; 4] = ["spam", "off_topic", "outdated", "resolved"];

/// How many colours labels choose from: `0` to `11`.
pub const COLORS: u8 = 12;

/// The longest name of a label, in characters.
pub const MAX_LABEL: usize = 50;

/// The longest description of a label, in characters.
pub const MAX_LABEL_DESCRIPTION: usize = 200;

/// What a timeline's entries say happened, besides a comment: the title
/// or the text changed, the state, a label, an assignee or the lock, or
/// another thread named this one (`#4` in its repository, `owner/name#4`
/// in another).
pub const EVENTS: [&str; 12] = [
    "renamed",
    "edited",
    "closed",
    "reopened",
    "labeled",
    "unlabeled",
    "assigned",
    "unassigned",
    "locked",
    "unlocked",
    "referenced",
    "cross_referenced",
];

/// Whether `reason` is one a comment may be hidden for.
pub fn is_hide_reason(reason: &str) -> bool {
    HIDE_REASONS.contains(&reason)
}

/// Whether `reason` is one an issue may be closed as.
pub fn is_close_reason(reason: &str) -> bool {
    reason == COMPLETED || reason == NOT_PLANNED
}

/// An issue.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// Its number in its repository.
    pub number: u64,
    /// Its title.
    pub title: String,
    /// Its text, Markdown.
    #[serde(default)]
    pub body: String,
    /// `open` or `closed`.
    pub state: String,
    /// For a closed issue, what it was closed as: `completed` or
    /// `not_planned`.
    #[serde(default)]
    pub state_reason: Option<String>,
    /// Who opened it.
    pub author: String,
    /// The label of the agent that opened it, if one said.
    #[serde(default)]
    pub agent: Option<String>,
    /// Its labels, by name.
    #[serde(default)]
    pub labels: Vec<String>,
    /// The accounts it is assigned to.
    #[serde(default)]
    pub assignees: Vec<String>,
    /// Whether only the repository's writers may comment on it.
    #[serde(default)]
    pub locked: bool,
    /// How many comments it has.
    #[serde(default)]
    pub comments: u64,
    /// When it was opened.
    pub created: String,
    /// When it last changed.
    pub updated: String,
    /// When it was closed, while it is.
    #[serde(default)]
    pub closed: Option<String>,
    /// Who closed it, unless a landing did.
    #[serde(default)]
    pub closed_by: Option<String>,
    /// The changeset whose landing closed it (`fixes #4` in its message).
    #[serde(default)]
    pub closed_by_changeset: Option<String>,
    /// Its text as HTML, as the caller's pages show it: given by the
    /// routes that answer one issue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
}

/// A page of issues.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issues {
    /// The issues.
    pub issues: Vec<Issue>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// `POST …/issues`: a new issue.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewIssue {
    /// Its title: 1 to 256 characters, one line.
    pub title: String,
    /// Its text, Markdown, at most 64 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Labels to give it, by name: the repository's writers only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Accounts to assign it to: the repository's writers only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    /// The label of the agent that opens it, recorded with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// `PATCH …/issues/{n}`: the fields to change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuePatch {
    /// A new title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A new text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// `open` or `closed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// With `closed`: `completed` (the default) or `not_planned`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_reason: Option<String>,
    /// Its labels, all of them, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    /// The accounts it is assigned to, all of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignees: Option<Vec<String>>,
    /// Whether only writers may comment on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
}

/// A comment on a thread.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    /// Its id in its repository.
    pub id: u64,
    /// Who wrote it.
    pub author: String,
    /// The label of the agent that wrote it, if one said.
    #[serde(default)]
    pub agent: Option<String>,
    /// Its text, Markdown; empty when it is hidden from the caller.
    #[serde(default)]
    pub body: String,
    /// When it was written.
    pub created: String,
    /// When it was last edited, if it was.
    #[serde(default)]
    pub edited: Option<String>,
    /// Why a writer hid it, if one did: `spam`, `off_topic`, `outdated`
    /// or `resolved`; to the repository's writers, `suspended` for a
    /// comment of a suspended account, which others do not see at all.
    #[serde(default)]
    pub hidden: Option<String>,
    /// Its text as HTML, as the caller's pages show it: given by the
    /// routes that answer one comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
}

/// A page of comments, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comments {
    /// The comments.
    pub comments: Vec<Comment>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// `POST …/issues/{n}/comments`: a new comment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewComment {
    /// Its text, Markdown: 1 byte to 64 KiB.
    pub body: String,
    /// The label of the agent that writes it, recorded with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// `PATCH …/comments/{id}`: a comment's new text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentPatch {
    /// The new text.
    pub body: String,
}

/// `POST …/comments/{id}/hide`: why a comment is hidden, or `""` to show
/// it again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HideComment {
    /// One of [`HIDE_REASONS`], or empty.
    pub reason: String,
}

/// A label of a repository.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    /// Its name: 1 to 50 characters, without `,`; compared without regard
    /// to case.
    pub name: String,
    /// Its colour in the palette, `0` to `11`.
    pub color: u8,
    /// What it means.
    #[serde(default)]
    pub description: String,
}

/// `GET …/labels`: a repository's labels, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Labels {
    /// The labels.
    pub labels: Vec<Label>,
}

/// `PUT …/labels/{label}`: a label made, or changed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelPut {
    /// Its colour, `0` to `11`: kept when it is left out, `0` for a new
    /// label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<u8>,
    /// What it means: kept when it is left out, empty for a new label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Where a reference to a thread was made.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSource {
    /// The repository that made it, when it is another one than the
    /// thread's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoRef>,
    /// The number of the thread that made it.
    pub number: u64,
}

/// An entry of a thread's timeline: a comment, or something that happened.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineItem {
    /// `comment`, or one of [`EVENTS`].
    pub kind: String,
    /// Who did it; empty for what the hub did by itself, such as locking a
    /// thread that holds the most comments one may.
    pub actor: String,
    /// When.
    pub created: String,
    /// The comment, for a `comment`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<Comment>,
    /// The label, for `labeled` and `unlabeled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The account, for `assigned` and `unassigned`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Why: what `closed` closed it as, or why the hub `locked` it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The title before, for `renamed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The title after, for `renamed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The thread that referenced this one, for `referenced` and
    /// `cross_referenced`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ThreadSource>,
    /// The changeset whose landing closed it, for `closed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changeset: Option<String>,
}

/// A page of a thread's timeline, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timeline {
    /// The entries.
    pub items: Vec<TimelineItem>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// `GET …/threads/{n}`: what a number names.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadInfo {
    /// `issue` or `request`.
    pub kind: String,
    /// Its number.
    pub number: u64,
    /// Its title.
    pub title: String,
    /// `open` or `closed`, and for a change request `landed`.
    pub state: String,
}

/// An issue of some repository, in `GET /v1/me/issues`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoIssue {
    /// Its repository.
    pub repo: RepoRef,
    /// The issue.
    #[serde(flatten)]
    pub issue: Issue,
}

/// `GET /v1/me/issues`: a page of the issues the caller is assigned to,
/// opened or is mentioned in, latest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoIssues {
    /// The issues.
    pub issues: Vec<RepoIssue>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_on_the_wire() {
        let issue = Issue {
            number: 4,
            title: "Crash on start".into(),
            state: OPEN.into(),
            author: "ada".into(),
            labels: vec!["bug".into()],
            created: "2026-10-18T12:00:00Z".into(),
            updated: "2026-10-18T12:00:00Z".into(),
            ..Issue::default()
        };
        let text = serde_json::to_string(&issue).unwrap();
        // What is unknown is null, never missing: but the HTML, which only
        // the routes of one issue give.
        assert!(text.contains(r#""state_reason":null"#), "{text}");
        assert!(text.contains(r#""closed_by_changeset":null"#), "{text}");
        assert!(!text.contains("html"), "{text}");
        assert_eq!(serde_json::from_str::<Issue>(&text).unwrap(), issue);
        // In a list of the caller's issues, with its repository beside it.
        let mine = RepoIssue {
            repo: RepoRef {
                owner: "ada".into(),
                name: "demo".into(),
            },
            issue: issue.clone(),
        };
        let value = serde_json::to_value(&mine).unwrap();
        assert_eq!(value["repo"]["name"], "demo");
        assert_eq!(value["number"], 4);
        assert_eq!(serde_json::from_value::<RepoIssue>(value).unwrap(), mine);
    }

    #[test]
    fn bodies_as_clients_send_them() {
        let new: NewIssue = serde_json::from_str(r#"{"title":"x"}"#).unwrap();
        assert_eq!(
            (new.title.as_str(), new.body.as_deref(), new.labels.len()),
            ("x", None, 0)
        );
        assert_eq!(serde_json::to_string(&new).unwrap(), r#"{"title":"x"}"#);
        let patch: IssuePatch = serde_json::from_str(r#"{"state":"closed","labels":[]}"#).unwrap();
        assert_eq!(patch.state.as_deref(), Some(CLOSED));
        assert_eq!(patch.labels, Some(Vec::new()));
        assert_eq!(patch.assignees, None);
        let put: LabelPut = serde_json::from_str(r#"{"color":3}"#).unwrap();
        assert_eq!((put.color, put.description), (Some(3), None));
        let item: TimelineItem =
            serde_json::from_str(r#"{"kind":"closed","actor":"ada","created":"t"}"#).unwrap();
        assert_eq!(item.reason, None);
        let text = serde_json::to_string(&item).unwrap();
        assert_eq!(text, r#"{"kind":"closed","actor":"ada","created":"t"}"#);
    }

    #[test]
    fn reasons() {
        for reason in HIDE_REASONS {
            assert!(is_hide_reason(reason));
        }
        assert!(!is_hide_reason("suspended") && !is_hide_reason(""));
        assert!(is_close_reason(COMPLETED) && is_close_reason(NOT_PLANNED));
        assert!(!is_close_reason("done"));
    }
}
