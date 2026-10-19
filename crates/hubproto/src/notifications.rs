// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The inbox (section 21): what the hub tells an account about the threads
//! it takes part in or watches, and about its own requests to the hub;
//! marking notifications read or done; and following or muting one thread.
//! The inbox is all there is: the hub sends no e-mail.
//!
//! A notification names a repository, a thread by its kind and number, who
//! did what, and why the account was told. A thread's title is read when
//! the inbox is listed, and only from a repository the caller may read
//! then. Times are RFC 3339, in UTC; logins are lowercase.

use serde::{Deserialize, Serialize};

use crate::cards::RepoRef;

/// The account was mentioned (`@login`) in the thread.
pub const MENTION: &str = "mention";

/// The account was asked to review the change request.
pub const REVIEW_REQUESTED: &str = "review_requested";

/// The thread is assigned to the account.
pub const ASSIGNED: &str = "assigned";

/// The account reviewed the change request.
pub const REVIEW: &str = "review";

/// The account opened the thread.
pub const AUTHOR: &str = "author";

/// The account commented on the thread.
pub const COMMENT: &str = "comment";

/// The thread was closed or opened again.
pub const STATE: &str = "state";

/// The change request landed.
pub const LANDED: &str = "landed";

/// The account watches all of the repository, or follows the thread.
pub const SUBSCRIBED: &str = "subscribed";

/// A notice of the hub about the account's own requests, such as a fork
/// made or a grant to renew.
pub const SYSTEM: &str = "system";

/// Why an account is told about a thread, strongest first: a notification
/// gives the strongest of the reasons it has, until it is read.
pub const REASONS: [&str; 9] = [
    MENTION,
    REVIEW_REQUESTED,
    ASSIGNED,
    REVIEW,
    AUTHOR,
    COMMENT,
    STATE,
    LANDED,
    SUBSCRIBED,
];

/// A notification about an issue.
pub const ISSUE: &str = "issue";

/// A notification about a change request.
pub const REQUEST: &str = "request";

/// What a thread subscription says when the account chose to follow the
/// thread.
pub const MANUAL: &str = "manual";

/// What a thread subscription says when the account muted the thread.
pub const MUTED: &str = "muted";

/// What a thread subscription says when the account takes part in the
/// thread.
pub const PARTICIPATING: &str = "participating";

/// What a thread subscription says when the account watches all of the
/// repository.
pub const WATCHING: &str = "watching";

/// What a thread subscription says when the account ignores the
/// repository, which silences every thread of it.
pub const IGNORING: &str = "ignoring";

/// The thread a notification is about.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationThread {
    /// `issue` or `request`.
    pub kind: String,
    /// Its number in its repository.
    pub number: u64,
    /// Its title, as it is now.
    pub title: String,
}

/// A notification.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// Its id: what marking it read or done names.
    pub id: u64,
    /// The repository it is about, if any.
    #[serde(default)]
    pub repo: Option<RepoRef>,
    /// The thread it is about; `None` for a notice of the hub.
    #[serde(default)]
    pub thread: Option<NotificationThread>,
    /// Why the account was told: one of [`REASONS`], or [`SYSTEM`].
    pub reason: String,
    /// Who did what it reports last; `None` for the hub itself.
    #[serde(default)]
    pub actor: Option<String>,
    /// What happened last: in a thread, `opened`, `commented`, `closed`,
    /// `reopened`, `assigned`, `mentioned`, `review_requested`, `reviewed`
    /// or `landed`; in a notice of the hub, what it says, such as
    /// `fork_done`.
    #[serde(default)]
    pub code: Option<String>,
    /// When it last changed.
    pub updated: String,
    /// Whether it waits to be read.
    pub unread: bool,
    /// The page of the hub it leads to.
    pub url: String,
}

/// `GET /v1/notifications`: a page of the inbox, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notifications {
    /// The notifications.
    pub notifications: Vec<Notification>,
    /// How many of the caller's notifications wait to be read, of the
    /// repository asked for, if one was.
    pub unread: u64,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// `POST /v1/notifications/read`: which notifications to mark read. Either
/// `ids`, or any of `all`, `before` and `repo`, which mark every
/// notification that matches them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkRead {
    /// These notifications, by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<u64>,
    /// Every notification.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
    /// Those that last changed at or before this time: what a client
    /// showed, and nothing that came since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// Those of this repository, `owner/name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// The answer of `POST /v1/notifications/read`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marked {
    /// How many notifications it marked read that were unread.
    pub marked: u64,
}

/// `GET …/threads/{n}/subscription`, the answer of `PUT`, and its body,
/// where `reason` is left out: whether the caller is told about a thread.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSubscription {
    /// Whether the thread notifies the caller.
    pub subscribed: bool,
    /// Why: [`MANUAL`], [`MUTED`], [`PARTICIPATING`], [`WATCHING`] or
    /// [`IGNORING`]; absent when nothing ties the caller to the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_on_the_wire() {
        let page = Notifications {
            notifications: vec![
                Notification {
                    id: 7,
                    repo: Some(RepoRef {
                        owner: "ada".into(),
                        name: "demo".into(),
                    }),
                    thread: Some(NotificationThread {
                        kind: ISSUE.into(),
                        number: 4,
                        title: "A title".into(),
                    }),
                    reason: MENTION.into(),
                    actor: Some("bob".into()),
                    code: Some("commented".into()),
                    updated: "2026-10-18T12:00:00Z".into(),
                    unread: true,
                    url: "https://hub.example/ada/demo/issues/4".into(),
                },
                Notification {
                    id: 8,
                    reason: SYSTEM.into(),
                    code: Some("fork_failed".into()),
                    updated: "2026-10-18T12:00:00Z".into(),
                    url: "https://hub.example/notifications".into(),
                    ..Notification::default()
                },
            ],
            unread: 1,
            next: Some("n1.7".into()),
        };
        let text = serde_json::to_string(&page).unwrap();
        assert_eq!(serde_json::from_str::<Notifications>(&text).unwrap(), page);
        // A notice of the hub names no thread and no actor.
        let notice: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(notice["notifications"][1]["thread"].is_null());
        assert!(notice["notifications"][1]["actor"].is_null());
        assert_eq!(REASONS[0], MENTION);
        assert!(!REASONS.contains(&SYSTEM));
    }

    #[test]
    fn marking_and_subscriptions_on_the_wire() {
        let ids = MarkRead {
            ids: vec![1, 2],
            ..MarkRead::default()
        };
        assert_eq!(serde_json::to_string(&ids).unwrap(), r#"{"ids":[1,2]}"#);
        let all: MarkRead = serde_json::from_str(r#"{"all":true}"#).unwrap();
        assert!(all.all && all.ids.is_empty() && all.repo.is_none());
        assert_eq!(serde_json::to_string(&all).unwrap(), r#"{"all":true}"#);
        let body: ThreadSubscription = serde_json::from_str(r#"{"subscribed":false}"#).unwrap();
        assert_eq!(body.reason, None);
        let muted = ThreadSubscription {
            subscribed: false,
            reason: Some(MUTED.into()),
        };
        assert_eq!(
            serde_json::to_string(&muted).unwrap(),
            r#"{"subscribed":false,"reason":"muted"}"#
        );
    }
}
