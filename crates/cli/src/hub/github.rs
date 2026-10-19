// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub github`: granting the hub GitHub access, and syncing your
//! GitHub repositories to it (sections 2 and 23 of `docs/hub.md`).
//!
//! - `grant [--public-only]`: a device flow run through the hub, as `braid
//!   login` runs one: braid prints a web address and a code, and waits
//!   until you enter the code on GitHub and approve.
//! - `repos [--refresh]`: the repositories you own on GitHub, each with
//!   the name it takes on the hub and whether it may be imported.
//! - `sync (--all | FULL_NAME...)`: imports them, each kept as a mirror
//!   that follows GitHub, in a batch of jobs; `--wait` waits for the batch
//!   to end.
//! - `status [BATCH]`, `cancel BATCH` and `retry BATCH FULL_NAME`: how a
//!   batch stands, cancelling what has not started, and importing again
//!   what failed or was skipped.
//!
//! Exit statuses as the other hub commands: 0 done, 1 refused (and, for a
//! sync, anything that failed or was skipped), 3 when the hub could not be
//! reached or failed.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Result, bail};
use braid_hubproto::api::DeviceStart;
use braid_hubproto::onboard::{
    self as proto, GithubRepos, GrantPoll, RepoPick, SyncBatch, SyncRequest, SyncStarted,
};
use clap::{Args, Subcommand};
use serde_json::json;

use super::creds::{self, HubEnv};
use super::http::HttpHub;
use super::remote::{HubError, shown};
use super::social_common::{REFUSED, exit_for, idempotency_key, print};
use crate::Cli;

/// How long `sync --wait` waits between two looks at the batch.
const POLL: Duration = Duration::from_millis(500);

/// The long help of `hub github sync`.
const SYNC_HELP: &str = "Import repositories you own on GitHub to the hub, each kept as a mirror
that follows GitHub: what lands on the hub is pushed to GitHub, and what is
pushed to GitHub is brought to the hub, merged when both moved (--follow
ff-only brings it only when nothing landed on the hub meanwhile, --follow off
never brings it). Name them as `braid hub github repos` lists them
(OWNER/NAME), or ask for --all: every one that may be imported, most
recently pushed first, as many as your account has room for.

A repository takes its own name on the hub, unless --name OWNER/NAME=NEW
gives another; a name that starts with '.' takes '_' instead. Private
repositories stay private; public ones stay public, unless --all-private.
--reserve-tdm reserves text and data mining of what is imported. An archived
repository is imported without a mirror; an empty one is made empty, with
its mirror set.

The hub needs your GitHub access first: `braid hub github grant`. One sync
runs at a time. With --wait, braid waits until every import ended, and the
exit status is 1 when any failed or was skipped. With --json it prints the
batch, its queued and skipped repositories, or with --wait the batch as it
ended (as `braid hub github status --json`).";

/// `hub github`.
#[derive(Debug, Args)]
pub(crate) struct GithubArgs {
    #[command(subcommand)]
    pub(crate) action: GithubAction,
}

/// What `hub github` does.
#[derive(Debug, Subcommand)]
pub(crate) enum GithubAction {
    /// Grant the hub access to your GitHub repositories, with a code you
    /// enter on GitHub.
    Grant(GrantArgs),
    /// List the repositories you own on GitHub, and what each would become
    /// on the hub.
    Repos(ReposArgs),
    /// Import repositories you own on GitHub to the hub, each kept as a
    /// mirror.
    #[command(long_about = SYNC_HELP)]
    Sync(SyncArgs),
    /// Show how a sync stands: the latest, or BATCH.
    Status(StatusArgs),
    /// Cancel the imports of a sync that have not started.
    Cancel(CancelArgs),
    /// Import again a repository of a sync that failed or was skipped.
    Retry(RetryArgs),
}

/// `hub github grant`.
#[derive(Debug, Args)]
pub(crate) struct GrantArgs {
    /// Grant access to your public repositories only (GitHub's
    /// public_repo), not your private ones (repo).
    #[arg(long)]
    pub(crate) public_only: bool,
}

