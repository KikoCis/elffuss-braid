// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub star`, `unstar`, `stars`, `watch`, `unwatch`, `follow` and
//! `unfollow`.
//!
//! Each is one request of section 17 of `docs/hub.md`, and each may be sent
//! again: starring a repository starred already, or following an account
//! followed already, does nothing more. The repository is `-R OWNER/NAME`,
//! else the one this repository is linked to. Exit statuses as the other
//! hub commands: 0 done, 1 refused, 3 when the hub could not be reached or
//! failed.

use std::fmt::Write;
use std::process::ExitCode;

use anyhow::{Result, bail};
use braid_hubproto::cards::RepoCards;
use braid_hubproto::names::is_owner;
use braid_hubproto::social::Subscription;
use clap::{Args, ValueEnum};
use serde_json::json;

use super::creds::{self, HubEnv};
use super::http::HttpHub;
use super::remote::HubError;
use super::social_common::{HubRepoArg, exit_for, hub_repo, print};
use crate::Cli;

/// The hub signed in to, for the commands about accounts: BRAID_HUB_URL,
/// else the hub signed in to last, else the public hub.
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

/// A login given on the command line, checked as the hub checks it.
fn login_arg(login: &str) -> Result<String> {
    if !is_owner(login) {
        bail!(
            "{login:?} is not a login: 1 to 39 letters, digits and hyphens, with no hyphen at either end and no two in a row"
        );
    }
    Ok(login.to_ascii_lowercase())
}

/// `hub star`.
#[derive(Debug, Args)]
pub(crate) struct StarArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
}

pub(crate) fn star(cli: &Cli, args: &StarArgs) -> Result<ExitCode> {
    let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    if let Err(err) = hub.call("PUT", &format!("{}/star", path.api_path()), &[], None) {
        return Ok(failed(&err));
    }
    print(
        cli,
        &json!({"repo": path.to_string(), "starred": true}),
        || format!("Starred {path}."),
    )?;
    Ok(ExitCode::SUCCESS)
}

/// `hub unstar`.
#[derive(Debug, Args)]
pub(crate) struct UnstarArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
}

pub(crate) fn unstar(cli: &Cli, args: &UnstarArgs) -> Result<ExitCode> {
    let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    if let Err(err) = hub.call("DELETE", &format!("{}/star", path.api_path()), &[], None) {
        return Ok(failed(&err));
    }
    print(
        cli,
        &json!({"repo": path.to_string(), "starred": false}),
        || format!("Took back the star of {path}."),
    )?;
    Ok(ExitCode::SUCCESS)
}

/// `hub stars`.
#[derive(Debug, Args)]
pub(crate) struct StarsArgs {
    /// List what this account starred (default: your own stars).
    #[arg(long, value_name = "LOGIN")]
    pub(crate) user: Option<String>,
    /// The most repositories to list, 1 to 100 (default: 30).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=100))]
    pub(crate) limit: Option<u32>,
    /// Where to start: the `next` of the page before.
    #[arg(long, value_name = "CURSOR")]
    pub(crate) after: Option<String>,
}

pub(crate) fn stars(cli: &Cli, args: &StarsArgs) -> Result<ExitCode> {
    let user = args.user.as_deref().map(login_arg).transpose()?;
    let mut route = match &user {
        Some(login) => format!("/v1/users/{login}/starred"),
        None => "/v1/me/starred".to_owned(),
    };
    let mut query = Vec::new();
    if let Some(limit) = args.limit {
        query.push(format!("limit={limit}"));
    }
    if let Some(after) = &args.after {
        query.push(format!("after={}", query_value(after)));
    }
    if !query.is_empty() {
        route = format!("{route}?{}", query.join("&"));
    }
    let hub = signed_in_hub()?;
    let page: RepoCards = match hub.call_json("GET", &route, &[], None) {
        Ok(page) => page,
        Err(err) => return Ok(failed(&err)),
    };
    print(cli, &serde_json::to_value(&page)?, || {
        let mut out = String::new();
        if page.repos.is_empty() {
            out.push_str(match &user {
                Some(_) => "No stars of repositories you can read.",
                None => "No stars yet.",
            });
        }
        for card in &page.repos {
            if !out.is_empty() {
                out.push('\n');
            }
            let _ = write!(out, "{}/{}", card.owner, card.name);
            if card.private {
                out.push_str(" (private)");
            }
            let _ = write!(
                out,
                ", {} {}",
                card.stars,
                if card.stars == 1 { "star" } else { "stars" }
            );
            if let Some(when) = &card.starred_at {
                let _ = write!(out, ", starred {when}");
            }
            if !card.description.is_empty() {
                let _ = write!(out, "\n    {}", card.description);
            }
        }
        if let Some(next) = &page.next {
            let _ = write!(out, "\nMore: braid hub stars");
            if let Some(login) = &user {
                let _ = write!(out, " --user {login}");
            }
            let _ = write!(out, " --after {next}");
        }
        out
    })?;
    Ok(ExitCode::SUCCESS)
}

