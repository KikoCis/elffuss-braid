// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Forks, jobs and syncing a fork with its upstream (section 20).
//!
//! A fork is made by a job: `POST …/forks` answers at once with the job,
//! and the fork appears whole once the job is done. The list of a
//! repository's forks is a [`RepoCards`](crate::cards::RepoCards). Ids are
//! 64 lowercase hex digits; times are RFC 3339, in UTC.

use braid_format::ObjectId;
use serde::{Deserialize, Serialize};

use crate::cards::RepoRef;
use crate::hex;

/// A job that makes a fork.
pub const FORK: &str = "fork";

/// A job that imports a repository from GitHub.
pub const IMPORT: &str = "import";

/// A job waiting for its turn.
pub const QUEUED: &str = "queued";

/// A job under way.
pub const RUNNING: &str = "running";

/// A job that waits for something outside the hub, such as GitHub's rate
/// limit, before it goes on.
pub const WAITING: &str = "waiting";

/// A job that did what it was asked.
pub const DONE: &str = "done";

/// A job that could not do it: `error` says why.
pub const FAILED: &str = "failed";

/// A job cancelled before it started.
pub const CANCELLED: &str = "cancelled";

/// A job that had nothing to do: `error` says why.
pub const SKIPPED: &str = "skipped";

/// Whether a job in `state` has ended, and will not change again.
pub fn is_finished(state: &str) -> bool {
    matches!(state, DONE | FAILED | CANCELLED | SKIPPED)
}

/// A sync that moved the fork's ref to the upstream's head.
pub const FAST_FORWARD: &str = "fast-forward";

/// A sync that found nothing to bring: the fork has all the upstream has.
pub const UP_TO_DATE: &str = "up-to-date";

/// A sync that moved nothing: the fork and the upstream each have work the
/// other has not.
pub const DIVERGED: &str = "diverged";

/// `POST …/forks`: the fork to make. Both fields are optional: the fork
/// takes the name and the visibility of the repository it forks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkRequest {
    /// The fork's name in the caller's account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether only those given access can read it. A fork of a private
    /// repository is private.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
}

/// How far a job got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobProgress {
    /// Objects copied so far.
    pub objects: u64,
    /// Bytes copied so far.
    pub bytes: u64,
}

/// A job: `GET /v1/jobs/{id}`, and the lists of jobs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// Its id: `j_` and 26 characters of `a-z 2-7`.
    pub id: String,
    /// What it does: `fork` or `import`.
    pub kind: String,
    /// `queued`, `running`, `waiting`, `done`, `failed`, `cancelled` or
    /// `skipped`.
    pub state: String,
    /// How far it got.
    #[serde(default)]
    pub progress: JobProgress,
    /// The repository it makes, `owner/name`.
    #[serde(default)]
    pub repo: Option<String>,
    /// Why it failed, or was skipped.
    #[serde(default)]
    pub error: Option<String>,
    /// When it was asked for.
    pub created: String,
    /// When it last started.
    #[serde(default)]
    pub started: Option<String>,
    /// When it ended.
    #[serde(default)]
    pub finished: Option<String>,
}

impl Job {
    /// Whether it has ended, and will not change again.
    pub fn is_finished(&self) -> bool {
        is_finished(&self.state)
    }
}

/// `GET /v1/jobs`: a page of the account's jobs, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jobs {
    /// The jobs.
    pub jobs: Vec<Job>,
    /// The cursor of the next page, or `None` on the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// The answer of `POST …/forks` (202): the job that makes the fork, and
/// where the fork will be.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkStarted {
    /// The job.
    pub job: Job,
    /// The fork it makes.
    pub repo: RepoRef,
}

/// `POST …/sync-upstream`: which ref to bring up to date.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncUpstream {
    /// The ref, a branch or a tag; the fork's default branch when left
    /// out. The upstream's ref of the same name is the one brought in.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// The answer of `POST …/sync-upstream`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedUpstream {
    /// The ref.
    #[serde(rename = "ref")]
    pub reference: String,
    /// `fast-forward`, `up-to-date` or `diverged`.
    pub outcome: String,
    /// The fork's head before, `None` when the fork lacked the ref.
    #[serde(with = "hex::opt_id")]
    pub head_before: Option<ObjectId>,
    /// Its head after: the upstream's after a fast-forward, the one before
    /// otherwise.
    #[serde(with = "hex::opt_id")]
    pub head: Option<ObjectId>,
    /// How many changesets of the upstream's ref the fork still lacks
    /// after the sync, counted up to [`BEHIND_MOST`]: 0 unless it
    /// diverged.
    pub behind: u64,
    /// Whether there are more than [`BEHIND_MOST`], which `behind` then
    /// holds.
    #[serde(default)]
    pub more: bool,
}

/// The most changesets a fork is counted behind its upstream.
pub const BEHIND_MOST: u64 = 1_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forks_and_jobs_on_the_wire() {
        // A request that names nothing takes the source's name and
        // visibility.
        let empty: ForkRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, ForkRequest::default());
        assert_eq!(serde_json::to_string(&empty).unwrap(), "{}");
        let job = Job {
            id: "j_aaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            kind: FORK.into(),
            state: QUEUED.into(),
            repo: Some("bob/demo".into()),
            created: "2026-01-01T00:00:00Z".into(),
            ..Job::default()
        };
        let started = ForkStarted {
            job: job.clone(),
            repo: RepoRef {
                owner: "bob".into(),
                name: "demo".into(),
            },
        };
        let text = serde_json::to_string(&started).unwrap();
        assert_eq!(serde_json::from_str::<ForkStarted>(&text).unwrap(), started);
        assert!(!job.is_finished());
        for state in [DONE, FAILED, CANCELLED, SKIPPED] {
            assert!(is_finished(state), "{state}");
        }
        for state in [QUEUED, RUNNING, WAITING, "other"] {
            assert!(!is_finished(state), "{state}");
        }
        // A short job, from a hub that says less, still reads.
        let short: Job =
            serde_json::from_str(r#"{"id":"j_x","kind":"fork","state":"done","created":"t"}"#)
                .unwrap();
        assert_eq!((short.progress, short.repo), (JobProgress::default(), None));
    }

    #[test]
    fn syncs_on_the_wire() {
        let asked: SyncUpstream = serde_json::from_str(r#"{"ref":"main"}"#).unwrap();
        assert_eq!(asked.reference.as_deref(), Some("main"));
        assert_eq!(
            serde_json::to_string(&SyncUpstream::default()).unwrap(),
            "{}"
        );
        let head = ObjectId::hash(braid_format::Kind::Blob, b"x");
        let synced = SyncedUpstream {
            reference: "main".into(),
            outcome: FAST_FORWARD.into(),
            head_before: None,
            head: Some(head),
            behind: 0,
            more: false,
        };
        let text = serde_json::to_string(&synced).unwrap();
        assert!(text.contains(r#""ref":"main""#) && text.contains(r#""head_before":null"#));
        assert_eq!(
            serde_json::from_str::<SyncedUpstream>(&text).unwrap(),
            synced
        );
    }
}
