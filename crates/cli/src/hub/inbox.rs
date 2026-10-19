// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub inbox`: the notifications of the account signed in, from
//! section 21 of `docs/hub.md`.
//!
//! Listing never marks anything: `inbox read` marks notifications read,
//! `inbox done` takes one out of the inbox, and `inbox mute` and `unmute`
//! stop or bring back the notifications of one thread, named
//! `OWNER/NAME#NUMBER`. The inbox is where notifications are: the hub sends
//! no e-mail. With `--json` each prints what the hub answered. Exit
//! statuses as the other hub commands: 0 done, 1 refused, 3 when the hub
//! could not be reached or failed.
//!
//! Titles and logins are anyone's words: printed without `--json`, they go
//! through [`shown`], so that no control character of theirs reaches a
//! terminal raw.

use std::fmt::Write;
use std::process::ExitCode;

use anyhow::{Context, Result};
use braid_hubproto::RepoPath;
use braid_hubproto::notifications::{Marked, Notification, Notifications, ThreadSubscription};
use clap::{Args, Subcommand};
use serde_json::json;

use super::creds::{self, HubEnv};
use super::http::HttpHub;
use super::remote::{HubError, shown};
use super::social_common::{exit_for, print};
use crate::Cli;

/// `hub inbox`.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct InboxArgs {
    #[command(subcommand)]
    pub(crate) action: Option<InboxAction>,
    /// Show the notifications read too (default: the unread ones).
    #[arg(long)]
    pub(crate) all: bool,
    /// Only the notifications of this repository.
    #[arg(short = 'R', long = "hub-repo", value_name = "OWNER/NAME")]
    pub(crate) hub_repo: Option<String>,
    /// The most notifications to list, 1 to 100 (default: 30).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=100))]
    pub(crate) limit: Option<u32>,
    /// Where to start: the `next` of the page before.
    #[arg(long, value_name = "CURSOR")]
    pub(crate) after: Option<String>,
}

/// What `hub inbox` does besides listing.
#[derive(Debug, Subcommand)]
pub(crate) enum InboxAction {
    /// Mark notifications read, by id, or all of them.
    Read(ReadArgs),
    /// Take a notification out of the inbox.
    Done(DoneArgs),
    /// Stop the notifications of a thread.
    Mute(ThreadArgs),
    /// Get the notifications of a thread again.
    Unmute(ThreadArgs),
}

/// `hub inbox read`.
#[derive(Debug, Args)]
pub(crate) struct ReadArgs {
    /// The notifications to mark read, by id.
    #[arg(
        value_name = "ID",
        required_unless_present = "all",
        conflicts_with = "all"
    )]
    pub(crate) ids: Vec<u64>,
    /// Mark every notification read.
    #[arg(long)]
    pub(crate) all: bool,
}

/// `hub inbox done`.
#[derive(Debug, Args)]
pub(crate) struct DoneArgs {
    /// The notification, by id.
    #[arg(value_name = "ID")]
    pub(crate) id: u64,
}

/// `hub inbox mute` and `unmute`.
#[derive(Debug, Args)]
pub(crate) struct ThreadArgs {
    /// The thread: OWNER/NAME#NUMBER.
    #[arg(value_name = "OWNER/NAME#NUMBER")]
    pub(crate) thread: String,
}

