// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub issue` and `label`: a repository's issues and labels.
//!
//! Each is a request of section 18 of `docs/hub.md`. The repository is
//! `-R OWNER/NAME`, else the one this repository is linked to. A text comes
//! from `--body`, or from `--body-file` (`-` reads standard input), at most
//! 64 KiB. Opening an issue and commenting send an `Idempotency-Key`, and
//! are sent once more with the same key when the answer was lost, so that
//! they are never done twice. With `--json` each prints what the hub
//! answered. Exit statuses as the other hub commands: 0 done, 1 refused,
//! 3 when the hub could not be reached or failed.
//!
//! Titles, texts, comments, logins and labels are anyone's words: printed
//! without `--json`, they go through [`shown`], so that no control
//! character of theirs reaches a terminal raw.

use std::fmt::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use braid_hubproto::RepoPath;
use braid_hubproto::issues::{Comment, Comments, Issue, Issues, Label, Labels};
use clap::{Args, Subcommand, ValueEnum};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::http::HttpHub;
use super::remote::{HubError, shown};
use super::social_common::{
    HubRepoArg, agent, body_text, exit_for, hub_repo, idempotency_key, print,
};
use crate::Cli;

/// A text of several lines from the hub, as braid prints it: each line as
/// [`shown`] shows text, its line breaks and tabs kept.
fn shown_lines(text: &str) -> String {
    text.split('\n')
        .map(|line| line.split('\t').map(shown).collect::<Vec<_>>().join("\t"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a command answers when the hub refused it or could not be
/// reached: the error, and the exit status it calls for.
fn failed(err: &HubError) -> ExitCode {
    eprintln!("braid: error: {err}");
    ExitCode::from(exit_for(err))
}

/// `text` as a value of a query string, or a segment of a path:
/// everything but letters, digits and `-._~` percent-encoded.
fn encoded(text: &str) -> String {
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

/// Sends a request that creates something, with an `Idempotency-Key`, and
/// sends it once more with the same key when the answer was lost: the hub
/// then gives the first answer again rather than doing it twice.
fn create<T: DeserializeOwned>(hub: &HttpHub, route: &str, body: &Value) -> Result<T, HubError> {
    let key = idempotency_key();
    let headers = [("Idempotency-Key", key.as_str())];
    match hub.call_json("POST", route, &headers, Some(body)) {
        Err(err) if err.may_have_happened() => hub.call_json("POST", route, &headers, Some(body)),
        answer => answer,
    }
}

/// An issue in a line: its number, title, labels and state.
fn line(issue: &Issue) -> String {
    let mut out = format!("#{} {}", issue.number, shown(&issue.title));
    if !issue.labels.is_empty() {
        let _ = write!(out, " [{}]", shown(&issue.labels.join(", ")));
    }
    if issue.state != braid_hubproto::issues::OPEN {
        out.push_str(" (closed)");
    }
    out
}

/// `hub issue`.
#[derive(Debug, Args)]
pub(crate) struct IssueArgs {
    #[command(subcommand)]
    pub(crate) action: IssueAction,
}

/// What `hub issue` does.
#[derive(Debug, Subcommand)]
pub(crate) enum IssueAction {
    /// List a repository's issues, newest first.
    List(ListArgs),
    /// Show an issue and its comments.
    Show(ShowArgs),
    /// Open an issue.
    Create(CreateArgs),
    /// Comment on an issue.
    Comment(CommentArgs),
    /// Close an issue.
    Close(CloseArgs),
    /// Open a closed issue again.
    Reopen(NumberArgs),
    /// Change an issue: its title, text, labels, assignees or lock.
    Edit(EditArgs),
}

/// Which issues `issue list` shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum StateArg {
    /// Those open.
    Open,
    /// Those closed.
    Closed,
    /// Both.
    All,
}

impl StateArg {
    fn as_str(self) -> &'static str {
        match self {
            StateArg::Open => "open",
            StateArg::Closed => "closed",
            StateArg::All => "all",
        }
    }
}

/// What `issue list` sorts by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum SortArg {
    /// When each was opened.
    Created,
    /// When each last changed.
    Updated,
    /// How many comments each has.
    Comments,
}

impl SortArg {
    fn as_str(self) -> &'static str {
        match self {
            SortArg::Created => "created",
            SortArg::Updated => "updated",
            SortArg::Comments => "comments",
        }
    }
}