/// `hub github repos`.
#[derive(Debug, Args)]
pub(crate) struct ReposArgs {
    /// Ask GitHub again rather than take the hub's listing of the last ten
    /// minutes (at most once a minute).
    #[arg(long)]
    pub(crate) refresh: bool,
}

/// `hub github sync`.
#[derive(Debug, Args)]
pub(crate) struct SyncArgs {
    /// Every repository that may be imported, as many as there is room for.
    #[arg(long, conflicts_with = "repos")]
    pub(crate) all: bool,
    /// The repositories to import: OWNER/NAME on GitHub.
    #[arg(value_name = "FULL_NAME", required_unless_present = "all")]
    pub(crate) repos: Vec<String>,
    /// The name a repository takes on the hub: OWNER/NAME=NEW.
    #[arg(long = "name", value_name = "FULL_NAME=NAME")]
    pub(crate) names: Vec<String>,
    /// Import public repositories as private too.
    #[arg(long)]
    pub(crate) all_private: bool,
    /// Reserve text and data mining of the repositories imported.
    #[arg(long)]
    pub(crate) reserve_tdm: bool,
    /// How each mirror takes in what is pushed to GitHub.
    #[arg(long, value_name = "POLICY", default_value = "merge",
          value_parser = [proto::FOLLOW_MERGE, proto::FOLLOW_FF_ONLY, proto::FOLLOW_OFF])]
    pub(crate) follow: String,
    /// Wait until every import ended.
    #[arg(long)]
    pub(crate) wait: bool,
}

/// `hub github status`.
#[derive(Debug, Args)]
pub(crate) struct StatusArgs {
    /// The sync (default: the latest).
    #[arg(value_name = "BATCH")]
    pub(crate) batch: Option<String>,
}

/// `hub github cancel`.
#[derive(Debug, Args)]
pub(crate) struct CancelArgs {
    /// The sync.
    #[arg(value_name = "BATCH")]
    pub(crate) batch: String,
}

/// `hub github retry`.
#[derive(Debug, Args)]
pub(crate) struct RetryArgs {
    /// The sync.
    #[arg(value_name = "BATCH")]
    pub(crate) batch: String,
    /// The repository to import again: OWNER/NAME on GitHub.
    #[arg(value_name = "FULL_NAME")]
    pub(crate) full_name: String,
    /// The name it takes on the hub this time.
    #[arg(long, value_name = "NAME")]
    pub(crate) name: Option<String>,
}

pub(crate) fn github(cli: &Cli, args: &GithubArgs) -> Result<ExitCode> {
    match &args.action {
        GithubAction::Grant(args) => grant(cli, args),
        GithubAction::Repos(args) => repos(cli, args),
        GithubAction::Sync(args) => sync(cli, args),
        GithubAction::Status(args) => status(cli, args),
        GithubAction::Cancel(args) => cancel(cli, args),
        GithubAction::Retry(args) => retry(cli, args),
    }
}

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

/// A batch id given on the command line, checked before it names a route:
/// `b_` and 26 characters of `a-z 2-7`.
fn batch_arg(id: &str) -> Result<&str> {
    let valid = id.strip_prefix("b_").is_some_and(|rest| {
        rest.len() == 26
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    });
    if !valid {
        bail!("{id:?} is not a sync's id: b_ and 26 characters of a-z and 2-7");
    }
    Ok(id)
}

