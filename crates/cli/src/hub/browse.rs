// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub show`, `ls` and `cat`: a repository on the hub, its
//! directories and files, read without cloning it.
//!
//! Each takes the repository with `-R OWNER/NAME` (default: the one this
//! repository is linked to) and the revision with `--ref` (default: the
//! default branch): a branch, `tags/<name>`, a changeset's full id or its
//! first 12 hex digits or more.

use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use braid_hubproto::RepoPath;
use braid_hubproto::api::RepoInfo;
use braid_hubproto::browse::{Contents, DirEntry, Directory, Readme, encode_path};
use clap::Args;
use serde_json::{Value, json};

use super::http::HttpHub;
use super::remote::{HubError, shown};
use super::social_common::{HubRepoArg, REFUSED, exit_for, hub_repo, print};
use crate::{Cli, print_json};

/// The most pages of one directory `ls` reads: a directory of a million
/// entries, at the hub's default page.
const MAX_PAGES: usize = 1_000;

/// `hub show`.
#[derive(Debug, Args)]
pub(crate) struct ShowArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// The revision whose README to show: a branch, tags/NAME, or a
    /// changeset's id or its start (default: the default branch).
    #[arg(long = "ref", value_name = "REV")]
    pub(crate) reference: Option<String>,
}

/// `hub ls`.
#[derive(Debug, Args)]
pub(crate) struct LsArgs {
    /// The directory, from the repository's root (default: the root).
    #[arg(value_name = "PATH")]
    pub(crate) path: Option<String>,
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// The revision: a branch, tags/NAME, or a changeset's id or its start
    /// (default: the default branch).
    #[arg(long = "ref", value_name = "REV")]
    pub(crate) reference: Option<String>,
}

/// `hub cat`.
#[derive(Debug, Args)]
pub(crate) struct CatArgs {
    /// The file, from the repository's root.
    #[arg(value_name = "PATH")]
    pub(crate) path: String,
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// The revision: a branch, tags/NAME, or a changeset's id or its start
    /// (default: the default branch).
    #[arg(long = "ref", value_name = "REV")]
    pub(crate) reference: Option<String>,
}

/// `value`, percent-encoded for a query string.
fn query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The query of a revision and more `pairs`, each value encoded: empty
/// when there is nothing to ask.
fn query(reference: Option<&str>, pairs: &[(&str, &str)]) -> String {
    let all: Vec<String> = reference
        .map(|rev| ("ref", rev))
        .into_iter()
        .chain(pairs.iter().copied())
        .map(|(name, value)| format!("{name}={}", query_value(value)))
        .collect();
    if all.is_empty() {
        String::new()
    } else {
        format!("?{}", all.join("&"))
    }
}

/// A path given on the command line, as a route's path takes it: `/` at
/// either end dropped, the rest encoded as it is, `%` included.
fn route_path(path: &str) -> String {
    encode_path(path.trim_matches('/').as_bytes())
}

/// Runs `work`, which asks the hub: a refusal or a failure is said on
/// standard error, with the exit status of [`exit_for`].
fn asking(work: impl FnOnce() -> Result<(), HubError>) -> Result<ExitCode> {
    match work() {
        Ok(()) => Ok(ExitCode::SUCCESS),
        // Standard output closed early: main says nothing of it.
        Err(HubError::Io(err)) => Err(err.into()),
        Err(err) => {
            eprintln!("braid: error: {err}");
            Ok(ExitCode::from(exit_for(&err)))
        }
    }
}

/// `GET …/contents/{path}`.
fn contents(
    hub: &HttpHub,
    repo: &RepoPath,
    path: &str,
    reference: Option<&str>,
    pairs: &[(&str, &str)],
) -> Result<Contents, HubError> {
    let route = if path.is_empty() {
        format!("{}/contents", repo.api_path())
    } else {
        format!("{}/contents/{path}", repo.api_path())
    };
    hub.call_json(
        "GET",
        &format!("{route}{}", query(reference, pairs)),
        &[],
        None,
    )
}

