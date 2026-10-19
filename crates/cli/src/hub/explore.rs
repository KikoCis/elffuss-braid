// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub explore` and `user`: finding public repositories, and an
//! account's profile.
//!
//! Each is one request of section 16 of `docs/hub.md` (section 17 for an
//! account's stars and follows) to the hub signed in to: BRAID_HUB_URL,
//! else the hub signed in to last, else the public hub. Neither needs a
//! sign-in; with a token, an account's private repositories the token may
//! read are counted and listed as well. Exit statuses as the other hub
//! commands: 0 done, 1 refused, 3 when the hub could not be reached or
//! failed.

use std::fmt::Write;
use std::process::ExitCode;

use anyhow::{Result, bail};
use braid_hubproto::cards::{RepoCard, RepoCards, UserCards};
use braid_hubproto::explore::{self as proto, Profile};
use braid_hubproto::names::is_owner;
use clap::{Args, ValueEnum};

use super::creds::{self, HubEnv};
use super::http::HttpHub;
use super::remote::{HubError, shown};
use super::social_common::{exit_for, print};
use crate::Cli;

/// The hub signed in to: BRAID_HUB_URL, else the hub signed in to last,
/// else the public hub; with the token saved for it, if any.
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

/// `text` as a value of a query string: letters, digits, `-._~` and `/` as
/// they are, and every other byte percent-encoded.
fn query_value(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// `route` with the query string of `pairs`, those with a value.
fn with_query(route: &str, pairs: &[(&str, Option<String>)]) -> String {
    let mut out = route.to_owned();
    for (name, value) in pairs {
        if let Some(value) = value {
            out.push(if out.contains('?') { '&' } else { '?' });
            let _ = write!(out, "{name}={}", query_value(value));
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

/// A count of things, as words: `1 star`, `3 stars`.
fn counted(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Repositories, a few lines each: the name, whether private, where it was
/// forked from, its stars and last landing, its description and topics;
/// `empty` when there are none, and how to ask for the next page. What the
/// hub says goes through [`shown`]: names, descriptions and topics are
/// anyone's text, and no control character of theirs reaches a terminal.
fn cards_text(cards: &RepoCards, empty: &str, more: &str) -> String {
    let mut out = String::new();
    if cards.repos.is_empty() {
        out.push_str(empty);
    }
    for card in &cards.repos {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&card_line(card));
        if !card.description.is_empty() {
            let _ = write!(out, "\n    {}", shown(&card.description));
        }
        if !card.topics.is_empty() {
            let _ = write!(out, "\n    Topics: {}", shown(&card.topics.join(", ")));
        }
    }
    if let Some(next) = &cards.next {
        let _ = write!(out, "\nMore: {more} --after {}", shown(next));
    }
    out
}

/// The first line of a repository: `owner/name (private, fork of x/y), 3
/// stars, updated 2026-10-18`.
fn card_line(card: &RepoCard) -> String {
    let mut out = format!("{}/{}", shown(&card.owner), shown(&card.name));
    let mut notes = Vec::new();
    if card.private {
        notes.push("private".to_owned());
    }
    if let Some(upstream) = &card.fork_of {
        notes.push(format!(
            "fork of {}/{}",
            shown(&upstream.owner),
            shown(&upstream.name)
        ));
    }
    if !notes.is_empty() {
        let _ = write!(out, " ({})", notes.join(", "));
    }
    let _ = write!(out, ", {}", counted(card.stars, "star", "stars"));
    if let Some(pushed) = &card.pushed {
        let _ = write!(
            out,
            ", updated {}",
            shown(pushed.get(..10).unwrap_or(pushed))
        );
    }
    out
}

/// The order of explore and search.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ExploreSort {
    /// Best match first: the default with a query.
    Match,
    /// The latest landing first: the default without one.
    Recent,
    /// The most stars first.
    Stars,
    /// The newest first.
    New,
}

impl ExploreSort {
    fn as_str(self) -> &'static str {
        match self {
            ExploreSort::Match => proto::MATCH,
            ExploreSort::Recent => proto::RECENT,
            ExploreSort::Stars => proto::STARS,
            ExploreSort::New => proto::NEW,
        }
    }
}

/// `hub explore`.
#[derive(Debug, Args)]
pub(crate) struct ExploreArgs {
    /// What to look for: words that start words of a repository's owner,
    /// name, description or topics; every one must match.
    #[arg(value_name = "QUERY")]
    pub(crate) query: Vec<String>,
    /// Only the repositories with this topic.
    #[arg(long, value_name = "TOPIC")]
    pub(crate) topic: Option<String>,
    /// Only the repositories of this account.
    #[arg(long, value_name = "LOGIN")]
    pub(crate) owner: Option<String>,
    /// The order (default: match with a query, else recent).
    #[arg(long, value_enum)]
    pub(crate) sort: Option<ExploreSort>,
    /// List forks too.
    #[arg(long)]
    pub(crate) forks: bool,
    /// The most repositories to list, 1 to 100 (default: 30).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=100))]
    pub(crate) limit: Option<u32>,
    /// Where to start: the `next` of the page before.
    #[arg(long, value_name = "CURSOR")]
    pub(crate) after: Option<String>,
}

pub(crate) fn explore(cli: &Cli, args: &ExploreArgs) -> Result<ExitCode> {
    let query = args.query.join(" ");
    if let Err(err) = proto::terms(&query) {
        bail!("{err}");
    }
    let owner = args.owner.as_deref().map(login_arg).transpose()?;
    let route = with_query(
        "/v1/explore",
        &[
            ("q", (!query.is_empty()).then(|| query.clone())),
            ("topic", args.topic.clone()),
            ("owner", owner),
            ("sort", args.sort.map(|sort| sort.as_str().to_owned())),
            ("forks", args.forks.then(|| "1".to_owned())),
            ("limit", args.limit.map(|limit| limit.to_string())),
            ("after", args.after.clone()),
        ],
    );
    let hub = signed_in_hub()?;
    let page: RepoCards = match hub.call_json("GET", &route, &[], None) {
        Ok(page) => page,
        Err(err) => return Ok(failed(&err)),
    };
    print(cli, &serde_json::to_value(&page)?, || {
        cards_text(
            &page,
            "No public repository matches.",
            "the same command with",
        )
    })?;
    Ok(ExitCode::SUCCESS)
}

/// `hub user`.
#[derive(Debug, Args)]
pub(crate) struct UserArgs {
    /// The account.
    #[arg(value_name = "LOGIN")]
    pub(crate) login: String,
    /// List its repositories you may read.
    #[arg(long, group = "list")]
    pub(crate) repos: bool,
    /// List what it starred that you may read.
    #[arg(long, group = "list")]
    pub(crate) stars: bool,
    /// List the accounts that follow it.
    #[arg(long, group = "list")]
    pub(crate) followers: bool,
    /// List the accounts it follows.
    #[arg(long, group = "list")]
    pub(crate) following: bool,
    /// The most entries of a list, 1 to 100 (default: 30).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=100))]
    pub(crate) limit: Option<u32>,
    /// Where a list starts: the `next` of the page before.
    #[arg(long, value_name = "CURSOR")]
    pub(crate) after: Option<String>,
}

/// What `hub user` shows of an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    Profile,
    Repos,
    Stars,
    Followers,
    Following,
}

impl UserArgs {
    fn shown(&self) -> Shown {
        if self.repos {
            Shown::Repos
        } else if self.stars {
            Shown::Stars
        } else if self.followers {
            Shown::Followers
        } else if self.following {
            Shown::Following
        } else {
            Shown::Profile
        }
    }
}

pub(crate) fn user(cli: &Cli, args: &UserArgs) -> Result<ExitCode> {
    let login = login_arg(&args.login)?;
    let shown = args.shown();
    let page = [
        ("limit", args.limit.map(|limit| limit.to_string())),
        ("after", args.after.clone()),
    ];
    let route = match shown {
        Shown::Profile => format!("/v1/users/{login}"),
        Shown::Repos => with_query(&format!("/v1/users/{login}/repos"), &page),
        Shown::Stars => with_query(&format!("/v1/users/{login}/starred"), &page),
        Shown::Followers => with_query(&format!("/v1/users/{login}/followers"), &page),
        Shown::Following => with_query(&format!("/v1/users/{login}/following"), &page),
    };
    let hub = signed_in_hub()?;
    let answer: serde_json::Value = match hub.call_json("GET", &route, &[], None) {
        Ok(answer) => answer,
        Err(err) => return Ok(failed(&err)),
    };
    let flag = match shown {
        Shown::Profile => "",
        Shown::Repos => " --repos",
        Shown::Stars => " --stars",
        Shown::Followers => " --followers",
        Shown::Following => " --following",
    };
    let more = format!("braid hub user {login}{flag}");
    let text = match shown {
        Shown::Profile => profile_text(&serde_json::from_value(answer.clone())?),
        Shown::Repos => cards_text(
            &serde_json::from_value(answer.clone())?,
            "No repositories you may read.",
            &more,
        ),
        Shown::Stars => cards_text(
            &serde_json::from_value(answer.clone())?,
            "No stars of repositories you may read.",
            &more,
        ),
        Shown::Followers => users_text(
            &serde_json::from_value(answer.clone())?,
            "No followers.",
            &more,
        ),
        Shown::Following => users_text(
            &serde_json::from_value(answer.clone())?,
            "Follows nobody.",
            &more,
        ),
    };
    print(cli, &answer, || text)?;
    Ok(ExitCode::SUCCESS)
}

/// An account in words: its login and name, when it joined, its counts,
/// and whether you follow it; what the hub says through [`shown`].
fn profile_text(profile: &Profile) -> String {
    let login = shown(&profile.login);
    let mut out = login.clone();
    if let Some(name) = profile.name.as_deref().filter(|name| !name.is_empty()) {
        let _ = write!(out, " ({})", shown(name));
    }
    let _ = write!(
        out,
        "\nJoined {}",
        shown(profile.created.get(..10).unwrap_or(&profile.created))
    );
    let stars = match profile.stars {
        Some(stars) => counted(stars, "star", "stars"),
        None => "stars kept private".to_owned(),
    };
    let _ = write!(
        out,
        "\n{}, {stars}, {}, {} following",
        counted(profile.repos, "repository", "repositories"),
        counted(profile.followers, "follower", "followers"),
        profile.following
    );
    if profile.viewer.as_ref().is_some_and(|viewer| viewer.follows) {
        let _ = write!(out, "\nYou follow {login}.");
    }
    out
}

/// Accounts, one a line, with their names and since when; what the hub
/// says through [`shown`].
fn users_text(users: &UserCards, empty: &str, more: &str) -> String {
    let mut out = String::new();
    if users.users.is_empty() {
        out.push_str(empty);
    }
    for user in &users.users {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&shown(&user.login));
        if let Some(name) = user.name.as_deref().filter(|name| !name.is_empty()) {
            let _ = write!(out, " ({})", shown(name));
        }
        let _ = write!(
            out,
            ", since {}",
            shown(user.since.get(..10).unwrap_or(&user.since))
        );
    }
    if let Some(next) = &users.next {
        let _ = write!(out, "\nMore: {more} --after {}", shown(next));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_hubproto::cards::RepoRef;
    use braid_hubproto::explore::ProfileViewer;

    #[test]
    fn routes_carry_their_query() {
        assert_eq!(query_value("ada/demo x&y"), "ada/demo%20x%26y");
        assert_eq!(
            with_query(
                "/v1/explore",
                &[
                    ("q", Some("rust cli".into())),
                    ("topic", None),
                    ("forks", Some("1".into()))
                ]
            ),
            "/v1/explore?q=rust%20cli&forks=1"
        );
        assert_eq!(with_query("/v1/explore", &[("q", None)]), "/v1/explore");
        assert_eq!(login_arg("Ada").unwrap(), "ada");
        assert!(login_arg("a--b").is_err());
    }

    #[test]
    fn repositories_and_accounts_in_words() {
        let card = RepoCard {
            owner: "bob".into(),
            name: "tool".into(),
            private: true,
            description: "A tool".into(),
            topics: vec!["rust".into(), "cli".into()],
            stars: 1,
            created: "2026-01-01T00:00:00Z".into(),
            pushed: Some("2026-10-18T09:00:00Z".into()),
            default_branch: "main".into(),
            fork_of: Some(RepoRef {
                owner: "ada".into(),
                name: "tool".into(),
            }),
            ..RepoCard::default()
        };
        let text = cards_text(
            &RepoCards {
                repos: vec![card],
                next: Some("r2.x".into()),
            },
            "none",
            "braid hub user bob --repos",
        );
        assert_eq!(
            text,
            "bob/tool (private, fork of ada/tool), 1 star, updated 2026-10-18\n    A tool\n    Topics: rust, cli\nMore: braid hub user bob --repos --after r2.x"
        );
        let profile = Profile {
            login: "ada".into(),
            name: Some("Ada Lovelace".into()),
            created: "2026-10-01T00:00:00Z".into(),
            followers: 1,
            following: 2,
            repos: 3,
            stars: None,
            viewer: Some(ProfileViewer { follows: true }),
        };
        assert_eq!(
            profile_text(&profile),
            "ada (Ada Lovelace)\nJoined 2026-10-01\n3 repositories, stars kept private, 1 follower, 2 following\nYou follow ada."
        );
        assert_eq!(
            users_text(&UserCards::default(), "No followers.", "x"),
            "No followers."
        );
    }

    #[test]
    fn what_the_hub_says_drives_no_terminal() {
        // What a hub not to be trusted could send in any field: a title for
        // the terminal, its bell, and a clear screen.
        let evil = "\u{1b}]0;owned\u{7}\u{1b}[2J";
        let bad = |text: &str| format!("{text}{evil}");
        let card = RepoCard {
            owner: bad("bob"),
            name: bad("tool"),
            description: bad("A tool\nMore: run this"),
            topics: vec![bad("rust")],
            pushed: Some(bad("2026")),
            fork_of: Some(RepoRef {
                owner: bad("ada"),
                name: bad("tool"),
            }),
            ..RepoCard::default()
        };
        let cards = cards_text(
            &RepoCards {
                repos: vec![card],
                next: Some(bad("r2")),
            },
            "none",
            "braid hub explore",
        );
        let profile = profile_text(&Profile {
            login: bad("ada"),
            name: Some(bad("Ada")),
            created: bad("2026"),
            viewer: Some(ProfileViewer { follows: true }),
            ..Profile::default()
        });
        let users = users_text(
            &UserCards {
                users: vec![braid_hubproto::cards::UserCard {
                    login: bad("bob"),
                    name: Some(bad("Bob")),
                    since: bad("2026"),
                }],
                next: Some(bad("f1")),
            },
            "none",
            "braid hub user ada --followers",
        );
        for text in [&cards, &profile, &users] {
            assert!(!text.contains(['\u{1b}', '\u{7}']), "{text:?}");
            assert!(text.contains("\\u{1b}]0;owned\\u{7}\\u{1b}[2J"), "{text:?}");
        }
        // A line break in a description prints no line of braid's.
        assert_eq!(
            cards
                .lines()
                .filter(|line| line.starts_with("More:"))
                .count(),
            1
        );
        assert!(cards.contains("A tool\\nMore: run this"), "{cards:?}");
    }
}
