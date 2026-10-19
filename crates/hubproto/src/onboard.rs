// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Syncing an account's GitHub repositories to the hub (section 23), and
//! granting the hub the GitHub access it needs for that (section 2).
//!
//! An account grants access to its private and public repositories
//! ([`SCOPE_PRIVATE`]) or to its public ones only ([`SCOPE_PUBLIC`]), in a
//! browser or with a device flow run through the hub. The hub then lists
//! the repositories the account owns on GitHub, each with what it would
//! become here ([`GithubRepos`]), and imports those asked for in a batch of
//! jobs ([`SyncRequest`], [`SyncStarted`], [`SyncBatch`]), each kept as a
//! mirror that follows GitHub. Times are RFC 3339, in UTC.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The grant of access to the account's private and public repositories:
/// GitHub's `repo` scope.
pub const SCOPE_PRIVATE: &str = "private";

/// The grant of access to the account's public repositories only:
/// GitHub's `public_repo` scope.
pub const SCOPE_PUBLIC: &str = "public";

/// A mirror that takes in what is pushed to GitHub by fast-forwarding, or
/// by merging when both sides moved: the default of a sync.
pub const FOLLOW_MERGE: &str = "merge";

/// A mirror that takes in what is pushed to GitHub only by fast-forwarding.
pub const FOLLOW_FF_ONLY: &str = "ff-only";

/// A mirror the hub only publishes to.
pub const FOLLOW_OFF: &str = "off";

/// Why a repository is not imported: the account has no room left for it,
/// in repositories or in bytes.
pub const REASON_QUOTA: &str = "quota";

/// Why a repository is not imported: its name is taken here. A retry with
/// another name imports it.
pub const REASON_NAME_TAKEN: &str = "name taken";

/// Why a repository is not imported: its name cannot name a repository
/// here. A retry with another name imports it.
pub const REASON_NAME_NOT_ALLOWED: &str = "name not allowed here";

/// Why a repository is not imported: a repository here is its mirror
/// already.
pub const REASON_SYNCED: &str = "already synced";

/// Why a repository is not imported: it is not one the account owns on
/// GitHub, or not one the access it granted shows.
pub const REASON_NOT_FOUND: &str = "not found";

/// A batch whose imports are still under way.
pub const BATCH_RUNNING: &str = "running";

/// A batch whose imports all ended.
pub const BATCH_DONE: &str = "done";

/// A batch whose imports not started yet were cancelled, and whose others
/// ended.
pub const BATCH_CANCELLED: &str = "cancelled";

fn yes() -> bool {
    true
}

fn merge() -> String {
    FOLLOW_MERGE.to_owned()
}

/// `POST /v1/github/access/device`: the access to grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantStart {
    /// [`SCOPE_PRIVATE`] or [`SCOPE_PUBLIC`].
    pub scope: String,
}

/// `POST /v1/github/access/device/poll`: the flow to ask about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantPollRequest {
    /// What `POST /v1/github/access/device` answered in `poll`.
    pub poll: String,
}

/// The answer of `POST /v1/github/access/device/poll`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GrantPoll {
    /// The person has not approved yet.
    Pending,
    /// Poll less often.
    SlowDown {
        /// The new interval, in seconds.
        interval: u64,
    },
    /// The person refused.
    Denied,
    /// The code expired, or the flow is not known.
    Expired,
    /// Granted.
    Ok {
        /// The account.
        login: String,
        /// The scopes GitHub granted, as it writes them: `repo` or
        /// `public_repo`.
        scope: String,
    },
}

/// What the account has room for, as its quotas count it: what it owns
/// and what its jobs are making already.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    /// Repositories it may still own.
    pub repos_left: u64,
    /// Bytes its repositories may still store.
    pub bytes_left: u64,
}

