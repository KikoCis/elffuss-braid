// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub log` and `diff`: the changesets of a branch on the hub, and
//! what one changed, read without cloning.
//!
//! Each takes the repository with `-R OWNER/NAME` (default: the one this
//! repository is linked to). A revision is a branch, `tags/<name>`, a
//! changeset's full id or its first 12 hex digits or more.

use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use braid_hubproto::RepoPath;
use braid_hubproto::history::{ChangesetSummary, DiffPage, Log};
use clap::Args;

use super::http::HttpHub;
use super::remote::{HubError, shown};
use super::social_common::{HubRepoArg, exit_for, hub_repo};
use crate::{Cli, print_json};

/// The most changesets one request asks for: a page of the hub's lists.
const PAGE: usize = 100;

/// The most pages of a diff `diff --json` reads.
const MAX_PAGES: u64 = 10_000;

/// `hub log`.
#[derive(Debug, Args)]
pub(crate) struct LogArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// The revision to start from, following first parents: a branch,
    /// tags/NAME, or a changeset's id or its start (default: the default
    /// branch).
    #[arg(long = "ref", value_name = "REV")]
    pub(crate) reference: Option<String>,
    /// How many changesets to show.
    #[arg(long, value_name = "N", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=100_000))]
    pub(crate) limit: u64,
}

/// `hub diff`.
#[derive(Debug, Args)]
pub(crate) struct DiffArgs {
    /// The changeset: a branch or tags/NAME for its head, or a changeset's
    /// id or its start. Its diff is against its first parent.
    #[arg(value_name = "REV")]
    pub(crate) rev: String,
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// Only the diff of this file, from the repository's root: for a diff
    /// larger than the hub gives whole.
    #[arg(long, value_name = "PATH")]
    pub(crate) path: Option<String>,
}

