// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub fork`, `sync-fork` and `jobs`: forking a repository, bringing
//! what its upstream landed into a fork, and the jobs that do it.
//!
//! A fork is made by a job of the hub (section 20 of `docs/hub.md`): `fork`
//! asks for it, then waits until the job ends, unless `--no-wait`, and may
//! clone the fork it made. Exit statuses as the other hub commands: 0
//! done, 1 refused (a fork or a job that failed too), 2 when a fork and its
//! upstream diverged, 3 when the hub could not be reached or failed.

use std::fmt::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};
use braid_hubproto::RepoPath;
use braid_hubproto::forks::{self as proto, ForkStarted, Job, Jobs, SyncedUpstream};
use clap::Args;
use serde_json::json;

use super::creds::{self, HubEnv};
use super::http::HttpHub;
use super::remote::{HubError, shown};
use super::social_common::{
    CONFLICTS, HubRepoArg, REFUSED, exit_for, hub_repo, idempotency_key, print,
};
use super::{CloneArgs, HubAction};
use crate::Cli;

/// How long `--wait` waits between two looks at a job.
const POLL: Duration = Duration::from_millis(500);

/// The hub signed in to: BRAID_HUB_URL, else the hub signed in to last,
/// else the public hub.
fn signed_in_hub() -> Result<HttpHub> {
    let env = HubEnv::from_process();
    let creds = env.credentials()?;
    let url = creds::hub_url(&env, &creds, None)?;
    Ok(super::client(&env, &url)?.0)
}

/// What a command answers when the hub refused it or could not be
/// reached: the error, and the exit status it calls for.
fn failed(err: &HubError) -> ExitCode {
    eprintln!("braid: error: {err}");
    ExitCode::from(exit_for(err))
}

