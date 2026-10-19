// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The JSON bodies of the hub's routes, section by section of
//! `docs/hub.md`. Every field that holds a braid id is 64 hex digits on the
//! wire and an [`ObjectId`] here.

use std::collections::BTreeMap;

use braid_format::ObjectId;
use serde::{Deserialize, Serialize};

use crate::cards::RepoRef;
use crate::hex;

/// An error body: `{"error", "detail"}`, with `ids` for `missing`,
/// `retry_after` for `rate_limited`, and `head` for `not_on_branch`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// The error's code: `unauthorized` (401), `forbidden` (403),
    /// `not_found` (404), `method_not_allowed` (405), `exists` (409),
    /// `invalid` (400), `missing` (409), `not_on_branch` (409), `too_large`
    /// (413), `rate_limited` (429) or `internal` (500).
    pub error: String,
    /// What went wrong, for people.
    pub detail: String,
    /// With `missing`: the objects the hub needs.
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "hex::ids")]
    pub ids: Vec<ObjectId>,
    /// With `rate_limited`: seconds to wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
    /// With `not_on_branch`: where the ref points now, if it exists, as 64
    /// hex digits. Text rather than an [`ObjectId`], so that an error stays
    /// small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

/// `GET /v1`: the protocol version the hub speaks, and the sizes a client
/// keeps its requests within.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    /// The protocol version: 1.
    pub version: u32,
    /// What one request may carry or ask for.
    pub limits: RootLimits,
}

/// The sizes of section 12 a hub enforces, as `GET /v1` gives them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootLimits {
    /// The largest request body, and so the largest object stream.
    pub max_body: u64,
    /// The largest object of a `PUT …/o/{id}`.
    pub max_put: u64,
    /// The largest JSON request body.
    pub max_json: u64,
    /// The most ids of one list: `lacks`, `want`, `have`, a stack.
    pub max_ids: u64,
    /// The most path prefixes of a `missing` request.
    pub max_paths: u64,
    /// The entries of a `missing` page without a `limit`.
    pub missing_page: u64,
    /// The most entries of a `missing` page.
    pub missing_page_max: u64,
    /// The most objects one `missing` walk lists.
    pub missing_objects: u64,
    /// The most entries of a tree listing.
    pub tree_entries: u64,
    /// The deepest tree listing.
    pub tree_depth: u32,
    /// The most repositories, tokens or collaborators a list holds.
    pub max_list: u64,
    /// The sizes of issues, change requests, topics and browsing, from a
    /// hub that has them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub social: Option<SocialCaps>,
}

/// The sizes of the social features and of browsing a hub enforces, as
/// `GET /v1` gives them in `limits.social`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocialCaps {
    /// The longest title of an issue or a change request, in characters.
    pub title: u64,
    /// The largest body of an issue, a comment or a review, in bytes.
    pub text: u64,
    /// The most entries of one page of a list.
    pub page: u64,
    /// The most topics of a repository.
    pub topics: u64,
    /// The longest topic, in characters.
    pub topic: u64,
    /// The most labels of a repository.
    pub labels: u64,
    /// The most labels of one thread.
    pub thread_labels: u64,
    /// The most assignees of one thread.
    pub assignees: u64,
    /// The most comments of one thread: it locks itself at that many.
    pub comments_per_thread: u64,
    /// The most line comments of one review.
    pub review_comments: u64,
    /// The most mentions of one text that notify.
    pub mentions: u64,
    /// The most references one text links.
    pub references: u64,
    /// The most changesets of one change request.
    pub request_changesets: u64,
    /// The most bytes a change request from another repository brings.
    pub request_bytes: u64,
    /// The most entries of one page of a directory.
    pub dir_page: u64,
    /// The largest file shown as text, in bytes.
    pub text_view: u64,
}

// ---------------------------------------------------------------- section 2

/// `POST /v1/login/device` → the start of a device flow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStart {
    /// What to send back when polling.
    pub poll: String,
    /// The code the person types at `verification_uri`.
    pub user_code: String,
    /// Where the person goes.
    pub verification_uri: String,
    /// Seconds until the code expires.
    pub expires_in: u64,
    /// Seconds to wait between polls.
    pub interval: u64,
}

/// `POST /v1/login/device/poll` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePollRequest {
    /// From [`DeviceStart::poll`].
    pub poll: String,
}