/// A repository the account owns on GitHub, as `GET /v1/github/repos`
/// lists it: what GitHub says of it, and what it would become here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubRepo {
    /// GitHub's id of it, which stays when it is renamed.
    pub github_id: u64,
    /// `owner/name` on GitHub.
    pub full_name: String,
    /// Whether it is private on GitHub.
    pub private: bool,
    /// Whether it is a fork on GitHub.
    pub fork: bool,
    /// Whether it is archived on GitHub: such a repository is imported
    /// without a mirror.
    pub archived: bool,
    /// Whether it holds nothing yet: such a repository is made empty, with
    /// its mirror set.
    pub empty: bool,
    /// Its size, as GitHub gives it, in KiB.
    pub size_kb: u64,
    /// Its default branch on GitHub.
    #[serde(default)]
    pub default_branch: Option<String>,
    /// When something was last pushed to it.
    #[serde(default)]
    pub pushed_at: Option<String>,
    /// Whether the account administers it, as a webhook needs.
    pub admin: bool,
    /// The name it takes here.
    pub hub_name: String,
    /// The account's repository here that is its mirror already, as
    /// `owner/name`, when there is one.
    #[serde(default)]
    pub synced: Option<String>,
    /// Whether it may be imported.
    pub eligible: bool,
    /// Why it may not be imported, or what its import does differently.
    #[serde(default)]
    pub reason: Option<String>,
    /// About how many bytes of the account's room it takes here: four
    /// times its size on GitHub.
    pub estimate_bytes: u64,
}

/// `GET /v1/github/repos`: the repositories the account owns on GitHub,
/// most recently pushed first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubRepos {
    /// The access the account granted: [`SCOPE_PRIVATE`] or
    /// [`SCOPE_PUBLIC`]. With the second, private repositories are not
    /// listed.
    pub scope: String,
    /// The repositories.
    pub repos: Vec<GithubRepo>,
    /// Whether GitHub had more than the hub lists.
    pub truncated: bool,
    /// What the account has room for.
    pub quota: Room,
}

/// A repository named in a [`SyncRequest`]: GitHub's id of it, or its
/// `owner/name` on GitHub.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RepoPick {
    /// GitHub's id.
    Id(u64),
    /// `owner/name` on GitHub.
    Name(String),
}

/// `POST /v1/github/sync`: what to import, and how.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRequest {
    /// Every repository that may be imported, most recently pushed first,
    /// as many as the account has room for.
    #[serde(default)]
    pub all: bool,
    /// The repositories to import, when not `all`.
    #[serde(default)]
    pub repos: Vec<RepoPick>,
    /// The names some take here, other than their own: by GitHub's id, in
    /// decimal, or by `owner/name` on GitHub.
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    /// Whether public repositories stay public here; `false` imports them
    /// all as private. Private ones stay private either way.
    #[serde(default = "yes")]
    pub keep_public: bool,
    /// Whether text and data mining of the imported repositories is
    /// reserved; otherwise they take the account's default.
    #[serde(default)]
    pub reserve_tdm: bool,
    /// How each mirror takes in what is pushed to GitHub:
    /// [`FOLLOW_MERGE`], [`FOLLOW_FF_ONLY`] or [`FOLLOW_OFF`].
    #[serde(default = "merge")]
    pub follow: String,
}

impl Default for SyncRequest {
    fn default() -> Self {
        SyncRequest {
            all: false,
            repos: Vec::new(),
            names: BTreeMap::new(),
            keep_public: true,
            reserve_tdm: false,
            follow: merge(),
        }
    }
}

/// A repository a batch queued for import.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queued {
    /// GitHub's id of it.
    pub github_id: u64,
    /// `owner/name` on GitHub.
    pub full_name: String,
    /// The name it takes here.
    pub hub_name: String,
    /// The job that imports it.
    pub job: String,
}

/// A repository a batch did not queue, and why.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    /// GitHub's id of it, 0 when it named none the account owns.
    pub github_id: u64,
    /// `owner/name` on GitHub, as listed or as asked for.
    pub full_name: String,
    /// Why: [`REASON_QUOTA`], [`REASON_NAME_TAKEN`],
    /// [`REASON_NAME_NOT_ALLOWED`], [`REASON_SYNCED`] or
    /// [`REASON_NOT_FOUND`].
    pub reason: String,
}

/// The answer of `POST /v1/github/sync` (202) and of a retry: the batch,
/// what it queued and what it did not.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStarted {
    /// The batch's id: `b_` and 26 characters of `a-z 2-7`.
    pub batch: String,
    /// The repositories queued, each with its job.
    pub queued: Vec<Queued>,
    /// The repositories not queued, each with why.
    pub skipped: Vec<Skipped>,
}

/// How many repositories of a batch are in each state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchCounts {
    /// Waiting for their turn.
    pub queued: u64,
    /// Being imported.
    pub running: u64,
    /// Waiting for GitHub to let the hub go on.
    pub waiting: u64,
    /// Imported.
    pub done: u64,
    /// Not imported: `error` says why.
    pub failed: u64,
    /// Cancelled before their import started.
    pub cancelled: u64,
    /// Not queued: `error` says why.
    pub skipped: u64,
}