/// `hub github grant`: starts the grant, prints where to approve it, and
/// polls until GitHub says.
fn grant(cli: &Cli, args: &GrantArgs) -> Result<ExitCode> {
    let hub = signed_in_hub()?;
    let scope = if args.public_only {
        proto::SCOPE_PUBLIC
    } else {
        proto::SCOPE_PRIVATE
    };
    let body = json!({ "scope": scope });
    let start: DeviceStart =
        match hub.call_json("POST", "/v1/github/access/device", &[], Some(&body)) {
            Ok(start) => start,
            Err(err) => return Ok(failed(&err)),
        };
    eprintln!(
        "To grant the hub access to your {} GitHub repositories, open {} and enter the code {}",
        if args.public_only {
            "public"
        } else {
            "private and public"
        },
        shown(&start.verification_uri),
        shown(&start.user_code)
    );
    let mut interval = start.interval.max(1);
    let mut waited = 0u64;
    loop {
        if waited >= start.expires_in {
            bail!(
                "the code expired before the grant was approved: run `braid hub github grant` again"
            );
        }
        sleep(Duration::from_secs(interval));
        waited = waited.saturating_add(interval);
        let poll = json!({ "poll": start.poll });
        match hub.call_json::<GrantPoll>("POST", "/v1/github/access/device/poll", &[], Some(&poll))
        {
            Ok(GrantPoll::Pending) => {}
            Ok(GrantPoll::SlowDown { interval: asked }) => interval = asked.max(interval),
            Ok(GrantPoll::Denied) => {
                eprintln!("braid: error: the grant was denied on GitHub: nothing changed");
                return Ok(ExitCode::from(REFUSED));
            }
            Ok(GrantPoll::Expired) => {
                bail!(
                    "the code expired before the grant was approved: run `braid hub github grant` again"
                )
            }
            Ok(GrantPoll::Ok { login, scope }) => {
                let value = json!({ "login": login, "scope": scope });
                print(cli, &value, || {
                    format!(
                        "GitHub access granted for {} ({}). `braid hub github repos` lists your repositories.",
                        shown(&login),
                        shown(&scope)
                    )
                })?;
                return Ok(ExitCode::SUCCESS);
            }
            // Asked to wait: wait as long as the hub says, then poll again.
            Err(HubError::Api { status: 429, body }) => {
                interval = body.retry_after.unwrap_or(interval).max(interval);
            }
            Err(err) => return Ok(failed(&err)),
        }
    }
}

/// A repository of a listing, in one line, for people: every field as the
/// hub sent it, escaped.
fn listed_line(repo: &proto::GithubRepo) -> String {
    let mut marks = Vec::new();
    if repo.private {
        marks.push("private");
    }
    if repo.fork {
        marks.push("fork");
    }
    if repo.archived {
        marks.push("archived");
    }
    if repo.empty {
        marks.push("empty");
    }
    let mut out = format!(
        "{}  {}",
        if repo.eligible { "+" } else { "-" },
        shown(&repo.full_name)
    );
    if !marks.is_empty() {
        let _ = write!(out, " ({})", marks.join(", "));
    }
    let _ = write!(out, "  -> {}", shown(&repo.hub_name));
    if let Some(synced) = &repo.synced {
        let _ = write!(out, "  synced as {}", shown(synced));
    } else if let Some(reason) = &repo.reason {
        let _ = write!(out, "  {}", shown(reason));
    }
    out
}

/// `hub github repos`.
fn repos(cli: &Cli, args: &ReposArgs) -> Result<ExitCode> {
    let hub = signed_in_hub()?;
    let route = if args.refresh {
        "/v1/github/repos?refresh=1"
    } else {
        "/v1/github/repos"
    };
    let listed: GithubRepos = match hub.call_json("GET", route, &[], None) {
        Ok(listed) => listed,
        Err(err) => return Ok(failed(&err)),
    };
    print(cli, &serde_json::to_value(&listed)?, || {
        let mut out = String::new();
        if listed.repos.is_empty() {
            out.push_str("No repositories on GitHub that the access you granted shows.");
        }
        for repo in &listed.repos {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&listed_line(repo));
        }
        let _ = write!(
            out,
            "\nRoom left on the hub: {} repositories, {} MiB.",
            listed.quota.repos_left,
            listed.quota.bytes_left >> 20
        );
        if listed.truncated {
            out.push_str("\nGitHub has more: these are the most recently pushed.");
        }
        out
    })?;
    Ok(ExitCode::SUCCESS)
}

/// The names `--name FULL_NAME=NAME` gives, by full name.
fn names_of(given: &[String]) -> Result<BTreeMap<String, String>> {
    let mut names = BTreeMap::new();
    for pair in given {
        let Some((full_name, name)) = pair.split_once('=') else {
            bail!("--name takes OWNER/NAME=NEW, not {pair:?}");
        };
        if !full_name.contains('/') || name.is_empty() {
            bail!("--name takes OWNER/NAME=NEW, not {pair:?}");
        }
        names.insert(full_name.to_owned(), name.to_owned());
    }
    Ok(names)
}