/// `POST /v1/login/device/poll` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DevicePoll {
    /// The person has not approved yet.
    Pending,
    /// Poll less often.
    SlowDown {
        /// The new interval, in seconds.
        interval: u64,
    },
    /// The person refused.
    Denied,
    /// The code expired.
    Expired,
    /// Signed in.
    Ok {
        /// The new token, shown this once.
        token: String,
        /// The GitHub login.
        login: String,
        /// When the token expires.
        expires: String,
    },
}

// ---------------------------------------------------------------- section 3

/// A token's scope: rights on repositories.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// `owner/name`, `owner/*`, or `*`.
    pub repo: String,
    /// `read`, `write`, `land:<glob>`, `admin`.
    pub rights: Vec<String>,
}

/// What `GET /v1/me` says about the token that asked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenInfo {
    /// The token's public id.
    pub id: String,
    /// A note, for people.
    #[serde(default)]
    pub note: String,
    /// Its scopes.
    pub scopes: Vec<Scope>,
    /// When it was made.
    #[serde(default)]
    pub created: String,
    /// When it expires.
    pub expires: String,
    /// When it was last used, if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<String>,
}

/// `GET /v1/me`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    /// The GitHub login.
    pub login: String,
    /// The display name, if any.
    #[serde(default)]
    pub name: Option<String>,
    /// The avatar, if any.
    #[serde(default)]
    pub avatar_url: Option<String>,
    /// Whether the hub holds GitHub repository access for the account.
    #[serde(default)]
    pub github_repo_access: bool,
    /// The token that asked, when one did.
    #[serde(default)]
    pub token: Option<TokenInfo>,
    /// The account's settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<MeSettings>,
    /// The scopes of the GitHub access the account granted the hub, as
    /// GitHub wrote them, when it granted any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_scope: Option<String>,
}

/// The settings of an account, in `GET /v1/me`; `PATCH /v1/me` changes
/// them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeSettings {
    /// What text and data mining new public repositories start with:
    /// `allowed` or `reserved`.
    pub tdm_default: String,
    /// Whether the repositories the account starred show on its profile.
    pub stars_public: bool,
}

/// `POST /v1/tokens` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCreate {
    /// A note, for people.
    #[serde(default)]
    pub note: String,
    /// The scopes.
    pub scopes: Vec<Scope>,
    /// Days until it expires, at most 366.
    pub expires_in_days: u32,
}

/// `POST /v1/tokens` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCreated {
    /// The new token, shown this once.
    pub token: String,
    /// Its public id.
    pub id: String,
    /// Its scopes.
    pub scopes: Vec<Scope>,
    /// When it expires.
    pub expires: String,
}

/// `GET /v1/tokens`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenList {
    /// The account's tokens.
    pub tokens: Vec<TokenInfo>,
}

// ---------------------------------------------------------------- section 4

/// `POST /v1/repos` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCreate {
    /// The repository's name; the owner is the caller.
    pub name: String,
    /// Private unless `false`.
    #[serde(default = "yes")]
    pub private: bool,
    /// A description.
    #[serde(default)]
    pub description: String,
    /// A git URL to take the history from first.
    #[serde(default)]
    pub import: Option<String>,
    /// A git URL to publish the default branch to.
    #[serde(default)]
    pub mirror: Option<String>,
}

fn yes() -> bool {
    true
}

/// A mirror's state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorInfo {
    /// The git URL.
    pub url: String,
    /// When it was last synced, if ever.
    #[serde(default)]
    pub last_sync: Option<String>,
    /// The last sync's outcome code (`pushed`, `up_to_date`, `diverged`,
    /// …), if any.
    #[serde(default)]
    pub outcome: Option<String>,
    /// More about it, for people.
    #[serde(default)]
    pub detail: Option<String>,
    /// How the hub takes in what is pushed to the mirror: `off`, `ff-only`
    /// or `merge`. Absent is `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow: Option<String>,
    /// The GitHub repository's numeric id, which stays when it is renamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_id: Option<u64>,
    /// The git commit both sides held after the last publish, follow or
    /// import, as 40 hex digits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced: Option<String>,
    /// How the hub hears of pushes to the mirror, such as `webhook` or
    /// `polling`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook: Option<String>,
}