/// `hub issue list`.
#[derive(Debug, Args)]
pub(crate) struct ListArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// Which issues, by state.
    #[arg(long, value_enum, default_value_t = StateArg::Open)]
    pub(crate) state: StateArg,
    /// Only those with this label; given again, with every one of them.
    #[arg(long = "label", value_name = "LABEL")]
    pub(crate) labels: Vec<String>,
    /// Only those assigned to this account (`none` for nobody, `*` for
    /// anybody).
    #[arg(long, value_name = "LOGIN")]
    pub(crate) assignee: Option<String>,
    /// Only those this account opened.
    #[arg(long, value_name = "LOGIN")]
    pub(crate) author: Option<String>,
    /// Only those whose title holds this text: public repositories only.
    #[arg(long, value_name = "TEXT")]
    pub(crate) search: Option<String>,
    /// What sorts them, the newest or the most first.
    #[arg(long, value_enum, default_value_t = SortArg::Created)]
    pub(crate) sort: SortArg,
    /// The most issues to list, 1 to 100 (default: 30).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=100))]
    pub(crate) limit: Option<u32>,
    /// Where to start: the `next` of the page before.
    #[arg(long, value_name = "CURSOR")]
    pub(crate) after: Option<String>,
}

/// `hub issue show`.
#[derive(Debug, Args)]
pub(crate) struct ShowArgs {
    /// The issue's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
}

/// A text, from the command line or a file.
#[derive(Debug, Args)]
pub(crate) struct BodyArgs {
    /// The text, Markdown.
    #[arg(long, value_name = "TEXT")]
    pub(crate) body: Option<String>,
    /// Read the text from this file; `-` reads standard input.
    #[arg(long, value_name = "FILE")]
    pub(crate) body_file: Option<PathBuf>,
}

impl BodyArgs {
    fn text(&self) -> Result<Option<String>> {
        body_text(self.body.as_deref(), self.body_file.as_deref())
    }
}

/// `hub issue create`.
#[derive(Debug, Args)]
pub(crate) struct CreateArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// Its title, one line.
    #[arg(long, value_name = "TITLE")]
    pub(crate) title: String,
    #[command(flatten)]
    pub(crate) body: BodyArgs,
    /// A label to give it, by its name; given again, more (the
    /// repository's writers only).
    #[arg(long = "label", value_name = "LABEL")]
    pub(crate) labels: Vec<String>,
    /// An account to assign it to; given again, more (the repository's
    /// writers only).
    #[arg(long = "assignee", value_name = "LOGIN")]
    pub(crate) assignees: Vec<String>,
}

/// `hub issue comment`.
#[derive(Debug, Args)]
pub(crate) struct CommentArgs {
    /// The issue's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    #[command(flatten)]
    pub(crate) body: BodyArgs,
}

/// What a closed issue was closed as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ReasonArg {
    /// Done.
    Completed,
    /// Not to be done.
    NotPlanned,
}

impl ReasonArg {
    fn as_str(self) -> &'static str {
        match self {
            ReasonArg::Completed => braid_hubproto::issues::COMPLETED,
            ReasonArg::NotPlanned => braid_hubproto::issues::NOT_PLANNED,
        }
    }
}

/// `hub issue close`.
#[derive(Debug, Args)]
pub(crate) struct CloseArgs {
    /// The issue's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// What it is closed as.
    #[arg(long, value_enum, default_value_t = ReasonArg::Completed)]
    pub(crate) reason: ReasonArg,
}

/// `hub issue reopen`.
#[derive(Debug, Args)]
pub(crate) struct NumberArgs {
    /// The issue's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
}

/// `hub issue edit`.
#[derive(Debug, Args)]
pub(crate) struct EditArgs {
    /// The issue's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// A new title.
    #[arg(long, value_name = "TITLE")]
    pub(crate) title: Option<String>,
    #[command(flatten)]
    pub(crate) body: BodyArgs,
    /// A label to add; given again, more.
    #[arg(long = "add-label", value_name = "LABEL")]
    pub(crate) add_labels: Vec<String>,
    /// A label to take off; given again, more.
    #[arg(long = "remove-label", value_name = "LABEL")]
    pub(crate) remove_labels: Vec<String>,
    /// An account to assign it to; given again, more.
    #[arg(long = "add-assignee", value_name = "LOGIN")]
    pub(crate) add_assignees: Vec<String>,
    /// An account to take off it; given again, more.
    #[arg(long = "remove-assignee", value_name = "LOGIN")]
    pub(crate) remove_assignees: Vec<String>,
    /// Let only the repository's writers comment on it.
    #[arg(long, conflicts_with = "unlock")]
    pub(crate) lock: bool,
    /// Let everyone who may comment do so again.
    #[arg(long)]
    pub(crate) unlock: bool,
}