/// `text` as a value of a query string: everything but letters, digits
/// and `-._~` percent-encoded.
fn query_value(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// A job id given on the command line, checked before it names a route:
/// `j_` and 26 characters of `a-z 2-7`.
fn job_arg(id: &str) -> Result<&str> {
    let valid = id.strip_prefix("j_").is_some_and(|rest| {
        rest.len() == 26
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    });
    if !valid {
        anyhow::bail!("{id:?} is not a job id: j_ and 26 characters of a-z and 2-7");
    }
    Ok(id)
}

/// Asks for the job `id` until it ends.
fn wait_for(hub: &HttpHub, id: &str) -> Result<Job, HubError> {
    loop {
        let job: Job = hub.call_json("GET", &format!("/v1/jobs/{id}"), &[], None)?;
        if job.is_finished() {
            return Ok(job);
        }
        sleep(POLL);
    }
}

/// A job, in one line or two, for people: every field as the hub sent it,
/// escaped.
fn described(job: &Job) -> String {
    let mut out = format!(
        "{}  {}  {}",
        shown(&job.id),
        shown(&job.kind),
        shown(&job.state)
    );
    if let Some(repo) = &job.repo {
        let _ = write!(out, "  {}", shown(repo));
    }
    if job.progress.objects > 0 && !job.is_finished() {
        let _ = write!(out, "  ({} objects so far)", job.progress.objects);
    }
    if let Some(error) = &job.error {
        let _ = write!(out, "\n    {}", shown(error));
    }
    out
}

/// The exit status of a job that ended: 0 when it did what it was asked or
/// had nothing to do, 1 otherwise.
fn ended(job: &Job) -> ExitCode {
    match job.state.as_str() {
        proto::DONE | proto::SKIPPED => ExitCode::SUCCESS,
        _ => ExitCode::from(REFUSED),
    }
}

/// `hub fork`.
#[derive(Debug, Args)]
pub(crate) struct ForkArgs {
    // Not named `repo`: that is the id of the global --repo.
    /// The repository to fork: OWNER/NAME.
    #[arg(value_name = "OWNER/NAME")]
    pub(crate) source: String,
    /// The fork's name in your account (default: the repository's).
    #[arg(long, value_name = "NAME")]
    pub(crate) name: Option<String>,
    /// Make the fork private (a fork of a private repository always is).
    #[arg(long)]
    pub(crate) private: bool,
    /// Clone the fork once it is made, into DIR (default: its name).
    #[arg(long, value_name = "DIR", num_args = 0..=1)]
    pub(crate) clone: Option<Option<PathBuf>>,
    /// Do not wait for the fork to be made: print its job, and return.
    #[arg(long, conflicts_with = "clone")]
    pub(crate) no_wait: bool,
}

pub(crate) fn fork(cli: &Cli, args: &ForkArgs) -> Result<ExitCode> {
    let path: RepoPath = args
        .source
        .parse()
        .with_context(|| format!("{:?} is not OWNER/NAME", args.source))?;
    let hub = signed_in_hub()?;
    let mut body = json!({});
    if let Some(name) = &args.name {
        body["name"] = json!(name);
    }
    if args.private {
        body["private"] = json!(true);
    }
    // Sent again after a lost answer, the request is not made twice.
    let key = idempotency_key();
    let route = format!("{}/forks", path.api_path());
    let started: ForkStarted =
        match hub.call_json("POST", &route, &[("Idempotency-Key", &key)], Some(&body)) {
            Ok(started) => started,
            Err(err) => return Ok(failed(&err)),
        };
    let fork = format!("{}/{}", started.repo.owner, started.repo.name);
    // Checked as the id it says it is before it names a route, or a line.
    let Ok(id) = job_arg(&started.job.id) else {
        return Ok(failed(&HubError::Protocol(format!(
            "the hub named its job {}, which is not a job id",
            shown(&started.job.id)
        ))));
    };
    if args.no_wait {
        print(cli, &serde_json::to_value(&started)?, || {
            format!(
                "Forking {path} into {}: job {id} is {}. `braid hub jobs {id} --wait` waits for it.",
                shown(&fork),
                shown(&started.job.state),
            )
        })?;
        return Ok(ExitCode::SUCCESS);
    }
    let job = match wait_for(&hub, id) {
        Ok(job) => job,
        Err(err) => return Ok(failed(&err)),
    };
    let value = json!({"job": job, "repo": started.repo});
    if job.state != proto::DONE {
        if cli.json {
            crate::print_json(&value)?;
        } else {
            eprintln!(
                "braid: error: forking {path} into {} did not go through: {}",
                shown(&fork),
                shown(job.error.as_deref().unwrap_or(&job.state))
            );
        }
        return Ok(ended(&job));
    }
    print(cli, &value, || {
        format!("Forked {path} into {}.", shown(&fork))
    })?;
    match &args.clone {
        Some(dir) => super::hub(
            cli,
            &HubAction::Clone(CloneArgs {
                source: fork,
                dir: dir.clone(),
                paths: Vec::new(),
            }),
        ),
        None => Ok(ExitCode::SUCCESS),
    }
}

/// `hub sync-fork`.
#[derive(Debug, Args)]
pub(crate) struct SyncForkArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// The branch or tag to bring up to date (default: the fork's default
    /// branch); the upstream's of the same name is brought in.
    #[arg(long = "ref", value_name = "REF")]
    pub(crate) reference: Option<String>,
}

pub(crate) fn sync_fork(cli: &Cli, args: &SyncForkArgs) -> Result<ExitCode> {
    let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let mut body = json!({});
    if let Some(reference) = &args.reference {
        body["ref"] = json!(reference);
    }
    let route = format!("{}/sync-upstream", path.api_path());
    let synced: SyncedUpstream = match hub.call_json("POST", &route, &[], Some(&body)) {
        Ok(synced) => synced,
        Err(err) => return Ok(failed(&err)),
    };
    let reference = shown(&synced.reference);
    print(cli, &serde_json::to_value(&synced)?, || {
        match synced.outcome.as_str() {
            proto::FAST_FORWARD => format!(
                "Synced {reference} of {path} with its upstream: it moved to {}.",
                synced
                    .head
                    .map_or_else(|| "nothing".to_owned(), |head| head.short().to_string())
            ),
            proto::DIVERGED => format!(
                "{reference} of {path} and its upstream diverged: the upstream has {}{} changesets it lacks, and it has its own. Merge the upstream's {reference} into it, then land.",
                synced.behind,
                if synced.more { "+" } else { "" }
            ),
            _ => format!("{reference} of {path} is up to date with its upstream."),
        }
    })?;
    if synced.outcome == proto::DIVERGED {
        return Ok(ExitCode::from(CONFLICTS));
    }
    Ok(ExitCode::SUCCESS)
}