/// A repository, as `GET /v1/r/{owner}/{name}` and others return it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    /// The repository's id, which no other repository ever takes, not even
    /// one made later under the same name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// The owner's login, lowercase.
    pub owner: String,
    /// The name.
    pub name: String,
    /// Whether only those given access can read it.
    pub private: bool,
    /// A description.
    #[serde(default)]
    pub description: String,
    /// The branch the mirror publishes and the web shows.
    pub default_branch: String,
    /// When it was made.
    pub created: String,
    /// The mirror, if any.
    #[serde(default)]
    pub mirror: Option<MirrorInfo>,
    /// The caller's permission: `read`, `write` or `admin`.
    pub permission: String,
    /// Its topics: lowercase letters, digits and `-`.
    #[serde(default)]
    pub topics: Vec<String>,
    /// The repository it was forked from, when the caller may read that
    /// one.
    #[serde(default)]
    pub fork_of: Option<RepoRef>,
    /// Text and data mining of its content: `allowed` or `reserved`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tdm: String,
    /// When a landing last moved one of its refs, if one ever did.
    #[serde(default)]
    pub pushed: Option<String>,
    /// Its counts, where one repository is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<RepoCounts>,
    /// The caller's own state on it, for a signed-in caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer: Option<RepoViewer>,
    /// Its settings, for its admins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<RepoSettingsInfo>,
}

/// The counts of a repository, as the caller may see them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCounts {
    /// The accounts that starred it.
    pub stars: u64,
    /// Its forks the caller may see.
    pub forks: u64,
    /// The accounts that watch all of it.
    pub watchers: u64,
    /// Its open issues.
    pub open_issues: u64,
    /// Its open change requests.
    pub open_requests: u64,
}

/// What the caller itself does with a repository.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoViewer {
    /// Whether it starred it.
    pub starred: bool,
    /// How it watches it: `all`, `participating` or `ignore`.
    pub watching: String,
}

/// The settings of a repository, as its admins see them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSettingsInfo {
    /// Whether it takes issues.
    pub issues: bool,
    /// Who may open change requests: `readers`, `collaborators` or `off`.
    pub requests_from: String,
    /// Whether it may be forked.
    pub allow_forks: bool,
    /// The approvals a change request needs before it lands, 0 to 10.
    pub require_approvals: u32,
    /// How its mirror is followed (`off`, `ff-only` or `merge`), when it
    /// has a mirror.
    #[serde(default)]
    pub mirror_follow: Option<String>,
}

/// `GET /v1/repos`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoList {
    /// The repositories.
    pub repos: Vec<RepoInfo>,
}

/// `PATCH /v1/r/{owner}/{name}`: the fields to change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPatch {
    /// New visibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
    /// New description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// New mirror URL; an empty string removes the mirror.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
    /// New default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// New topics, replacing the old ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topics: Option<Vec<String>>,
    /// Text and data mining: `allowed` or `reserved`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tdm: Option<String>,
    /// Whether it takes issues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issues: Option<bool>,
    /// Who may open change requests: `readers`, `collaborators` or `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_from: Option<String>,
    /// Whether it may be forked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_forks: Option<bool>,
    /// The approvals a change request needs before it lands, 0 to 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_approvals: Option<u32>,
    /// How the mirror is followed: `off`, `ff-only` or `merge`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_follow: Option<String>,
}

/// `PUT …/collaborators/{login}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collaborator {
    /// `read`, `write` or `admin`.
    pub permission: String,
}

/// One entry of `GET …/collaborators`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaboratorEntry {
    /// The collaborator's login, lowercase.
    pub login: String,
    /// `read`, `write` or `admin`.
    pub permission: String,
}

/// `GET …/collaborators`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaboratorList {
    /// Every collaborator, by login.
    pub collaborators: Vec<CollaboratorEntry>,
}

// ---------------------------------------------------------------- section 6

/// `POST …/lacks` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LacksRequest {
    /// Ids to check, at most 10,000.
    #[serde(with = "hex::ids")]
    pub ids: Vec<ObjectId>,
}

/// `POST …/lacks` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LacksResponse {
    /// The ids the hub does not store.
    #[serde(with = "hex::ids")]
    pub lacks: Vec<ObjectId>,
}

/// `POST …/objects` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    /// How many records were stored.
    pub stored: u64,
}