pub(crate) fn issue(cli: &Cli, args: &IssueArgs) -> Result<ExitCode> {
    match &args.action {
        IssueAction::List(args) => list(cli, args),
        IssueAction::Show(args) => show(cli, args),
        IssueAction::Create(args) => create_issue(cli, args),
        IssueAction::Comment(args) => comment(cli, args),
        IssueAction::Close(args) => {
            let body = json!({"state": "closed", "state_reason": args.reason.as_str()});
            change(cli, &args.repo, args.number, &body, |path, issue| {
                format!(
                    "Closed {path}#{} as {}.",
                    issue.number,
                    args.reason.as_str().replace('_', " ")
                )
            })
        }
        IssueAction::Reopen(args) => change(
            cli,
            &args.repo,
            args.number,
            &json!({"state": "open"}),
            |path, issue| format!("Reopened {path}#{}.", issue.number),
        ),
        IssueAction::Edit(args) => edit(cli, args),
    }
}

/// The route of the issue `number` of `path`.
fn issue_route(path: &RepoPath, number: u64) -> String {
    format!("{}/issues/{number}", path.api_path())
}

fn list(cli: &Cli, args: &ListArgs) -> Result<ExitCode> {
    let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let mut query = vec![
        format!("state={}", args.state.as_str()),
        format!("sort={}", args.sort.as_str()),
    ];
    if !args.labels.is_empty() {
        query.push(format!("labels={}", encoded(&args.labels.join(","))));
    }
    for (name, value) in [
        ("assignee", &args.assignee),
        ("author", &args.author),
        ("q", &args.search),
        ("after", &args.after),
    ] {
        if let Some(value) = value {
            query.push(format!("{name}={}", encoded(value)));
        }
    }
    if let Some(limit) = args.limit {
        query.push(format!("limit={limit}"));
    }
    let route = format!("{}/issues?{}", path.api_path(), query.join("&"));
    let page: Issues = match hub.call_json("GET", &route, &[], None) {
        Ok(page) => page,
        Err(err) => return Ok(failed(&err)),
    };
    print(cli, &serde_json::to_value(&page)?, || {
        let mut out = String::new();
        if page.issues.is_empty() {
            out.push_str("No issues.");
        }
        for issue in &page.issues {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&line(issue));
            let _ = write!(out, ", by {}", shown(&issue.author));
            if issue.comments > 0 {
                let noun = if issue.comments == 1 {
                    "comment"
                } else {
                    "comments"
                };
                let _ = write!(out, ", {} {noun}", issue.comments);
            }
        }
        if let Some(next) = &page.next {
            let _ = write!(
                out,
                "\nMore: braid hub issue list -R {path} --state {} --after {}",
                args.state.as_str(),
                shown(next)
            );
        }
        out
    })?;
    Ok(ExitCode::SUCCESS)
}

fn show(cli: &Cli, args: &ShowArgs) -> Result<ExitCode> {
    let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let route = issue_route(&path, args.number);
    let mut issue: Issue = match hub.call_json("GET", &route, &[], None) {
        Ok(issue) => issue,
        Err(err) => return Ok(failed(&err)),
    };
    let comments: Comments =
        match hub.call_json("GET", &format!("{route}/comments?limit=100"), &[], None) {
            Ok(comments) => comments,
            Err(err) => return Ok(failed(&err)),
        };
    issue.html = None;
    let value = json!({"issue": issue, "comments": comments.comments, "next": comments.next});
    print(cli, &value, || {
        let mut out = format!("{path}{}\n", line(&issue));
        let _ = writeln!(
            out,
            "Opened by {} on {}",
            shown(&issue.author),
            shown(&issue.created)
        );
        if !issue.assignees.is_empty() {
            let _ = writeln!(out, "Assigned to {}", shown(&issue.assignees.join(", ")));
        }
        if issue.locked {
            out.push_str("Locked: only the repository's writers comment on it.\n");
        }
        if !issue.body.trim().is_empty() {
            let _ = write!(out, "\n{}\n", shown_lines(issue.body.trim_end()));
        }
        for comment in &comments.comments {
            let _ = write!(
                out,
                "\n--- {} on {}",
                shown(&comment.author),
                shown(&comment.created)
            );
            match &comment.hidden {
                Some(reason) if comment.body.is_empty() => {
                    let _ = write!(out, " (hidden: {})", shown(reason));
                }
                _ => {
                    let _ = write!(out, "\n{}", shown_lines(comment.body.trim_end()));
                }
            }
            out.push('\n');
        }
        if let Some(next) = &comments.next {
            let _ = write!(
                out,
                "\nMore comments: braid hub api GET '{route}/comments?after={}'",
                shown(next)
            );
        }
        out.trim_end().to_owned()
    })?;
    Ok(ExitCode::SUCCESS)
}