/// `text` percent-encoded, every byte but letters, digits and `-._~`: a
/// part of a path or a query that holds anything.
fn encoded(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Says on standard error why asking the hub failed, and gives the exit
/// status of [`exit_for`].
fn failed(err: &HubError) -> ExitCode {
    eprintln!("braid: error: {err}");
    ExitCode::from(exit_for(err))
}

/// A time of the hub's (RFC 3339) as `braid log` writes one:
/// `2026-10-18 12:00 +0200`.
fn date(time: &str) -> String {
    let (Some(day), Some(clock)) = (time.get(..10), time.get(11..16)) else {
        return shown(time);
    };
    let zone = match time.get(19..) {
        Some("Z") | None => "+0000".to_owned(),
        Some(zone) => zone.replace(':', ""),
    };
    shown(&format!("{day} {clock} {zone}"))
}

/// A changeset as `braid log` prints one: its short id, its author's date,
/// the author with the agent, and the summary.
fn line(entry: &ChangesetSummary) -> String {
    let agent = entry
        .agent
        .as_deref()
        .map_or_else(String::new, |agent| format!(" ({})", shown(agent)));
    format!(
        "{}  {}  {}{agent}  {}",
        entry.id.short(),
        date(&entry.author.time),
        shown(&entry.author.name),
        shown(&entry.summary)
    )
}

pub(crate) fn log(cli: &Cli, args: &LogArgs) -> Result<ExitCode> {
    let (hub, repo) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let limit = usize::try_from(args.limit).unwrap_or(usize::MAX);
    let log = match fetch_log(&hub, &repo, args.reference.as_deref(), limit) {
        Ok(log) => log,
        Err(err) => return Ok(failed(&err)),
    };
    if cli.json {
        print_json(&serde_json::to_value(&log)?)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut out = std::io::stdout().lock();
    for entry in &log.changesets {
        writeln!(out, "{}", line(entry))?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

/// At most `limit` changesets of the log of `reference`, page after page,
/// as one log: its `next` is where it would go on.
fn fetch_log(
    hub: &HttpHub,
    repo: &RepoPath,
    reference: Option<&str>,
    limit: usize,
) -> Result<Log, HubError> {
    log_pages(
        |route| hub.call_json("GET", route, &[], None),
        repo,
        reference,
        limit,
    )
}

/// [`fetch_log`], each page asked for with `get`.
fn log_pages(
    mut get: impl FnMut(&str) -> Result<Log, HubError>,
    repo: &RepoPath,
    reference: Option<&str>,
    limit: usize,
) -> Result<Log, HubError> {
    let mut log: Option<Log> = None;
    loop {
        let listed = log.as_ref().map_or(0, |log| log.changesets.len());
        let mut route = format!(
            "{}/log?limit={}",
            repo.api_path(),
            (limit - listed).min(PAGE)
        );
        if let Some(rev) = reference {
            route.push_str(&format!("&ref={}", encoded(rev)));
        }
        if let Some(after) = log.as_ref().and_then(|log| log.next.as_deref()) {
            route.push_str(&format!("&after={}", encoded(after)));
        }
        let page: Log = get(&route)?;
        // A page that brings nothing ends the log, whatever cursor it
        // gives: a hub that answers so would be asked for ever.
        let brought = page.changesets.len();
        let log = match &mut log {
            None => log.insert(page),
            Some(log) => {
                log.changesets.extend(page.changesets);
                log.next = page.next;
                log
            }
        };
        if log.changesets.len() > limit {
            log.changesets.truncate(limit);
        }
        if brought == 0 || log.next.is_none() || log.changesets.len() >= limit {
            break;
        }
    }
    Ok(log.unwrap_or_else(|| Log {
        reference: String::new(),
        changesets: Vec::new(),
        next: None,
    }))
}

pub(crate) fn diff(cli: &Cli, args: &DiffArgs) -> Result<ExitCode> {
    let (hub, repo) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let route = format!("{}/changesets/{}/diff", repo.api_path(), encoded(&args.rev));
    let path = args
        .path
        .as_deref()
        .map(|path| format!("path={}", encoded(path.trim_matches('/'))));
    if !cli.json {
        let query = path.map(|path| format!("?{path}")).unwrap_or_default();
        let asked = hub.call(
            "GET",
            &format!("{route}{query}"),
            &[("Accept", "text/x-diff")],
            None,
        );
        let text = match asked {
            Ok(text) => text,
            Err(err) => return Ok(failed(&err)),
        };
        let mut out = std::io::stdout().lock();
        out.write_all(&text)?;
        out.flush()?;
        return Ok(ExitCode::SUCCESS);
    }
    // Every page, as one.
    let mut all = DiffPage::default();
    let mut number = 1;
    while number <= MAX_PAGES {
        let mut query = format!("?page={number}");
        if let Some(path) = &path {
            query.push('&');
            query.push_str(path);
        }
        let page: DiffPage = match hub.call_json("GET", &format!("{route}{query}"), &[], None) {
            Ok(page) => page,
            Err(err) => return Ok(failed(&err)),
        };
        all.files.extend(page.files);
        match page.next_page {
            Some(next) if next > number => number = next,
            _ => break,
        }
    }
    print_json(&serde_json::to_value(&all)?)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_as_braid_log_writes_them() {
        assert_eq!(date("2023-11-14T22:13:20Z"), "2023-11-14 22:13 +0000");
        assert_eq!(date("2023-11-14T23:43:20+01:30"), "2023-11-14 23:43 +0130");
        assert_eq!(date("2023-11-14T17:13:20-05:00"), "2023-11-14 17:13 -0500");
        assert_eq!(date("soon"), "soon");
        assert_eq!(encoded("release/1.0 x"), "release%2F1.0%20x");
        assert_eq!(encoded("ñ"), "%C3%B1");
    }

    /// A changeset of a log page, by its number.
    fn entry(n: u8) -> ChangesetSummary {
        ChangesetSummary {
            id: braid_format::ObjectId::hash(braid_format::Kind::Changeset, &[n]),
            parents: Vec::new(),
            author: Default::default(),
            committer: Default::default(),
            summary: format!("change {n}"),
            agent: None,
        }
    }

    #[test]
    fn a_log_stops_whatever_the_hub_answers() {
        let repo: RepoPath = "ada/demo".parse().unwrap();
        // A hub that gives a next page and nothing on it, again and again:
        // the log ends instead of asking for ever.
        let mut asked = 0;
        let empty = log_pages(
            |_| {
                asked += 1;
                assert!(asked < 100, "the log asks again and again");
                Ok(Log {
                    reference: "main".into(),
                    changesets: Vec::new(),
                    next: Some("cursor".into()),
                })
            },
            &repo,
            None,
            30,
        )
        .unwrap();
        assert!(empty.changesets.is_empty());
        assert_eq!(asked, 1);
        // A hub that gives more than asked for: no more than the limit.
        let more = log_pages(
            |_| {
                Ok(Log {
                    reference: "main".into(),
                    changesets: (0..5).map(entry).collect(),
                    next: None,
                })
            },
            &repo,
            None,
            2,
        )
        .unwrap();
        assert_eq!(more.changesets, vec![entry(0), entry(1)]);
        // What the hub pages is read page after page, each from the cursor
        // the one before gave.
        let mut routes = Vec::new();
        let paged = log_pages(
            |route| {
                routes.push(route.to_owned());
                let page = routes.len() as u8;
                Ok(Log {
                    reference: "main".into(),
                    changesets: vec![entry(page)],
                    next: (page < 3).then(|| format!("c{page}")),
                })
            },
            &repo,
            Some("tags/v1"),
            10,
        )
        .unwrap();
        assert_eq!(paged.changesets, vec![entry(1), entry(2), entry(3)]);
        assert_eq!(paged.next, None);
        assert!(
            routes[0].ends_with("/log?limit=10&ref=tags%2Fv1"),
            "{routes:?}"
        );
        assert!(routes[2].ends_with("&after=c2"), "{routes:?}");
    }
}