/// `POST …/missing` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingRequest {
    /// Changesets wanted.
    #[serde(with = "hex::ids")]
    pub want: Vec<ObjectId>,
    /// Changesets the client holds with all they reach.
    #[serde(default, with = "hex::ids")]
    pub have: Vec<ObjectId>,
    /// `None` for every ancestor, `Some(0)` for the wanted changesets only,
    /// `Some(n)` for n generations.
    #[serde(default)]
    pub history: Option<u32>,
    /// Path prefixes to limit trees to.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Entries per page.
    #[serde(default)]
    pub limit: Option<u32>,
    /// The cursor from the page before.
    #[serde(default)]
    pub after: Option<String>,
}

/// One object a client is missing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingEntry {
    /// Its id.
    #[serde(with = "hex::id")]
    pub id: ObjectId,
    /// Its kind's name.
    pub kind: String,
    /// Its size in bytes.
    pub size: u64,
}

/// `POST …/missing` response, as JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingResponse {
    /// The objects, a changeset before what it names.
    pub missing: Vec<MissingEntry>,
    /// The cursor for the next page, or `None` when done.
    pub next: Option<String>,
}

// ---------------------------------------------------------------- section 8

/// `GET …/refs`, and each `refs` event of the watch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refs {
    /// The op log's sequence number.
    pub seq: u64,
    /// Every ref.
    #[serde(with = "hex::id_map")]
    pub refs: BTreeMap<String, ObjectId>,
}

/// How a landing goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LandMode {
    /// Fast-forward, or rebase onto the head.
    #[default]
    Rebase,
    /// Fast-forward only; refuse otherwise.
    FfOnly,
}

/// `POST …/land` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandRequest {
    /// The ref to land on.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The stack, oldest first.
    #[serde(with = "hex::ids")]
    pub changes: Vec<ObjectId>,
    /// How.
    #[serde(default)]
    pub mode: LandMode,
    /// Who is landing, for the op log.
    #[serde(default)]
    pub agent: Option<String>,
}

/// Where one changeset of a stack went.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placed {
    /// The changeset as sent.
    #[serde(with = "hex::id")]
    pub from: ObjectId,
    /// What holds it now, or `None` when it went nowhere.
    #[serde(with = "hex::opt_id")]
    pub to: Option<ObjectId>,
}

/// `POST …/land` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandResponse {
    /// The ref.
    #[serde(rename = "ref")]
    pub reference: String,
    /// `fast-forward`, `rebased`, `already-applied`, `conflicts` or
    /// `refused`.
    pub outcome: String,
    /// The ref's head before.
    #[serde(with = "hex::opt_id")]
    pub head_before: Option<ObjectId>,
    /// Its head after.
    #[serde(with = "hex::opt_id")]
    pub head: Option<ObjectId>,
    /// Where each changeset of the stack went.
    pub landed: Vec<Placed>,
    /// Paths both sides changed, merged.
    #[serde(default)]
    pub merged_paths: Vec<String>,
    /// With `conflicts`: each conflict, as `braid land --json` prints it
    /// (see [`crate::json::conflict`]).
    #[serde(default)]
    pub conflicts: Vec<serde_json::Value>,
    /// The changeset that conflicts.
    #[serde(default, with = "hex::opt_id")]
    pub change: Option<ObjectId>,
    /// The git commit the head exports to, or `None` when the hub cannot
    /// export it: never a commit the head would not export to.
    #[serde(default)]
    pub git_commit: Option<String>,
    /// The op log operation that moved the ref, or `None` when it did not
    /// move.
    #[serde(default)]
    pub operation: Option<u64>,
}

// ---------------------------------------------------------------- section 9

/// `GET …/git/commit/{sha1}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommit {
    /// The changeset.
    #[serde(with = "hex::id")]
    pub changeset: ObjectId,
    /// Its root tree.
    #[serde(with = "hex::id")]
    pub tree: ObjectId,
    /// The git tree of the commit.
    pub git_tree: String,
}

/// `GET …/git/changeset/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitChangeset {
    /// The changeset.
    #[serde(with = "hex::id")]
    pub changeset: ObjectId,
    /// The git commit it exports to.
    pub git_commit: String,
}

// --------------------------------------------------------------- section 10