pub(crate) fn inbox(cli: &Cli, args: &InboxArgs) -> Result<ExitCode> {
    match &args.action {
        None => list(cli, args),
        Some(InboxAction::Read(read)) => mark(cli, read),
        Some(InboxAction::Done(done)) => finish(cli, done),
        Some(InboxAction::Mute(thread)) => subscribe(cli, thread, false),
        Some(InboxAction::Unmute(thread)) => subscribe(cli, thread, true),
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

/// A repository given on the command line: OWNER/NAME.
fn repo_arg(text: &str) -> Result<RepoPath> {
    text.parse()
        .with_context(|| format!("{text:?} is not OWNER/NAME"))
}

/// A thread given on the command line: OWNER/NAME#NUMBER.
fn thread_arg(text: &str) -> Result<(RepoPath, u64)> {
    let parsed = text.split_once('#').and_then(|(repo, number)| {
        let number = number.parse::<u64>().ok().filter(|number| *number > 0)?;
        Some((repo.parse::<RepoPath>().ok()?, number))
    });
    parsed.with_context(|| format!("{text:?} is not a thread: give OWNER/NAME#NUMBER"))
}

/// A notification in a line: `*` when unread, its id, where, its words,
/// and who did what, why and when.
fn line(notification: &Notification) -> String {
    let mut out = format!(
        "{} {}",
        if notification.unread { "*" } else { " " },
        notification.id
    );
    if let Some(repo) = &notification.repo {
        let _ = write!(out, " {}/{}", shown(&repo.owner), shown(&repo.name));
    }
    match &notification.thread {
        Some(thread) => {
            let _ = write!(out, "#{} {}", thread.number, shown(&thread.title));
        }
        None => {
            let _ = write!(
                out,
                " {}",
                shown(notification.code.as_deref().unwrap_or("notice"))
            );
        }
    }
    let mut about = Vec::new();
    if notification.thread.is_some() {
        let what = shown(notification.code.as_deref().unwrap_or(""));
        about.push(match &notification.actor {
            Some(actor) => format!("{} {what}", shown(actor)),
            None => what,
        });
    }
    about.push(shown(&notification.reason));
    about.push(shown(&notification.updated));
    let _ = write!(out, " ({})", about.join(", "));
    out
}

/// `hub inbox [--all] [-R OWNER/NAME] [--limit N] [--after CURSOR]`.
fn list(cli: &Cli, args: &InboxArgs) -> Result<ExitCode> {
    let repo = args.hub_repo.as_deref().map(repo_arg).transpose()?;
    let mut query = Vec::new();
    if args.all {
        query.push("all=true".to_owned());
    }
    if let Some(repo) = &repo {
        query.push(format!("repo={}", query_value(&repo.to_string())));
    }
    if let Some(limit) = args.limit {
        query.push(format!("limit={limit}"));
    }
    if let Some(after) = &args.after {
        query.push(format!("after={}", query_value(after)));
    }
    let mut route = "/v1/notifications".to_owned();
    if !query.is_empty() {
        route = format!("{route}?{}", query.join("&"));
    }
    let hub = signed_in_hub()?;
    let page: Notifications = match hub.call_json("GET", &route, &[], None) {
        Ok(page) => page,
        Err(err) => return Ok(failed(&err)),
    };
    print(cli, &serde_json::to_value(&page)?, || {
        let mut out = String::new();
        if page.notifications.is_empty() {
            out.push_str(if args.all {
                "No notifications."
            } else {
                "No unread notifications."
            });
        }
        for notification in &page.notifications {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&line(notification));
        }
        let _ = write!(out, "\n{} unread.", page.unread);
        if let Some(next) = &page.next {
            let _ = write!(out, "\nMore: braid hub inbox");
            if args.all {
                out.push_str(" --all");
            }
            if let Some(repo) = &repo {
                let _ = write!(out, " -R {repo}");
            }
            let _ = write!(out, " --after {}", shown(next));
        }
        out
    })?;
    Ok(ExitCode::SUCCESS)
}

/// `hub inbox read ID... | --all`.
fn mark(cli: &Cli, args: &ReadArgs) -> Result<ExitCode> {
    let body = if args.all {
        json!({"all": true})
    } else {
        json!({"ids": args.ids})
    };
    let hub = signed_in_hub()?;
    let marked: Marked = match hub.call_json("POST", "/v1/notifications/read", &[], Some(&body)) {
        Ok(marked) => marked,
        Err(err) => return Ok(failed(&err)),
    };
    print(cli, &serde_json::to_value(marked)?, || {
        match marked.marked {
            0 => "Nothing to mark: no unread notification among those.".to_owned(),
            1 => "Marked 1 notification read.".to_owned(),
            n => format!("Marked {n} notifications read."),
        }
    })?;
    Ok(ExitCode::SUCCESS)
}

/// `hub inbox done ID`.
fn finish(cli: &Cli, args: &DoneArgs) -> Result<ExitCode> {
    let hub = signed_in_hub()?;
    let route = format!("/v1/notifications/{}", args.id);
    if let Err(err) = hub.call("DELETE", &route, &[], None) {
        return Ok(failed(&err));
    }
    print(cli, &json!({"id": args.id, "done": true}), || {
        format!("Notification {} is done.", args.id)
    })?;
    Ok(ExitCode::SUCCESS)
}

/// `hub inbox mute|unmute OWNER/NAME#NUMBER`.
fn subscribe(cli: &Cli, args: &ThreadArgs, subscribed: bool) -> Result<ExitCode> {
    let (repo, number) = thread_arg(&args.thread)?;
    let hub = signed_in_hub()?;
    let route = format!("{}/threads/{number}/subscription", repo.api_path());
    let body = json!({ "subscribed": subscribed });
    let answer: ThreadSubscription = match hub.call_json("PUT", &route, &[], Some(&body)) {
        Ok(answer) => answer,
        Err(err) => return Ok(failed(&err)),
    };
    let mut value = serde_json::to_value(&answer)?;
    value["repo"] = json!(repo.to_string());
    value["number"] = json!(number);
    print(cli, &value, || {
        if answer.subscribed {
            format!("Following {repo}#{number}: it notifies you again.")
        } else if subscribed {
            // Asked to follow it, but the repository is ignored.
            format!(
                "{repo}#{number} still notifies you of nothing: you ignore its repository (braid hub watch changes that)."
            )
        } else {
            format!("Muted {repo}#{number}: it notifies you no more.")
        }
    })?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_hubproto::cards::RepoRef;
    use braid_hubproto::notifications::NotificationThread;

    #[test]
    fn threads_and_lines_as_given() {
        let (repo, number) = thread_arg("ada/demo#4").unwrap();
        assert_eq!((repo.to_string().as_str(), number), ("ada/demo", 4));
        for bad in [
            "ada/demo",
            "ada/demo#",
            "ada/demo#0",
            "ada#4",
            "ada/demo#x",
            "#4",
        ] {
            assert!(thread_arg(bad).is_err(), "{bad}");
        }
        assert!(repo_arg("ada/demo").is_ok() && repo_arg("ada").is_err());
        assert_eq!(query_value("ada/demo"), "ada%2Fdemo");
        let notification = Notification {
            id: 7,
            repo: Some(RepoRef {
                owner: "ada".into(),
                name: "demo".into(),
            }),
            thread: Some(NotificationThread {
                kind: "issue".into(),
                number: 4,
                title: "a\u{1b}[31m title".into(),
            }),
            reason: "mention".into(),
            actor: Some("bob".into()),
            code: Some("commented".into()),
            updated: "2026-10-18T12:00:00Z".into(),
            unread: true,
            url: String::new(),
        };
        let shown = line(&notification);
        assert!(
            shown.starts_with("* 7 ada/demo#4 a\\u{1b}[31m title (bob commented, mention, "),
            "{shown}"
        );
        assert!(!shown.contains('\u{1b}'));
    }
}