/// A batch, in lines, for people: every field as the hub sent it, escaped.
fn described(batch: &SyncBatch) -> String {
    let counts = &batch.counts;
    let mut out = format!(
        "Sync {}: {}. {} imported, {} under way, {} queued, {} failed, {} skipped, {} cancelled.",
        shown(&batch.batch),
        shown(&batch.state),
        counts.done,
        counts.running + counts.waiting,
        counts.queued,
        counts.failed,
        counts.skipped,
        counts.cancelled
    );
    for item in &batch.repos {
        let _ = write!(
            out,
            "\n  {}  -> {}  {}",
            shown(&item.full_name),
            shown(&item.hub_name),
            shown(&item.state)
        );
        if let Some(error) = &item.error {
            let _ = write!(out, ": {}", shown(error));
        }
    }
    out
}

/// Asks for the batch `id` until it ends.
fn wait_for(hub: &HttpHub, id: &str) -> Result<SyncBatch, HubError> {
    loop {
        let batch: SyncBatch = hub.call_json("GET", &format!("/v1/github/sync/{id}"), &[], None)?;
        if batch.state != proto::BATCH_RUNNING {
            return Ok(batch);
        }
        sleep(POLL);
    }
}

/// `hub github sync`.
fn sync(cli: &Cli, args: &SyncArgs) -> Result<ExitCode> {
    let request = SyncRequest {
        all: args.all,
        repos: args
            .repos
            .iter()
            .map(|full_name| RepoPick::Name(full_name.clone()))
            .collect(),
        names: names_of(&args.names)?,
        keep_public: !args.all_private,
        reserve_tdm: args.reserve_tdm,
        follow: args.follow.clone(),
    };
    for full_name in &args.repos {
        if !full_name.contains('/') {
            bail!("{full_name:?} is not OWNER/NAME on GitHub");
        }
    }
    let hub = signed_in_hub()?;
    let body = serde_json::to_value(&request)?;
    // Sent again after a lost answer, the batch is not made twice.
    let key = idempotency_key();
    let started: SyncStarted = match hub.call_json(
        "POST",
        "/v1/github/sync",
        &[("Idempotency-Key", &key)],
        Some(&body),
    ) {
        Ok(started) => started,
        Err(err) => return Ok(failed(&err)),
    };
    let Ok(id) = batch_arg(&started.batch) else {
        return Ok(failed(&HubError::Protocol(format!(
            "the hub named the sync {}, which is not a sync's id",
            shown(&started.batch)
        ))));
    };
    if !args.wait {
        print(cli, &serde_json::to_value(&started)?, || {
            let mut out = format!(
                "Sync {id}: {} queued, {} skipped. `braid hub github status {id}` shows how it goes.",
                started.queued.len(),
                started.skipped.len()
            );
            for skipped in &started.skipped {
                let _ = write!(
                    out,
                    "\n  skipped {}: {}",
                    shown(&skipped.full_name),
                    shown(&skipped.reason)
                );
            }
            out
        })?;
        return Ok(if started.skipped.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(REFUSED)
        });
    }
    if !cli.json {
        eprintln!(
            "Sync {id}: {} queued, {} skipped; waiting for the imports.",
            started.queued.len(),
            started.skipped.len()
        );
    }
    let batch = match wait_for(&hub, id) {
        Ok(batch) => batch,
        Err(err) => return Ok(failed(&err)),
    };
    print(cli, &serde_json::to_value(&batch)?, || described(&batch))?;
    Ok(if batch.all_done() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(REFUSED)
    })
}

/// `hub github status`.
fn status(cli: &Cli, args: &StatusArgs) -> Result<ExitCode> {
    let route = match &args.batch {
        Some(batch) => format!("/v1/github/sync/{}", batch_arg(batch)?),
        None => "/v1/github/sync".to_owned(),
    };
    let hub = signed_in_hub()?;
    let batch: SyncBatch = match hub.call_json("GET", &route, &[], None) {
        Ok(batch) => batch,
        Err(err) => return Ok(failed(&err)),
    };
    print(cli, &serde_json::to_value(&batch)?, || described(&batch))?;
    Ok(ExitCode::SUCCESS)
}