fn create_issue(cli: &Cli, args: &CreateArgs) -> Result<ExitCode> {
    let body = args.body.text()?;
    let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let mut request = json!({ "title": args.title });
    if let Some(body) = body {
        request["body"] = json!(body);
    }
    if !args.labels.is_empty() {
        request["labels"] = json!(args.labels);
    }
    if !args.assignees.is_empty() {
        request["assignees"] = json!(args.assignees);
    }
    if let Some(agent) = agent(cli) {
        request["agent"] = json!(agent);
    }
    let mut issue: Issue = match create(&hub, &format!("{}/issues", path.api_path()), &request) {
        Ok(issue) => issue,
        Err(err) => return Ok(failed(&err)),
    };
    issue.html = None;
    print(cli, &serde_json::to_value(&issue)?, || {
        format!("Opened {path}#{}: {}", issue.number, shown(&issue.title))
    })?;
    Ok(ExitCode::SUCCESS)
}

fn comment(cli: &Cli, args: &CommentArgs) -> Result<ExitCode> {
    let Some(body) = args.body.text()? else {
        bail!("a comment needs its text: give --body, or --body-file (- reads standard input)");
    };
    let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let mut request = json!({ "body": body });
    if let Some(agent) = agent(cli) {
        request["agent"] = json!(agent);
    }
    let route = format!("{}/comments", issue_route(&path, args.number));
    let mut said: Comment = match create(&hub, &route, &request) {
        Ok(said) => said,
        Err(err) => return Ok(failed(&err)),
    };
    said.html = None;
    print(cli, &serde_json::to_value(&said)?, || {
        format!("Commented on {path}#{}.", args.number)
    })?;
    Ok(ExitCode::SUCCESS)
}

/// Changes the issue `number` as `body` says, and prints it.
fn change(
    cli: &Cli,
    repo: &HubRepoArg,
    number: u64,
    body: &Value,
    text: impl FnOnce(&RepoPath, &Issue) -> String,
) -> Result<ExitCode> {
    let (hub, path) = hub_repo(cli, repo.hub_repo.as_deref())?;
    let mut issue: Issue =
        match hub.call_json("PATCH", &issue_route(&path, number), &[], Some(body)) {
            Ok(issue) => issue,
            Err(err) => return Ok(failed(&err)),
        };
    issue.html = None;
    print(cli, &serde_json::to_value(&issue)?, || text(&path, &issue))?;
    Ok(ExitCode::SUCCESS)
}

/// `held`, with `added` and without `removed`, compared without regard to
/// case.
fn changed_list(held: &[String], added: &[String], removed: &[String]) -> Vec<String> {
    let mut list: Vec<String> = held
        .iter()
        .filter(|item| !removed.iter().any(|gone| gone.eq_ignore_ascii_case(item)))
        .cloned()
        .collect();
    for item in added {
        if !list.iter().any(|kept| kept.eq_ignore_ascii_case(item)) {
            list.push(item.clone());
        }
    }
    list
}

fn edit(cli: &Cli, args: &EditArgs) -> Result<ExitCode> {
    let body = args.body.text()?;
    let mut request = json!({});
    if let Some(title) = &args.title {
        request["title"] = json!(title);
    }
    if let Some(body) = body {
        request["body"] = json!(body);
    }
    if args.lock || args.unlock {
        request["locked"] = json!(args.lock);
    }
    let labels = !args.add_labels.is_empty() || !args.remove_labels.is_empty();
    let assignees = !args.add_assignees.is_empty() || !args.remove_assignees.is_empty();
    if labels || assignees {
        // The hub takes the whole lists: the issue's, as they are now,
        // changed as asked.
        let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
        let issue: Issue = match hub.call_json("GET", &issue_route(&path, args.number), &[], None) {
            Ok(issue) => issue,
            Err(err) => return Ok(failed(&err)),
        };
        if labels {
            request["labels"] = json!(changed_list(
                &issue.labels,
                &args.add_labels,
                &args.remove_labels
            ));
        }
        if assignees {
            request["assignees"] = json!(changed_list(
                &issue.assignees,
                &args.add_assignees,
                &args.remove_assignees
            ));
        }
    }
    if request.as_object().is_some_and(serde_json::Map::is_empty) {
        bail!(
            "nothing to change: give --title, --body, --add-label, --remove-label, --add-assignee, --remove-assignee, --lock or --unlock"
        );
    }
    change(cli, &args.repo, args.number, &request, |path, issue| {
        format!("Changed {path}#{}.", issue.number)
    })
}