pub(crate) fn show(cli: &Cli, args: &ShowArgs) -> Result<ExitCode> {
    let (hub, repo) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let reference = args.reference.as_deref();
    asking(|| {
        let info: RepoInfo = hub.call_json("GET", &repo.api_path(), &[], None)?;
        let route = format!("{}/readme{}", repo.api_path(), query(reference, &[]));
        let readme = match hub.call_json::<Readme>("GET", &route, &[], None) {
            Ok(readme) => Some(readme),
            Err(HubError::Api { status: 404, .. }) => {
                // No README, or no such revision: the root says which.
                if reference.is_some() {
                    contents(&hub, &repo, "", reference, &[("text", "false")])?;
                }
                None
            }
            Err(err) => return Err(err),
        };
        let value = json!({ "repo": info, "readme": readme });
        print(cli, &value, || show_text(&info, readme.as_ref())).map_err(io_error)
    })
}

/// An error of printing, as the hub commands report it.
fn io_error(err: anyhow::Error) -> HubError {
    match err.downcast::<std::io::Error>() {
        Ok(err) => HubError::Io(err),
        Err(err) => HubError::Io(std::io::Error::other(err.to_string())),
    }
}

/// Text of several lines from the hub, as braid shows it: each line as
/// [`shown`] shows text, so that no control character reaches a terminal
/// raw, but its line breaks and tabs kept.
fn shown_lines(text: &str) -> String {
    text.split('\n')
        .map(|line| line.split('\t').map(shown).collect::<Vec<_>>().join("\t"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What `show` prints without `--json`. What the hub says goes through
/// [`shown`]: a repository's names, description and README are anyone's
/// text.
fn show_text(info: &RepoInfo, readme: Option<&Readme>) -> String {
    let mut out = format!(
        "{}/{} ({})\n",
        shown(&info.owner),
        shown(&info.name),
        if info.private { "private" } else { "public" }
    );
    if !info.description.is_empty() {
        out.push_str(&format!("{}\n", shown(&info.description)));
    }
    out.push_str(&format!(
        "Default branch: {}\n",
        shown(&info.default_branch)
    ));
    if let Some(counts) = &info.counts {
        out.push_str(&format!(
            "Stars: {} · Forks: {} · Watchers: {} · Open issues: {} · Open requests: {}\n",
            counts.stars, counts.forks, counts.watchers, counts.open_issues, counts.open_requests
        ));
    }
    if !info.topics.is_empty() {
        out.push_str(&format!("Topics: {}\n", shown(&info.topics.join(", "))));
    }
    if let Some(upstream) = &info.fork_of {
        out.push_str(&format!(
            "Forked from: {}/{}\n",
            shown(&upstream.owner),
            shown(&upstream.name)
        ));
    }
    if let Some(mirror) = &info.mirror {
        out.push_str(&format!(
            "Mirror: {} ({})\n",
            shown(&mirror.url),
            shown(mirror.outcome.as_deref().unwrap_or("not synced yet"))
        ));
    }
    out.push_str(&format!(
        "Clone: braid hub clone {}/{}\n",
        shown(&info.owner),
        shown(&info.name)
    ));
    match readme {
        Some(readme) => {
            out.push_str(&format!(
                "\n{}:\n\n{}",
                shown(&readme.path),
                shown_lines(&readme.markdown)
            ));
            if readme.truncated {
                out.push_str("\n[The README goes on: the hub shows only its start.]");
            }
        }
        None => out.push_str("\nNo README."),
    }
    out.trim_end().to_owned()
}

pub(crate) fn ls(cli: &Cli, args: &LsArgs) -> Result<ExitCode> {
    let (hub, repo) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let path = route_path(args.path.as_deref().unwrap_or(""));
    let reference = args.reference.as_deref();
    asking(|| {
        let first = contents(&hub, &repo, &path, reference, &[("text", "false")])?;
        let found = match first {
            Contents::Dir(dir) => Contents::Dir(whole_dir(&hub, &repo, &path, dir)?),
            file => file,
        };
        let value =
            serde_json::to_value(&found).map_err(|err| HubError::Protocol(err.to_string()))?;
        print(cli, &value, || ls_text(&found)).map_err(io_error)
    })
}

/// `dir`, the first page of a directory, with the entries of every page
/// after it.
fn whole_dir(
    hub: &HttpHub,
    repo: &RepoPath,
    path: &str,
    mut dir: Directory,
) -> Result<Directory, HubError> {
    // The pages after the first ask for the changeset the first one read,
    // so that a landing meanwhile does not mix two revisions.
    let at = dir.changeset.to_hex();
    for _ in 0..MAX_PAGES {
        let Some(after) = dir.next.take() else {
            return Ok(dir);
        };
        let page = contents(hub, repo, path, Some(&at), &[("after", &after)])?;
        let Contents::Dir(page) = page else {
            return Err(HubError::Protocol(
                "a directory's next page is not a directory".to_owned(),
            ));
        };
        dir.entries.extend(page.entries);
        dir.next = page.next;
    }
    Err(HubError::Protocol(format!(
        "the directory has more than {MAX_PAGES} pages: list a directory below it, or use braid hub api"
    )))
}

/// One line of `ls`: kind, size and name, a directory's name ending in `/`.
/// The name goes through [`shown`]: it may hold any byte but `/` and NUL,
/// and a line break in it would print an entry the directory does not
/// hold.
fn entry_line(entry: &DirEntry) -> String {
    let size = entry
        .size
        .map_or_else(|| "-".to_owned(), |size| size.to_string());
    let slash = if entry.kind == "tree" { "/" } else { "" };
    format!(
        "{:<10} {:>10}  {}{slash}",
        shown(&entry.kind),
        size,
        shown(&entry.name)
    )
}

/// What `ls` prints without `--json`.
fn ls_text(found: &Contents) -> String {
    match found {
        Contents::Dir(dir) => dir
            .entries
            .iter()
            .map(entry_line)
            .collect::<Vec<_>>()
            .join("\n"),
        Contents::File(file) => entry_line(&DirEntry {
            name: file.path.clone(),
            raw_name: None,
            kind: file.kind.clone(),
            id: file.id.clone(),
            size: file.size,
        }),
    }
}

pub(crate) fn cat(cli: &Cli, args: &CatArgs) -> Result<ExitCode> {
    let (hub, repo) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let path = route_path(&args.path);
    if path.is_empty() {
        anyhow::bail!("give the path of a file; braid hub ls lists a directory");
    }
    let reference = args.reference.as_deref();
    if cli.json {
        let found = match contents(&hub, &repo, &path, reference, &[]) {
            Ok(Contents::Dir(_)) => return Ok(a_directory(&args.path)),
            other => other,
        };
        return asking(|| {
            let value: Value =
                serde_json::to_value(found?).map_err(|err| HubError::Protocol(err.to_string()))?;
            print_json(&value).map_err(io_error)
        });
    }
    let route = format!("{}/raw/{path}{}", repo.api_path(), query(reference, &[]));
    let mut out = std::io::stdout().lock();
    let sent = hub.get_to(&route, &mut out);
    // A directory has no bytes: say so, as ls would show it.
    if let Err(HubError::Api { status: 404, .. }) = &sent
        && let Ok(Contents::Dir(_)) = contents(&hub, &repo, &path, reference, &[("text", "false")])
    {
        return Ok(a_directory(&args.path));
    }
    asking(|| {
        sent?;
        out.flush().map_err(HubError::Io)
    })
}

/// What `cat` says of a directory, and its exit status: refused.
fn a_directory(path: &str) -> ExitCode {
    eprintln!("braid: error: {path:?} is a directory: braid hub ls lists it");
    ExitCode::from(REFUSED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_and_queries_are_encoded() {
        assert_eq!(route_path("/src/main.rs/"), "src/main.rs");
        assert_eq!(route_path("a b/100%.txt"), "a%20b/100%25.txt");
        assert_eq!(query(None, &[]), "");
        assert_eq!(query(Some("release/1.0"), &[]), "?ref=release/1.0");
        assert_eq!(
            query(Some("a&b c+d"), &[("text", "false")]),
            "?ref=a%26b%20c%2Bd&text=false"
        );
        let line = entry_line(&DirEntry {
            name: "src".into(),
            raw_name: None,
            kind: "tree".into(),
            id: String::new(),
            size: None,
        });
        assert_eq!(line, "tree                -  src/");
        let odd = entry_line(&DirEntry {
            name: "a\nfile 1  b\x1b[2J".into(),
            raw_name: None,
            kind: "file".into(),
            id: String::new(),
            size: Some(1),
        });
        assert_eq!(odd, r"file                1  a\nfile 1  b\u{1b}[2J");
        assert_eq!(
            shown_lines("\x1b[1m# T\r\n\tx\x07\n"),
            "\\u{1b}[1m# T\\r\n\tx\\u{7}\n"
        );
    }
}