/// One entry of a tree listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    /// The path, from the repository's root.
    pub path: String,
    /// `file`, `executable`, `symlink`, `tree` or `gitlink`.
    pub kind: String,
    /// The braid id (blob or tree), or the git commit of a gitlink.
    pub id: String,
    /// The size, for files and symlinks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// A directory below the requested depth.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// `GET …/t/{tree}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeListing {
    /// The tree listed.
    #[serde(with = "hex::id")]
    pub tree: ObjectId,
    /// The prefix asked for.
    pub prefix: String,
    /// The entries.
    pub entries: Vec<TreeEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::Kind;

    #[test]
    fn device_poll_is_tagged_by_status() {
        let ok = DevicePoll::Ok {
            token: "brd_x".into(),
            login: "ada".into(),
            expires: "2027-01-01T00:00:00Z".into(),
        };
        let text = serde_json::to_string(&ok).unwrap();
        assert!(text.contains("\"status\":\"ok\""), "{text}");
        assert_eq!(serde_json::from_str::<DevicePoll>(&text).unwrap(), ok);
        assert_eq!(
            serde_json::from_str::<DevicePoll>(r#"{"status":"slow_down","interval":10}"#).unwrap(),
            DevicePoll::SlowDown { interval: 10 }
        );
    }

    #[test]
    fn land_request_on_the_wire() {
        let id = ObjectId::hash(Kind::Changeset, b"x");
        let request = LandRequest {
            reference: "main".into(),
            changes: vec![id],
            mode: LandMode::FfOnly,
            agent: Some("agent-1".into()),
        };
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains("\"ref\":\"main\""), "{text}");
        assert!(text.contains("\"mode\":\"ff-only\""), "{text}");
        assert!(text.contains(&id.to_hex()));
        assert_eq!(serde_json::from_str::<LandRequest>(&text).unwrap(), request);
        // The mode defaults to rebase.
        let bare: LandRequest =
            serde_json::from_str(&format!(r#"{{"ref":"main","changes":["{id}"]}}"#)).unwrap();
        assert_eq!(bare.mode, LandMode::Rebase);
        // A bad id is refused.
        assert!(
            serde_json::from_str::<LandRequest>(r#"{"ref":"main","changes":["xyz"]}"#).is_err()
        );
    }

    #[test]
    fn what_an_older_hub_sends_still_reads() {
        let info: RepoInfo = serde_json::from_str(
            r#"{"owner":"ada","name":"demo","private":true,"description":"","default_branch":"main","created":"2026-01-01T00:00:00Z","mirror":{"url":"https://github.com/ada/demo.git"},"permission":"read"}"#,
        )
        .unwrap();
        assert!(info.id.is_empty() && info.topics.is_empty() && info.fork_of.is_none());
        assert!(info.counts.is_none() && info.viewer.is_none() && info.settings.is_none());
        let mirror = info.mirror.as_ref().unwrap();
        assert_eq!((&mirror.follow, mirror.github_id), (&None, None));
        // Written again, it says no more than it was told, but `topics`,
        // `fork_of` and `pushed`.
        let text = serde_json::to_string(&info).unwrap();
        for absent in [
            "\"id\"", "counts", "viewer", "settings", "tdm", "follow", "synced",
        ] {
            assert!(!text.contains(absent), "{absent}: {text}");
        }
        let root: Root = serde_json::from_str(
            r#"{"version":1,"limits":{"max_body":1,"max_put":1,"max_json":1,"max_ids":1,"max_paths":1,"missing_page":1,"missing_page_max":1,"missing_objects":1,"tree_entries":1,"tree_depth":1,"max_list":1}}"#,
        )
        .unwrap();
        assert_eq!(root.limits.social, None);
        let me: Me = serde_json::from_str(r#"{"login":"ada"}"#).unwrap();
        assert_eq!((me.settings, me.github_scope), (None, None));
    }

    #[test]
    fn refs_and_repo_create_defaults() {
        let id = ObjectId::hash(Kind::Changeset, b"y");
        let refs = Refs {
            seq: 3,
            refs: [("main".to_owned(), id)].into_iter().collect(),
        };
        let text = serde_json::to_string(&refs).unwrap();
        assert_eq!(serde_json::from_str::<Refs>(&text).unwrap(), refs);
        let create: RepoCreate = serde_json::from_str(r#"{"name":"demo"}"#).unwrap();
        assert!(create.private);
        assert_eq!(create.import, None);
    }
}