/// How to watch a repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum WatchLevel {
    /// Everything that happens in it.
    All,
    /// What you take part in or are mentioned in.
    Participating,
    /// Nothing, not even what you take part in.
    Ignore,
}

impl WatchLevel {
    fn as_str(self) -> &'static str {
        match self {
            WatchLevel::All => braid_hubproto::social::ALL,
            WatchLevel::Participating => braid_hubproto::social::PARTICIPATING,
            WatchLevel::Ignore => braid_hubproto::social::IGNORE,
        }
    }
}

/// How `path` is watched at `level`, in words.
fn watched(path: &impl std::fmt::Display, level: &str) -> String {
    match level {
        braid_hubproto::social::ALL => format!("Watching all of {path}."),
        braid_hubproto::social::IGNORE => format!("Ignoring {path}: nothing of it notifies you."),
        _ => format!("Watching what you take part in on {path}."),
    }
}

/// `hub watch`.
#[derive(Debug, Args)]
pub(crate) struct WatchArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// What to watch: all of it, what you take part in, or nothing.
    #[arg(long, value_enum, default_value_t = WatchLevel::All)]
    pub(crate) level: WatchLevel,
}

pub(crate) fn watch(cli: &Cli, args: &WatchArgs) -> Result<ExitCode> {
    let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let body = json!({"level": args.level.as_str()});
    let route = format!("{}/subscription", path.api_path());
    let subscription: Subscription = match hub.call_json("PUT", &route, &[], Some(&body)) {
        Ok(subscription) => subscription,
        Err(err) => return Ok(failed(&err)),
    };
    let mut value = serde_json::to_value(&subscription)?;
    value["repo"] = json!(path.to_string());
    print(cli, &value, || watched(&path, &subscription.level))?;
    Ok(ExitCode::SUCCESS)
}

/// `hub unwatch`.
#[derive(Debug, Args)]
pub(crate) struct UnwatchArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
}

pub(crate) fn unwatch(cli: &Cli, args: &UnwatchArgs) -> Result<ExitCode> {
    let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let route = format!("{}/subscription", path.api_path());
    if let Err(err) = hub.call("DELETE", &route, &[], None) {
        return Ok(failed(&err));
    }
    let level = braid_hubproto::social::PARTICIPATING;
    print(
        cli,
        &json!({"repo": path.to_string(), "level": level}),
        || watched(&path, level),
    )?;
    Ok(ExitCode::SUCCESS)
}

/// `hub follow`.
#[derive(Debug, Args)]
pub(crate) struct FollowArgs {
    /// The account to follow.
    #[arg(value_name = "LOGIN")]
    pub(crate) login: String,
}

pub(crate) fn follow(cli: &Cli, args: &FollowArgs) -> Result<ExitCode> {
    follow_or_not(cli, &args.login, true)
}

/// `hub unfollow`.
#[derive(Debug, Args)]
pub(crate) struct UnfollowArgs {
    /// The account to stop following.
    #[arg(value_name = "LOGIN")]
    pub(crate) login: String,
}

pub(crate) fn unfollow(cli: &Cli, args: &UnfollowArgs) -> Result<ExitCode> {
    follow_or_not(cli, &args.login, false)
}

/// Follows `login`, or stops following it.
fn follow_or_not(cli: &Cli, login: &str, follow: bool) -> Result<ExitCode> {
    let login = login_arg(login)?;
    let hub = signed_in_hub()?;
    let method = if follow { "PUT" } else { "DELETE" };
    if let Err(err) = hub.call(method, &format!("/v1/users/{login}/follow"), &[], None) {
        return Ok(failed(&err));
    }
    print(cli, &json!({"login": login, "following": follow}), || {
        if follow {
            format!("Following {login}.")
        } else {
            format!("No longer following {login}.")
        }
    })?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_and_logins() {
        assert_eq!(query_value("s1700000000.42"), "s1700000000.42");
        assert_eq!(query_value("wada/demo"), "wada%2Fdemo");
        assert_eq!(query_value("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(login_arg("Ada").unwrap(), "ada");
        for bad in ["", "-ada", "ada-", "a/b", "a b"] {
            assert!(login_arg(bad).is_err(), "{bad:?}");
        }
        assert_eq!(WatchLevel::Participating.as_str(), "participating");
        assert_eq!(watched(&"ada/demo", "all"), "Watching all of ada/demo.");
    }
}