/// `hub jobs`.
#[derive(Debug, Args)]
pub(crate) struct JobsArgs {
    /// The job to show (default: list your jobs, newest first).
    #[arg(value_name = "ID")]
    pub(crate) id: Option<String>,
    /// Wait until the job ends; the exit status is 1 when it failed or was
    /// cancelled.
    #[arg(long, requires = "id")]
    pub(crate) wait: bool,
    /// The most jobs to list, 1 to 100 (default: 30).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=100), conflicts_with = "id")]
    pub(crate) limit: Option<u32>,
    /// Where to start: the `next` of the page before.
    #[arg(long, value_name = "CURSOR", conflicts_with = "id")]
    pub(crate) after: Option<String>,
}

pub(crate) fn jobs(cli: &Cli, args: &JobsArgs) -> Result<ExitCode> {
    let hub = signed_in_hub()?;
    if let Some(id) = &args.id {
        let id = job_arg(id)?;
        let job: Job = if args.wait {
            match wait_for(&hub, id) {
                Ok(job) => job,
                Err(err) => return Ok(failed(&err)),
            }
        } else {
            match hub.call_json("GET", &format!("/v1/jobs/{id}"), &[], None) {
                Ok(job) => job,
                Err(err) => return Ok(failed(&err)),
            }
        };
        print(cli, &serde_json::to_value(&job)?, || described(&job))?;
        return Ok(if args.wait {
            ended(&job)
        } else {
            ExitCode::SUCCESS
        });
    }
    let mut query = Vec::new();
    if let Some(limit) = args.limit {
        query.push(format!("limit={limit}"));
    }
    if let Some(after) = &args.after {
        query.push(format!("after={}", query_value(after)));
    }
    let mut route = "/v1/jobs".to_owned();
    if !query.is_empty() {
        route = format!("{route}?{}", query.join("&"));
    }
    let page: Jobs = match hub.call_json("GET", &route, &[], None) {
        Ok(page) => page,
        Err(err) => return Ok(failed(&err)),
    };
    print(cli, &serde_json::to_value(&page)?, || {
        let mut out = String::new();
        if page.jobs.is_empty() {
            out.push_str("No jobs.");
        }
        for job in &page.jobs {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&described(job));
        }
        if let Some(next) = &page.next {
            let _ = write!(out, "\nMore: braid hub jobs --after {}", shown(next));
        }
        out
    })?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_and_query_values() {
        assert!(job_arg("j_abcdefghijklmnopqrstuvwxyz").is_ok());
        for bad in [
            "",
            "j_",
            "j_ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            "j_../../v1",
            "r_abcdefghijklmnopqrstuvwxyz",
        ] {
            assert!(job_arg(bad).is_err(), "{bad:?}");
        }
        assert_eq!(query_value("j42"), "j42");
        assert_eq!(query_value("a/b c"), "a%2Fb%20c");
        let job = Job {
            id: "j_abcdefghijklmnopqrstuvwxyz".into(),
            kind: proto::FORK.into(),
            state: proto::FAILED.into(),
            repo: Some("bob/demo".into()),
            error: Some("too large\u{1b}[31m".into()),
            created: "t".into(),
            ..Job::default()
        };
        let text = described(&job);
        assert!(text.starts_with("j_abcdefghijklmnopqrstuvwxyz  fork  failed  bob/demo"));
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert_eq!(ended(&job), ExitCode::from(REFUSED));
        // Every field a hub sends reaches the terminal escaped, not only
        // the error.
        let hostile = Job {
            id: "j_\u{1b}[2J".into(),
            kind: "fork\u{1b}]0;title\u{7}".into(),
            state: "done\u{1b}[31m".into(),
            ..job
        };
        let text = described(&hostile);
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text:?}"
        );
    }
}