/// One repository of a batch, and how its import stands.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchItem {
    /// GitHub's id of it.
    pub github_id: u64,
    /// `owner/name` on GitHub.
    pub full_name: String,
    /// The name it takes here.
    pub hub_name: String,
    /// `queued`, `running`, `waiting`, `done`, `failed`, `cancelled` or
    /// `skipped`.
    pub state: String,
    /// Why it failed, or was skipped.
    #[serde(default)]
    pub error: Option<String>,
    /// The job that imports it, the last one when it was retried.
    #[serde(default)]
    pub job: Option<String>,
}

/// `GET /v1/github/sync`: a batch, and how each of its repositories
/// stands.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncBatch {
    /// The batch's id.
    pub batch: String,
    /// [`BATCH_RUNNING`], [`BATCH_DONE`] or [`BATCH_CANCELLED`].
    pub state: String,
    /// How many of its repositories are in each state.
    pub counts: BatchCounts,
    /// Its repositories, in the order it took them.
    pub repos: Vec<BatchItem>,
}

impl SyncBatch {
    /// Whether every repository of it ended, and none failed or was
    /// skipped.
    pub fn all_done(&self) -> bool {
        self.state != BATCH_RUNNING
            && self.counts.failed == 0
            && self.counts.skipped == 0
            && self.counts.cancelled == 0
    }
}

/// `POST /v1/github/sync/{batch}/retry`: a repository of the batch that
/// failed or was skipped, to import again, under another name if given.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryRequest {
    /// GitHub's id of it.
    pub github_id: u64,
    /// The name it takes here this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_requests_take_their_defaults() {
        let empty: SyncRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, SyncRequest::default());
        assert!(empty.keep_public && !empty.reserve_tdm && !empty.all);
        assert_eq!(empty.follow, FOLLOW_MERGE);
        let picked: SyncRequest = serde_json::from_str(
            r#"{"repos": [7001, "ada/demo"], "names": {"7001": "other"}, "keep_public": false}"#,
        )
        .unwrap();
        assert_eq!(
            picked.repos,
            [RepoPick::Id(7001), RepoPick::Name("ada/demo".into())]
        );
        assert_eq!(picked.names["7001"], "other");
        assert!(!picked.keep_public);
    }

    #[test]
    fn grants_and_batches_on_the_wire() {
        let ok = GrantPoll::Ok {
            login: "ada".into(),
            scope: "repo".into(),
        };
        let text = serde_json::to_string(&ok).unwrap();
        assert_eq!(text, r#"{"status":"ok","login":"ada","scope":"repo"}"#);
        assert_eq!(serde_json::from_str::<GrantPoll>(&text).unwrap(), ok);
        let slow: GrantPoll =
            serde_json::from_str(r#"{"status":"slow_down","interval":10}"#).unwrap();
        assert_eq!(slow, GrantPoll::SlowDown { interval: 10 });
        let batch = SyncBatch {
            batch: "b_aaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            state: BATCH_DONE.into(),
            counts: BatchCounts {
                done: 2,
                ..BatchCounts::default()
            },
            repos: vec![BatchItem {
                github_id: 7,
                full_name: "ada/demo".into(),
                hub_name: "demo".into(),
                state: "done".into(),
                error: None,
                job: Some("j_aaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            }],
        };
        let text = serde_json::to_string(&batch).unwrap();
        assert_eq!(serde_json::from_str::<SyncBatch>(&text).unwrap(), batch);
        assert!(batch.all_done());
        let skipped = SyncBatch {
            counts: BatchCounts {
                skipped: 1,
                ..batch.counts
            },
            ..batch.clone()
        };
        assert!(!skipped.all_done());
        let running = SyncBatch {
            state: BATCH_RUNNING.into(),
            ..batch
        };
        assert!(!running.all_done());
        // A short listing, from a hub that says less, still reads.
        let repo: GithubRepo = serde_json::from_str(
            r#"{"github_id": 1, "full_name": "ada/x", "private": false, "fork": false,
                "archived": false, "empty": false, "size_kb": 3, "admin": true,
                "hub_name": "x", "eligible": true, "estimate_bytes": 12288}"#,
        )
        .unwrap();
        assert_eq!((repo.synced, repo.reason), (None, None));
    }
}