/// `hub label`.
#[derive(Debug, Args)]
pub(crate) struct LabelArgs {
    #[command(subcommand)]
    pub(crate) action: LabelAction,
}

/// What `hub label` does.
#[derive(Debug, Subcommand)]
pub(crate) enum LabelAction {
    /// List a repository's labels.
    List(LabelListArgs),
    /// Make a label, or change one of that name.
    Create(LabelCreateArgs),
    /// Delete a label, and take it off every issue.
    Delete(LabelDeleteArgs),
}

/// `hub label list`.
#[derive(Debug, Args)]
pub(crate) struct LabelListArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
}

/// `hub label create`.
#[derive(Debug, Args)]
pub(crate) struct LabelCreateArgs {
    /// Its name: 1 to 50 characters, without commas.
    #[arg(value_name = "NAME")]
    pub(crate) name: String,
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// Its colour in the hub's palette, 0 to 11.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u8).range(0..=11))]
    pub(crate) color: Option<u8>,
    /// What it means.
    #[arg(long, value_name = "TEXT")]
    pub(crate) description: Option<String>,
}

/// `hub label delete`.
#[derive(Debug, Args)]
pub(crate) struct LabelDeleteArgs {
    /// Its name.
    #[arg(value_name = "NAME")]
    pub(crate) name: String,
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
}

pub(crate) fn label(cli: &Cli, args: &LabelArgs) -> Result<ExitCode> {
    match &args.action {
        LabelAction::List(args) => {
            let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
            let route = format!("{}/labels", path.api_path());
            let labels: Labels = match hub.call_json("GET", &route, &[], None) {
                Ok(labels) => labels,
                Err(err) => return Ok(failed(&err)),
            };
            print(cli, &serde_json::to_value(&labels)?, || {
                let lines: Vec<String> = labels
                    .labels
                    .iter()
                    .map(|label| {
                        if label.description.is_empty() {
                            format!("{} (colour {})", shown(&label.name), label.color)
                        } else {
                            format!(
                                "{} (colour {}): {}",
                                shown(&label.name),
                                label.color,
                                shown(&label.description)
                            )
                        }
                    })
                    .collect();
                if lines.is_empty() {
                    "No labels.".to_owned()
                } else {
                    lines.join("\n")
                }
            })?;
            Ok(ExitCode::SUCCESS)
        }
        LabelAction::Create(args) => {
            let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
            let mut request = json!({});
            if let Some(color) = args.color {
                request["color"] = json!(color);
            }
            if let Some(description) = &args.description {
                request["description"] = json!(description);
            }
            let route = format!("{}/labels/{}", path.api_path(), encoded(&args.name));
            let label: Label = match hub.call_json("PUT", &route, &[], Some(&request)) {
                Ok(label) => label,
                Err(err) => return Ok(failed(&err)),
            };
            print(cli, &serde_json::to_value(&label)?, || {
                format!(
                    "The label {:?} of {path} has colour {}.",
                    label.name, label.color
                )
            })?;
            Ok(ExitCode::SUCCESS)
        }
        LabelAction::Delete(args) => {
            let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
            let route = format!("{}/labels/{}", path.api_path(), encoded(&args.name));
            if let Err(err) = hub.call("DELETE", &route, &[], None) {
                return Ok(failed(&err));
            }
            print(
                cli,
                &json!({"repo": path.to_string(), "label": args.name, "deleted": true}),
                || format!("Deleted the label {:?} of {path}.", args.name),
            )?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_and_lists() {
        assert_eq!(encoded("good first issue"), "good%20first%20issue");
        assert_eq!(encoded("a,b/c"), "a%2Cb%2Fc");
        assert_eq!(encoded("icd17.4"), "icd17.4");
        let held = vec!["bug".to_owned(), "question".to_owned()];
        assert_eq!(
            changed_list(
                &held,
                &["help wanted".into(), "BUG".into()],
                &["Question".into()]
            ),
            ["bug", "help wanted"]
        );
        let issue = Issue {
            number: 4,
            title: "Crash".into(),
            labels: vec!["bug".into()],
            state: "closed".into(),
            ..Issue::default()
        };
        assert_eq!(line(&issue), "#4 Crash [bug] (closed)");
    }
}