/// `hub github cancel`.
fn cancel(cli: &Cli, args: &CancelArgs) -> Result<ExitCode> {
    let id = batch_arg(&args.batch)?;
    let hub = signed_in_hub()?;
    let batch: SyncBatch =
        match hub.call_json("DELETE", &format!("/v1/github/sync/{id}"), &[], None) {
            Ok(batch) => batch,
            Err(err) => return Ok(failed(&err)),
        };
    print(cli, &serde_json::to_value(&batch)?, || described(&batch))?;
    Ok(ExitCode::SUCCESS)
}

/// `hub github retry`.
fn retry(cli: &Cli, args: &RetryArgs) -> Result<ExitCode> {
    let id = batch_arg(&args.batch)?;
    let hub = signed_in_hub()?;
    let batch: SyncBatch = match hub.call_json("GET", &format!("/v1/github/sync/{id}"), &[], None) {
        Ok(batch) => batch,
        Err(err) => return Ok(failed(&err)),
    };
    let Some(item) = batch
        .repos
        .iter()
        .find(|item| item.full_name.eq_ignore_ascii_case(&args.full_name))
    else {
        eprintln!(
            "braid: error: {} is not a repository of sync {id}",
            shown(&args.full_name)
        );
        return Ok(ExitCode::from(REFUSED));
    };
    let mut body = json!({ "github_id": item.github_id });
    if let Some(name) = &args.name {
        body["name"] = json!(name);
    }
    let started: SyncStarted = match hub.call_json(
        "POST",
        &format!("/v1/github/sync/{id}/retry"),
        &[],
        Some(&body),
    ) {
        Ok(started) => started,
        Err(err) => return Ok(failed(&err)),
    };
    print(cli, &serde_json::to_value(&started)?, || {
        let queued = started
            .queued
            .first()
            .map_or_else(String::new, |queued| shown(&queued.hub_name));
        format!(
            "Importing {} again, as {queued}. `braid hub github status {id}` shows how it goes.",
            shown(&args.full_name)
        )
    })?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_checked_before_they_name_a_route() {
        assert!(batch_arg("b_abcdefghijklmnopqrstuvwxyz").is_ok());
        for bad in ["", "b_", "j_abcdefghijklmnopqrstuvwxyz", "b_../../v1/x"] {
            assert!(batch_arg(bad).is_err(), "{bad:?}");
        }
        let names = names_of(&["ada/.github=dotgithub".to_owned()]).unwrap();
        assert_eq!(names["ada/.github"], "dotgithub");
        assert!(names_of(&["demo=x".to_owned()]).is_err());
        assert!(names_of(&["ada/demo=".to_owned()]).is_err());
        assert!(names_of(&["ada/demo".to_owned()]).is_err());
    }

    #[test]
    fn what_the_hub_says_reaches_the_terminal_escaped() {
        let repo = proto::GithubRepo {
            full_name: "ada/demo\u{1b}[2J".into(),
            hub_name: "demo".into(),
            private: true,
            reason: Some("name taken\u{7}".into()),
            ..proto::GithubRepo::default()
        };
        let line = listed_line(&repo);
        assert!(line.starts_with("-  ada/demo"), "{line:?}");
        assert!(
            !line.contains('\u{1b}') && !line.contains('\u{7}'),
            "{line:?}"
        );
        let batch = SyncBatch {
            batch: "b_x\u{1b}]0;t\u{7}".into(),
            state: "done".into(),
            repos: vec![proto::BatchItem {
                full_name: "ada/demo".into(),
                hub_name: "demo".into(),
                state: "failed".into(),
                error: Some("no\u{1b}[31m".into()),
                ..proto::BatchItem::default()
            }],
            ..SyncBatch::default()
        };
        let text = described(&batch);
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text:?}"
        );
    }
}
