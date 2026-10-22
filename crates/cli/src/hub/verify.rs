// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub verify`: keeping a repository's default branch green, as
//! section 25 of `docs/hub.md` says.
//!
//! - `status` shows how the branch stands; with `--fail-on-red` it exits 4
//!   while the branch is red or stuck.
//! - `report` tells the hub how a suite went on a changeset: what a CI job
//!   runs, with a token that only reads and posts verdicts.
//! - `policy` shows who reports on which suite, and changes it: a read, a
//!   merge of what the flags name, and a `PUT` of the whole policy, which
//!   keeps every field braid does not know as the hub gave it.
//! - `log` shows what verification did; `reset` moves green, or dismisses
//!   the fails there are now.
//!
//! Each acts on `-R OWNER/NAME`, else on the repository this one is linked
//! to, and asks `GET /v1` first: a hub that does not list the `verify`
//! feature is called for nothing else. `--json` prints what the hub
//! answered, whole. `report` and `reset` send an `Idempotency-Key`, and
//! send the request once more with the same key when the answer was lost.
//! Exit statuses: 0 done, 1 refused, 3 when the hub could not be reached or
//! failed, 4 for `status --fail-on-red` on a red branch.

use std::fmt::Write as _;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use braid_format::ObjectId;
use braid_hubproto::RepoPath;
use braid_hubproto::names::is_owner;
use braid_hubproto::verify::{
    FEATURE, MAX_REPORTERS, MODE_OFF, MODE_REPORT, SOURCE_REPORT, STATE_OFF, STATE_RED,
    STATE_STUCK, VerifyEvents, VerifyPolicy, VerifyReport, VerifyReportAnswer, VerifyReset,
    VerifyStatus, is_git_commit, is_suite,
};
use clap::{ArgGroup, Args, Subcommand, ValueEnum};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::remote::{HubError, shown};
use super::social_common::{DONE, HubRepoArg, exit_for, hub_repo, idempotency_key, print};
use crate::work::{count, short};
use crate::{Cli, open_repo, resolve_changeset};

/// The exit status of `status --fail-on-red` on a branch that is red or
/// stuck, as `braid verify` exits on a red branch.
pub(crate) const RED: u8 = 4;

/// The long help of `hub verify`.
pub(crate) const VERIFY_HELP: &str =
    "Keep a repository's default branch green, as section 25 of the hub's
protocol says. A CI job reports how a suite of tests went on a changeset
of the branch; the hub keeps braid/green/<branch> at the newest changeset
with a pass it accepts and, while the branch is red, braid/red/<branch> at
the changeset it blames, or at the first fail it cannot explain. It names
a culprit only when a single changeset separates a pass from a fail. The
hub runs no code of the repository: every verdict comes from a reporter.

  status   how the branch stands; --fail-on-red exits 4 while it is red
  report   how a suite went on a changeset, as a CI job says it
  policy   who reports, on which suite; its admins change it
  log      what verification did, newest first
  reset    move green, or dismiss the fails there are now (admins)

The repository is -R OWNER/NAME, else the one this repository is linked
to. A hub whose GET /v1 does not list the verify feature is asked nothing
else. With --json each command prints what the hub answered.

Exit statuses: 0 done; 1 refused, or the hub does not verify branches; 3
the hub could not be reached, or failed; 4 for status --fail-on-red on a
red or stuck branch. report and reset send an Idempotency-Key, and send
the request once more with the same key when the answer was lost.";

/// The long help of `hub verify status`.
pub(crate) const STATUS_HELP: &str =
    "Show how the repository's default branch stands on the hub: its state (off,
unknown, green, pending, red or stuck, and any other a newer hub gives),
why (its code), its head, green and red, the culprit and how it was named,
since when it is red, and the window the culprit is looked for in. The
repository's admins also see who landed the culprit.

With --fail-on-red, the exit status is 4 while the branch is red or stuck,
so that a script stops before it builds on a broken branch; it is 0 for
every other state. With --json it prints the status as the hub answered
it.";

/// The long help of `hub verify report`.
pub(crate) const REPORT_HELP: &str =
    "Tell the hub how a suite of tests went on a changeset of the default
branch, as a CI job does once it ran them: --outcome pass, fail, or flaky
(it failed, then passed when run again). Name the changeset by the git
commit the mirror holds (--git-commit, 40 hex digits, such as the commit a
GitHub workflow ran on), or by its id (--changeset, 64 hex digits). --suite
is the suite the repository's policy names.

The token is one made for this alone: read and verify:<branch glob> on the
repository and nothing more, of an account that may write to it and that
the policy names as a reporter. A CI job passes it in BRAID_TOKEN, and the
hub in BRAID_HUB_URL, from its secrets; it never signs in with `braid
login`.

It prints what the report did (green_moved, red_set, red_cleared,
recorded, duplicate, or stale for a changeset at or below green) and how
the branch stands. The report goes with an Idempotency-Key, and once more
with the same key when the answer was lost, so that it counts once. With
--json it prints {\"effect\", \"status\"} as the hub answered.";

/// The long help of `hub verify policy`.
pub(crate) const POLICY_HELP: &str =
    "Show how the hub verifies the repository's default branch, or change it:
its admins only. With no flag it prints the policy. With flags it reads
the policy, changes what the flags name, keeps everything else as the hub
gave it, and puts the whole of it back.

  --mode off|report    report takes the verdicts of the reporters; off
                       stops verifying, and the hub forgets green and red
  --suite SUITE        the suite reported on: 1 to 32 of a-z 0-9 . _ -,
                       starting with a letter or a digit
  --reporter LOGIN     an account whose reports count, in place of those
                       the policy names; give it again for each, up to 10
  --green-from SOURCE  a source whose passes move green, in place of those
                       the policy names: report, the default when the
                       policy names none

With --json it prints the policy as the hub answered it.";

/// The long help of `hub verify log`.
pub(crate) const LOG_HELP: &str =
    "Show what verification did on the repository, newest first: each event's
id, when, what (policy, green, red, culprit, fixed, stuck, reset or flaky),
the changeset it is about, what goes with it and, to the repository's
admins, who did it. --limit takes 1 to 100 (30 by default), and --after ID
shows the events before that one: the next of the page before. With
--json it prints the page as the hub answered it, with next.";

/// The long help of `hub verify reset`.
pub(crate) const RESET_HELP: &str =
    "Its admins only. With --green REV, move green to that changeset of the
default branch, backward too: it must be on the first-parent chain of the
branch's head. Without --green, dismiss the fails there are now: they no
longer count, and only a new one turns the branch red again. Either way
red is cleared, and the log records who did it. REV is a changeset's id
or, in a repository that holds it, any revision braid resolves there.

The reset goes with an Idempotency-Key, and once more with the same key
when the answer was lost. With --json it prints the status as the hub
answered it.";

/// The routes of section 25 for one repository: over HTTP for the
/// commands, in memory for their tests. Each answer comes as the hub gave
/// it, so that `--json` prints it whole.
pub(crate) trait VerifyApi {
    /// `GET /v1`: the features the hub lists.
    fn features(&self) -> Result<Vec<String>, HubError>;

    /// `GET …/verify`.
    fn status(&self) -> Result<Value, HubError>;

    /// `POST …/verify/report`, with the `Idempotency-Key` `key`.
    fn report(&self, report: &VerifyReport, key: &str) -> Result<Value, HubError>;

    /// `GET …/verify/policy`.
    fn policy(&self) -> Result<Value, HubError>;

    /// `PUT …/verify/policy`: the whole policy.
    fn put_policy(&self, policy: &Value) -> Result<Value, HubError>;

    /// `GET …/verify/log?after&limit`.
    fn log(&self, after: Option<u64>, limit: Option<u32>) -> Result<Value, HubError>;

    /// `POST …/verify/reset`, with the `Idempotency-Key` `key`.
    fn reset(&self, reset: &VerifyReset, key: &str) -> Result<Value, HubError>;
}

/// `hub verify`.
#[derive(Debug, Args)]
pub(crate) struct VerifyArgs {
    #[command(subcommand)]
    pub(crate) action: VerifyAction,
}

/// What `hub verify` does.
#[derive(Debug, Subcommand)]
pub(crate) enum VerifyAction {
    /// Show how the default branch stands: green, red and the culprit.
    #[command(long_about = STATUS_HELP)]
    Status(StatusArgs),
    /// Tell the hub how a suite went on a changeset, as a CI job does.
    #[command(long_about = REPORT_HELP)]
    Report(ReportArgs),
    /// Show who reports on which suite, or change it (its admins).
    #[command(long_about = POLICY_HELP)]
    Policy(PolicyArgs),
    /// Show what verification did, newest first.
    #[command(long_about = LOG_HELP)]
    Log(LogArgs),
    /// Move green, or dismiss the fails there are now (its admins).
    #[command(long_about = RESET_HELP)]
    Reset(ResetArgs),
}

/// `hub verify status`.
#[derive(Debug, Args)]
pub(crate) struct StatusArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// Exit with status 4 while the branch is red or stuck.
    #[arg(long)]
    pub(crate) fail_on_red: bool,
}

/// How a suite went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Outcome {
    /// It passed.
    Pass,
    /// It failed.
    Fail,
    /// It failed, then passed when run again.
    Flaky,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => braid_hubproto::verify::PASS,
            Outcome::Fail => braid_hubproto::verify::FAIL,
            Outcome::Flaky => braid_hubproto::verify::FLAKY,
        }
    }
}

/// `hub verify report`.
#[derive(Debug, Args)]
#[command(group(ArgGroup::new("tested").required(true).args(["git_commit", "changeset"])))]
pub(crate) struct ReportArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// How the suite went.
    #[arg(long, value_enum)]
    pub(crate) outcome: Outcome,
    /// The git commit tested, as the mirror holds it: 40 hex digits.
    #[arg(long, value_name = "SHA")]
    pub(crate) git_commit: Option<String>,
    /// The changeset tested: 64 hex digits.
    #[arg(long, value_name = "ID")]
    pub(crate) changeset: Option<String>,
    /// The suite, as the repository's policy names it.
    #[arg(long, value_name = "SUITE")]
    pub(crate) suite: String,
}

/// A policy's mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Mode {
    /// Verify nothing.
    Off,
    /// Take the verdicts of the reporters.
    Report,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Off => MODE_OFF,
            Mode::Report => MODE_REPORT,
        }
    }
}

/// A source of verdicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Source {
    /// The verdicts the reporters post.
    Report,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Report => SOURCE_REPORT,
        }
    }
}

/// `hub verify policy`.
#[derive(Debug, Args)]
pub(crate) struct PolicyArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// Take the reporters' verdicts (report), or verify nothing (off).
    #[arg(long, value_enum)]
    pub(crate) mode: Option<Mode>,
    /// The suite the reporters report on.
    #[arg(long, value_name = "SUITE")]
    pub(crate) suite: Option<String>,
    /// An account whose reports count, in place of those named before;
    /// give it again for each, up to 10.
    #[arg(long = "reporter", value_name = "LOGIN")]
    pub(crate) reporters: Vec<String>,
    /// A source whose passes move green, in place of those named before.
    #[arg(long = "green-from", value_enum, value_name = "SOURCE")]
    pub(crate) green_from: Vec<Source>,
}

/// `hub verify log`.
#[derive(Debug, Args)]
pub(crate) struct LogArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// The most events to show, 1 to 100 (default: 30).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=100))]
    pub(crate) limit: Option<u32>,
    /// Show the events before this one: the next of the page before.
    #[arg(long, value_name = "ID")]
    pub(crate) after: Option<u64>,
}

/// `hub verify reset`.
#[derive(Debug, Args)]
pub(crate) struct ResetArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// Move green to this changeset, backward too (default: dismiss the
    /// fails there are now).
    #[arg(long, value_name = "REV")]
    pub(crate) green: Option<String>,
}

impl VerifyAction {
    /// The repository `-R` names, if it names one.
    fn hub_repo(&self) -> Option<&str> {
        match self {
            VerifyAction::Status(args) => args.repo.hub_repo.as_deref(),
            VerifyAction::Report(args) => args.repo.hub_repo.as_deref(),
            VerifyAction::Policy(args) => args.repo.hub_repo.as_deref(),
            VerifyAction::Log(args) => args.repo.hub_repo.as_deref(),
            VerifyAction::Reset(args) => args.repo.hub_repo.as_deref(),
        }
    }
}

/// What a command asks of the hub, its arguments checked, so that nothing
/// the hub would refuse for them goes out.
#[derive(Debug)]
enum Asked {
    Status {
        fail_on_red: bool,
    },
    Report(VerifyReport),
    Policy(PolicyChange),
    Log {
        after: Option<u64>,
        limit: Option<u32>,
    },
    Reset(VerifyReset),
}

/// What `policy` changes: the fields its flags name, checked.
#[derive(Debug, Default, PartialEq, Eq)]
struct PolicyChange {
    mode: Option<&'static str>,
    suite: Option<String>,
    reporters: Option<Vec<String>>,
    green_from: Option<Vec<String>>,
}

impl PolicyChange {
    fn of(args: &PolicyArgs) -> Result<PolicyChange> {
        let suite = args.suite.as_deref().map(suite_arg).transpose()?;
        let reporters = if args.reporters.is_empty() {
            None
        } else {
            let mut logins: Vec<String> = Vec::new();
            for login in &args.reporters {
                if !is_owner(login) {
                    bail!(
                        "{login:?} is not a login: 1 to 39 letters, digits and hyphens, with no hyphen at either end and no two in a row"
                    );
                }
                let login = login.to_ascii_lowercase();
                if !logins.contains(&login) {
                    logins.push(login);
                }
            }
            if logins.len() > MAX_REPORTERS {
                bail!("a policy names at most {MAX_REPORTERS} reporters");
            }
            Some(logins)
        };
        let green_from = (!args.green_from.is_empty()).then(|| {
            let mut sources: Vec<String> = Vec::new();
            for source in &args.green_from {
                if !sources.iter().any(|known| known == source.as_str()) {
                    sources.push(source.as_str().to_owned());
                }
            }
            sources
        });
        Ok(PolicyChange {
            mode: args.mode.map(Mode::as_str),
            suite,
            reporters,
            green_from,
        })
    }

    /// Whether the flags name nothing: the policy is only read.
    fn is_empty(&self) -> bool {
        *self == PolicyChange::default()
    }

    /// `current`, the policy as the hub gave it, with the change made, and
    /// everything else kept as it was, fields braid does not know
    /// included. A policy that names no source takes `report` when it has
    /// reporters. A policy in `report` mode without a suite or reporters is
    /// refused here, as the hub would refuse it.
    fn merged(&self, current: &Value) -> Result<Value> {
        let Value::Object(current) = current else {
            bail!("the hub's policy is not a JSON object");
        };
        let mut policy = current.clone();
        if let Some(mode) = self.mode {
            policy.insert("mode".to_owned(), json!(mode));
        }
        if self.suite.is_some() || self.reporters.is_some() {
            let report = policy
                .entry("report")
                .or_insert_with(|| Value::Object(Map::new()));
            if !report.is_object() {
                *report = Value::Object(Map::new());
            }
            if let Some(suite) = &self.suite {
                report["suite"] = json!(suite);
            }
            if let Some(reporters) = &self.reporters {
                report["reporters"] = json!(reporters);
            }
        }
        let has_report = policy.get("report").is_some_and(Value::is_object);
        match &self.green_from {
            Some(sources) => {
                policy.insert("green_from".to_owned(), json!(sources));
            }
            None => {
                let none = policy
                    .get("green_from")
                    .and_then(Value::as_array)
                    .is_none_or(Vec::is_empty);
                if none && has_report {
                    policy.insert("green_from".to_owned(), json!([SOURCE_REPORT]));
                }
            }
        }
        let merged = Value::Object(policy);
        let read: VerifyPolicy = serde_json::from_value(merged.clone())
            .context("the hub's policy is not one braid can change")?;
        if read.mode == MODE_REPORT {
            let complete = read
                .report
                .as_ref()
                .is_some_and(|report| !report.suite.is_empty() && !report.reporters.is_empty());
            if !complete {
                bail!(
                    "verifying in report mode needs the suite and the reporters: give --suite and --reporter"
                );
            }
        }
        Ok(merged)
    }
}

/// `--suite`, checked.
fn suite_arg(suite: &str) -> Result<String> {
    if !is_suite(suite) {
        bail!(
            "{suite:?} is not a suite: 1 to 32 of a-z, 0-9, '.', '_' and '-', starting with a letter or a digit"
        );
    }
    Ok(suite.to_owned())
}

/// A changeset's id, 64 hex digits, given as an argument.
fn full_id(text: &str, flag: &str) -> Result<ObjectId> {
    text.to_ascii_lowercase()
        .parse()
        .ok()
        .with_context(|| format!("{flag} takes a changeset's id, 64 hex digits, not {text:?}"))
}

/// What the arguments of `action` ask of the hub, checked; `resolve` turns
/// a revision into a changeset.
fn asked(action: &VerifyAction, resolve: impl Fn(&str) -> Result<ObjectId>) -> Result<Asked> {
    Ok(match action {
        VerifyAction::Status(args) => Asked::Status {
            fail_on_red: args.fail_on_red,
        },
        VerifyAction::Report(args) => {
            let git_commit = match &args.git_commit {
                Some(sha) => {
                    let sha = sha.to_ascii_lowercase();
                    if !is_git_commit(&sha) {
                        bail!("--git-commit takes a git commit, 40 hex digits, not {sha:?}");
                    }
                    Some(sha)
                }
                None => None,
            };
            let changeset = args
                .changeset
                .as_deref()
                .map(|text| full_id(text, "--changeset"))
                .transpose()?;
            Asked::Report(VerifyReport {
                git_commit,
                changeset,
                outcome: args.outcome.as_str().to_owned(),
                suite: suite_arg(&args.suite)?,
            })
        }
        VerifyAction::Policy(args) => Asked::Policy(PolicyChange::of(args)?),
        VerifyAction::Log(args) => Asked::Log {
            after: args.after,
            limit: args.limit,
        },
        VerifyAction::Reset(args) => Asked::Reset(VerifyReset {
            green: args.green.as_deref().map(resolve).transpose()?,
            expect: None,
        }),
    })
}

/// What a command did: what `--json` prints, what it prints without, and
/// its exit status.
#[derive(Debug)]
struct Done {
    json: Value,
    text: String,
    code: u8,
}

/// Whether a state is red: `red` or `stuck`.
pub(crate) fn is_red(state: &str) -> bool {
    matches!(state, STATE_RED | STATE_STUCK)
}

/// `answer`, from `route`, read as `T`.
fn typed<T: DeserializeOwned>(answer: &Value, route: &str) -> Result<T, HubError> {
    T::deserialize(answer).map_err(|err| {
        HubError::Protocol(format!(
            "{route} answered with something that is not the hub protocol ({err})"
        ))
    })
}

/// Sends a request that changes something, and sends it once more when its
/// answer was lost: with the same `Idempotency-Key`, the hub gives the
/// first answer again rather than doing it twice.
fn twice(send: impl Fn() -> Result<Value, HubError>) -> Result<Value, HubError> {
    match send() {
        Err(err) if err.may_have_happened() => send(),
        answer => answer,
    }
}

/// `err`, saying, when the answer was lost twice, that the hub may have
/// done `what`, and that `look` shows whether it did.
fn lost(err: HubError, what: &str, look: &str) -> anyhow::Error {
    if err.may_have_happened() {
        anyhow::Error::new(err).context(format!(
            "the hub may have {what} before its answer was lost: {look} shows whether it did"
        ))
    } else {
        err.into()
    }
}

/// Runs what was asked against `api`, the repository `path` on the hub at
/// `url`.
fn run(api: &impl VerifyApi, url: &str, path: &RepoPath, asked: &Asked) -> Result<Done> {
    if !api.features()?.iter().any(|feature| feature == FEATURE) {
        bail!(
            "the hub at {url} does not verify branches: its GET /v1 lists no `{FEATURE}` feature"
        );
    }
    Ok(match asked {
        Asked::Status { fail_on_red } => {
            let answer = api.status()?;
            let status: VerifyStatus = typed(&answer, "GET …/verify")?;
            let code = if *fail_on_red && is_red(&status.state) {
                RED
            } else {
                DONE
            };
            Done {
                text: status_text(path, &status),
                json: answer,
                code,
            }
        }
        Asked::Report(report) => {
            let key = idempotency_key();
            let answer = twice(|| api.report(report, &key)).map_err(|err| {
                lost(
                    err,
                    "recorded the verdict",
                    &format!("`braid hub verify log -R {path}`"),
                )
            })?;
            let said: VerifyReportAnswer = typed(&answer, "POST …/verify/report")?;
            Done {
                text: format!(
                    "{}\n{}",
                    report_text(report, &said.effect),
                    status_text(path, &said.status)
                ),
                json: answer,
                code: DONE,
            }
        }
        Asked::Policy(change) => {
            let current = api.policy()?;
            let answer = if change.is_empty() {
                current
            } else {
                api.put_policy(&change.merged(&current)?)?
            };
            let policy: VerifyPolicy = typed(&answer, "…/verify/policy")?;
            Done {
                text: policy_text(path, &policy),
                json: answer,
                code: DONE,
            }
        }
        Asked::Log { after, limit } => {
            let answer = api.log(*after, *limit)?;
            let events: VerifyEvents = typed(&answer, "GET …/verify/log")?;
            Done {
                text: log_text(&events),
                json: answer,
                code: DONE,
            }
        }
        Asked::Reset(reset) => {
            let key = idempotency_key();
            let answer = twice(|| api.reset(reset, &key)).map_err(|err| {
                lost(
                    err,
                    "reset the branch",
                    &format!("`braid hub verify log -R {path}`"),
                )
            })?;
            let status: VerifyStatus = typed(&answer, "POST …/verify/reset")?;
            Done {
                text: format!("Reset.\n{}", status_text(path, &status)),
                json: answer,
                code: DONE,
            }
        }
    })
}

/// The exit status of a command that failed with `err`, when the hub
/// refused it or could not be reached; `None` for an error of braid's
/// own, which exits with 1.
fn hub_exit(err: &anyhow::Error) -> Option<u8> {
    err.downcast_ref::<HubError>().map(exit_for)
}

pub(crate) fn verify(cli: &Cli, args: &VerifyArgs) -> Result<ExitCode> {
    let asked = asked(&args.action, |rev| changeset_of(cli, rev))?;
    let (hub, path) = hub_repo(cli, args.action.hub_repo())?;
    let api = hub.repo(path.clone());
    match run(&api, hub.url(), &path, &asked) {
        Ok(done) => {
            print(cli, &done.json, || done.text.clone())?;
            Ok(ExitCode::from(done.code))
        }
        Err(err) => match hub_exit(&err) {
            Some(code) => {
                eprintln!("braid: error: {err:#}");
                Ok(ExitCode::from(code))
            }
            None => Err(err),
        },
    }
}

/// The changeset `rev` names: its id, or a revision of the repository
/// braid runs in.
fn changeset_of(cli: &Cli, rev: &str) -> Result<ObjectId> {
    if let Ok(id) = rev.to_ascii_lowercase().parse() {
        return Ok(id);
    }
    let repo = open_repo(cli).with_context(|| {
        format!(
            "{rev:?} is not a changeset's id (64 hex digits), and there is no repository here to read it in"
        )
    })?;
    resolve_changeset(&repo, rev)
}

/// How the branch stands, in words. What the hub says goes through
/// [`shown`].
fn status_text(path: &RepoPath, status: &VerifyStatus) -> String {
    if status.state == STATE_OFF || status.mode == MODE_OFF {
        return format!("{path}: verification is off");
    }
    let branch = status
        .branch
        .as_deref()
        .map_or_else(|| "the default branch".to_owned(), shown);
    let mut out = format!("{path} {branch}: {}", shown(&status.state));
    if let Some(code) = &status.code {
        let _ = write!(out, " ({})", shown(code));
    }
    if let Some(since) = &status.since {
        let _ = write!(out, " since {}", shown(since));
    }
    let _ = write!(
        out,
        "\n  head     {}\n  green    {}\n  red      {}",
        short(status.head),
        short(status.green),
        short(status.red)
    );
    if let Some(culprit) = status.culprit {
        let _ = write!(out, "\n  culprit  {}", culprit.short());
        if let Some(decided) = &status.decided_by {
            let _ = write!(out, " ({})", shown(decided));
        }
        if let Some(lander) = status
            .admin
            .as_ref()
            .and_then(|admin| admin.lander.as_ref())
        {
            let _ = write!(out, ", landed by {}", shown(lander));
        }
    }
    if let Some(window) = &status.window {
        let _ = write!(
            out,
            "\n  window   {} after {} ({}), up to {}",
            count(window.candidates, "changeset"),
            short(window.lower),
            shown(&window.lower_kind),
            short(window.upper)
        );
    }
    out
}

/// What a report did, in words.
fn report_text(report: &VerifyReport, effect: &str) -> String {
    let on = match (&report.changeset, &report.git_commit) {
        (Some(id), _) => id.short(),
        (None, Some(sha)) => format!("git commit {}", &sha[..sha.len().min(12)]),
        (None, None) => "the changeset".to_owned(),
    };
    let what = match effect {
        "red_set" => "the branch turned red".to_owned(),
        "green_moved" => "green moved".to_owned(),
        "red_cleared" => "the branch is no longer red".to_owned(),
        "recorded" => "recorded".to_owned(),
        "duplicate" => "the hub had this verdict already".to_owned(),
        "stale" => "it is at or below green: nothing was recorded".to_owned(),
        other => shown(other),
    };
    format!(
        "{} on {on}, suite {}: {what}",
        shown(&report.outcome),
        shown(&report.suite)
    )
}

/// The policy, in words.
fn policy_text(path: &RepoPath, policy: &VerifyPolicy) -> String {
    let mut out = format!("{path}: verification {}", shown(&policy.mode));
    let sources = if policy.green_from.is_empty() {
        "none".to_owned()
    } else {
        shown(&policy.green_from.join(", "))
    };
    let _ = write!(
        out,
        "\n  green from         {sources}\n  flaky moves green  {}",
        if policy.flaky_moves_green {
            "yes"
        } else {
            "no"
        }
    );
    if let Some(report) = &policy.report {
        let _ = write!(
            out,
            "\n  suite              {}\n  reporters          {}",
            shown(&report.suite),
            shown(&report.reporters.join(", "))
        );
    }
    out
}

/// A page of the log, in words: an event a line.
fn log_text(events: &VerifyEvents) -> String {
    if events.events.is_empty() {
        return "No events.".to_owned();
    }
    let mut out = String::new();
    for event in &events.events {
        let _ = write!(
            out,
            "{:<6} {}  {:<8} {}",
            event.id,
            shown(&event.at),
            shown(&event.code),
            event
                .changeset
                .map_or_else(|| "-".repeat(12), |id| id.short())
        );
        if let Some(other) = &event.other {
            let _ = write!(out, "  {}", shown(other));
        }
        if let Some(actor) = &event.actor {
            let _ = write!(out, "  by {}", shown(actor));
        }
        out.push('\n');
    }
    if let Some(next) = events.next {
        let _ = write!(out, "Older events: --after {next}");
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::testing::{FakeHub, Refusal, ada_change, set_ref};
    use braid_hubproto::verify::{
        EFFECT_DUPLICATE, EFFECT_GREEN_MOVED, EFFECT_RED_CLEARED, EFFECT_RED_SET, EFFECT_STALE,
        FAIL, PASS, STATE_GREEN, STATE_PENDING,
    };
    use clap::Parser;

    const URL: &str = "http://127.0.0.1:9";

    /// A hub that verifies branches, whose repository's main holds `a`,
    /// `b` and `c`, and whose policy takes the reports of ci-bot on `ci`.
    fn hub() -> (FakeHub, [ObjectId; 3]) {
        let hub = FakeHub::new();
        let a = ada_change(&hub.repo, None, &[("a.txt", "a\n")]);
        let b = ada_change(&hub.repo, Some(a), &[("b.txt", "b\n")]);
        let c = ada_change(&hub.repo, Some(b), &[("c.txt", "c\n")]);
        set_ref(&hub.repo, "main", c);
        hub.verify.borrow_mut().turn_on();
        (hub, [a, b, c])
    }

    fn path() -> RepoPath {
        "ada/demo".parse().unwrap()
    }

    fn report(outcome: &str, changeset: ObjectId) -> Asked {
        Asked::Report(VerifyReport {
            git_commit: None,
            changeset: Some(changeset),
            outcome: outcome.to_owned(),
            suite: "ci".to_owned(),
        })
    }

    fn status(fail_on_red: bool) -> Asked {
        Asked::Status { fail_on_red }
    }

    #[test]
    fn reports_move_the_refs_and_say_what_they_did() {
        let (hub, [a, b, c]) = hub();
        let done = run(&hub, URL, &path(), &report(PASS, a)).unwrap();
        assert_eq!(done.code, DONE);
        assert_eq!(done.json["effect"], EFFECT_GREEN_MOVED);
        assert_eq!(done.json["status"]["green"], a.to_hex());
        assert!(
            done.text
                .starts_with(&format!("pass on {}, suite ci: green moved", a.short()))
        );
        assert!(
            done.text.contains("ada/demo main: pending"),
            "{}",
            done.text
        );

        // A fail right after green names its culprit.
        let done = run(&hub, URL, &path(), &report(FAIL, b)).unwrap();
        assert_eq!(done.json["effect"], EFFECT_RED_SET);
        let said: VerifyStatus = serde_json::from_value(done.json["status"].clone()).unwrap();
        assert_eq!((said.state.as_str(), said.culprit), (STATE_RED, Some(b)));
        assert!(
            done.text
                .contains(&format!("culprit  {} (single)", b.short()))
        );
        // The same verdict again changes nothing; one below green neither.
        let again = run(&hub, URL, &path(), &report(FAIL, b)).unwrap();
        assert_eq!(again.json["effect"], EFFECT_DUPLICATE);
        let below = run(&hub, URL, &path(), &report(FAIL, a)).unwrap();
        assert_eq!(below.json["effect"], EFFECT_STALE);
        // A pass at the head ends the red.
        let done = run(&hub, URL, &path(), &report(PASS, c)).unwrap();
        assert_eq!(done.json["effect"], EFFECT_RED_CLEARED);
        assert_eq!(done.json["status"]["state"], STATE_GREEN);
        // Each report went with a key of its own.
        let keys: Vec<String> = hub
            .verify
            .borrow()
            .keyed
            .iter()
            .map(|(key, _)| key.clone())
            .collect();
        assert_eq!(keys.len(), 5);
        assert!(keys.windows(2).all(|pair| pair[0] != pair[1]), "{keys:?}");
    }

    #[test]
    fn status_and_its_exit_statuses() {
        let (hub, [_, b, _]) = hub();
        let done = run(&hub, URL, &path(), &status(true)).unwrap();
        assert_eq!(
            (done.code, done.json["state"].as_str()),
            (DONE, Some("unknown"))
        );
        run(&hub, URL, &path(), &report(FAIL, b)).unwrap();
        // Red: 4 with --fail-on-red, 0 without.
        let red = run(&hub, URL, &path(), &status(true)).unwrap();
        assert_eq!(red.code, RED);
        assert_eq!(red.json["red"], b.to_hex());
        assert_eq!(run(&hub, URL, &path(), &status(false)).unwrap().code, DONE);
        // Stuck counts as red; a state braid does not know does not.
        hub.verify.borrow_mut().status.state = STATE_STUCK.to_owned();
        assert_eq!(run(&hub, URL, &path(), &status(true)).unwrap().code, RED);
        hub.verify.borrow_mut().status.state = "held".to_owned();
        assert_eq!(run(&hub, URL, &path(), &status(true)).unwrap().code, DONE);
        // What the hub answered is printed whole, fields braid does not
        // know included.
        hub.verify.borrow_mut().extra = Some(json!({"later": [1, 2]}));
        let done = run(&hub, URL, &path(), &status(false)).unwrap();
        assert_eq!(done.json["later"], json!([1, 2]));
        // With mode off, nothing more is said.
        hub.verify.borrow_mut().status = VerifyStatus::default();
        hub.verify.borrow_mut().extra = None;
        let off = run(&hub, URL, &path(), &status(true)).unwrap();
        assert_eq!(
            (off.code, off.text.as_str()),
            (DONE, "ada/demo: verification is off")
        );
    }

    #[test]
    fn a_lost_answer_is_sent_again_with_the_same_key() {
        let (hub, [a, _, _]) = hub();
        // The answer of the first try is lost once the hub recorded the
        // verdict: the same report goes again, with the same key, and the
        // hub gives its first answer again.
        hub.verify.borrow_mut().lose = 1;
        let done = run(&hub, URL, &path(), &report(PASS, a)).unwrap();
        assert_eq!(done.json["effect"], EFFECT_GREEN_MOVED);
        {
            let verify = hub.verify.borrow();
            assert_eq!(verify.keyed.len(), 2);
            assert_eq!(verify.keyed[0], verify.keyed[1], "the same key and body");
            assert_eq!(verify.verdicts.len(), 1, "recorded once");
        }
        // Lost twice: an error that says the hub may have recorded it, as
        // a hub that cannot be reached.
        hub.verify.borrow_mut().lose = 2;
        let err = run(&hub, URL, &path(), &report(FAIL, a)).unwrap_err();
        assert_eq!(hub_exit(&err), Some(3));
        let said = format!("{err:#}");
        assert!(said.contains("may have recorded the verdict"), "{said}");
        assert!(said.contains("braid hub verify log -R ada/demo"), "{said}");
        // A reset too.
        hub.verify.borrow_mut().lose = 1;
        let reset = Asked::Reset(VerifyReset {
            green: Some(a),
            expect: None,
        });
        run(&hub, URL, &path(), &reset).unwrap();
        let verify = hub.verify.borrow();
        let n = verify.keyed.len();
        assert_eq!(verify.keyed[n - 1], verify.keyed[n - 2]);
        assert_eq!(
            verify.keyed[n - 1].1,
            json!({"green": a.to_hex(), "expect": null})
        );
    }

    #[test]
    fn the_policy_is_read_merged_and_put_back_whole() {
        let (hub, _) = hub();
        let read = Asked::Policy(PolicyChange::default());
        let done = run(&hub, URL, &path(), &read).unwrap();
        assert_eq!(done.json, hub.verify.borrow().policy);
        assert!(
            done.text.contains("reporters          ci-bot"),
            "{}",
            done.text
        );
        assert_eq!(hub.called("verify/policy put"), 0, "read only");

        // A newer hub's field stays as it was, through a change of what
        // braid knows.
        hub.verify.borrow_mut().policy["github"] = json!({"sources": [{"workflow": "ci.yml"}]});
        let args = PolicyArgs {
            repo: HubRepoArg::default(),
            mode: None,
            suite: Some("unit".into()),
            reporters: vec!["CI-Bot".into(), "other".into(), "ci-bot".into()],
            green_from: Vec::new(),
        };
        let change = PolicyChange::of(&args).unwrap();
        let done = run(&hub, URL, &path(), &Asked::Policy(change)).unwrap();
        assert_eq!(
            done.json["report"],
            json!({"suite": "unit", "reporters": ["ci-bot", "other"]})
        );
        assert_eq!(
            done.json["github"],
            json!({"sources": [{"workflow": "ci.yml"}]})
        );
        assert_eq!(hub.called("verify/policy put"), 1);

        // On a policy never set, the first change takes report as its
        // source; turning it off keeps the rest.
        hub.verify.borrow_mut().policy = serde_json::to_value(VerifyPolicy::default()).unwrap();
        let args = PolicyArgs {
            mode: Some(Mode::Report),
            suite: Some("ci".into()),
            reporters: vec!["ci-bot".into()],
            ..args
        };
        let done = run(
            &hub,
            URL,
            &path(),
            &Asked::Policy(PolicyChange::of(&args).unwrap()),
        )
        .unwrap();
        assert_eq!(done.json["green_from"], json!(["report"]));
        assert_eq!(done.json["mode"], "report");
        let off = PolicyChange {
            mode: Some(MODE_OFF),
            ..PolicyChange::default()
        };
        let done = run(&hub, URL, &path(), &Asked::Policy(off)).unwrap();
        assert_eq!(done.json["mode"], "off");
        assert_eq!(done.json["report"]["suite"], "ci");
        assert!(
            done.text.starts_with("ada/demo: verification off"),
            "{}",
            done.text
        );

        // Report mode without reporters is refused before anything is put.
        hub.verify.borrow_mut().policy = serde_json::to_value(VerifyPolicy::default()).unwrap();
        let half = PolicyChange {
            mode: Some(MODE_REPORT),
            ..PolicyChange::default()
        };
        let puts = hub.called("verify/policy put");
        let err = run(&hub, URL, &path(), &Asked::Policy(half)).unwrap_err();
        assert_eq!(hub_exit(&err), None);
        assert!(err.to_string().contains("--suite and --reporter"), "{err}");
        assert_eq!(hub.called("verify/policy put"), puts);
    }

    #[test]
    fn the_log_and_resets() {
        let (hub, [a, b, c]) = hub();
        for (outcome, id) in [(PASS, a), (FAIL, c)] {
            run(&hub, URL, &path(), &report(outcome, id)).unwrap();
        }
        // Newest first, a page at a time.
        let done = run(
            &hub,
            URL,
            &path(),
            &Asked::Log {
                after: None,
                limit: Some(1),
            },
        )
        .unwrap();
        let events: VerifyEvents = serde_json::from_value(done.json.clone()).unwrap();
        assert_eq!(events.events.len(), 1);
        assert!(
            done.text.lines().next().unwrap().contains("red"),
            "{}",
            done.text
        );
        let next = events.next.expect("a page that is not the last");
        assert!(
            done.text.ends_with(&format!("--after {next}")),
            "{}",
            done.text
        );
        let rest = run(
            &hub,
            URL,
            &path(),
            &Asked::Log {
                after: Some(next),
                limit: None,
            },
        )
        .unwrap();
        let older: VerifyEvents = serde_json::from_value(rest.json).unwrap();
        assert!(!older.events.is_empty());
        assert!(older.events.iter().all(|event| event.id < next));
        assert!(older.events.windows(2).all(|pair| pair[0].id > pair[1].id));
        assert_eq!(older.next, None);

        // A dismissal clears red; green stays.
        let dismiss = Asked::Reset(VerifyReset::default());
        let done = run(&hub, URL, &path(), &dismiss).unwrap();
        assert_eq!(done.json["red"], Value::Null);
        assert_eq!(done.json["green"], a.to_hex());
        assert_eq!(done.json["state"], STATE_PENDING);
        assert!(
            done.text.starts_with("Reset.\nada/demo main: pending"),
            "{}",
            done.text
        );
        // Green moves where it is told, backward too.
        let to_b = Asked::Reset(VerifyReset {
            green: Some(b),
            expect: None,
        });
        let done = run(&hub, URL, &path(), &to_b).unwrap();
        assert_eq!(done.json["green"], b.to_hex());
        let log = run(
            &hub,
            URL,
            &path(),
            &Asked::Log {
                after: None,
                limit: None,
            },
        )
        .unwrap();
        assert!(
            log.text.lines().next().unwrap().contains("reset"),
            "{}",
            log.text
        );
    }

    #[test]
    fn refusals_and_a_hub_out_of_reach() {
        let (hub, [a, _, _]) = hub();
        hub.verify.borrow_mut().refuse = Some(Refusal::Api(403, "forbidden"));
        let err = run(&hub, URL, &path(), &report(PASS, a)).unwrap_err();
        assert_eq!(hub_exit(&err), Some(1));
        hub.verify.borrow_mut().refuse = Some(Refusal::Unreachable);
        let err = run(&hub, URL, &path(), &status(false)).unwrap_err();
        assert_eq!(hub_exit(&err), Some(3));
        hub.verify.borrow_mut().refuse = None;
        // A suite the policy does not name, a commit the hub does not know,
        // and a changeset off the branch are refused by the hub.
        let other = Asked::Report(VerifyReport {
            suite: "other".into(),
            ..VerifyReport::default()
        });
        assert_eq!(
            hub_exit(&run(&hub, URL, &path(), &other).unwrap_err()),
            Some(1)
        );
        let unknown = Asked::Report(VerifyReport {
            git_commit: Some("0a".repeat(20)),
            changeset: None,
            outcome: PASS.into(),
            suite: "ci".into(),
        });
        let err = run(&hub, URL, &path(), &unknown).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<HubError>(),
            Some(HubError::Api { status: 404, .. })
        ));
        let elsewhere = ada_change(&hub.repo, None, &[("z.txt", "z\n")]);
        let err = run(&hub, URL, &path(), &report(FAIL, elsewhere)).unwrap_err();
        assert_eq!(
            err.downcast_ref::<HubError>().and_then(HubError::code),
            Some("not_on_branch")
        );
        // A commit the hub knows is taken.
        hub.verify.borrow_mut().commits.insert("0a".repeat(20), a);
        let done = run(&hub, URL, &path(), &unknown).unwrap();
        assert!(
            done.text.starts_with("pass on git commit 0a0a0a0a0a0a"),
            "{}",
            done.text
        );

        // A hub that does not verify is asked nothing more.
        hub.verify.borrow_mut().served = false;
        let calls = hub.calls.borrow().len();
        let err = run(&hub, URL, &path(), &status(false)).unwrap_err();
        assert_eq!(hub_exit(&err), None);
        assert!(
            err.to_string().contains("does not verify branches"),
            "{err}"
        );
        assert_eq!(
            hub.calls.borrow().len(),
            calls + 1,
            "{:?}",
            hub.calls.borrow()
        );
    }

    /// `braid` with `args`, parsed.
    fn parse(args: &[&str]) -> Result<VerifyAction, clap::Error> {
        let cli = crate::Cli::try_parse_from(args)?;
        match cli.command {
            crate::Command::Hub {
                action: super::super::HubAction::Verify(args),
            } => Ok(args.action),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn arguments_are_checked_before_anything_is_sent() {
        let id = ObjectId::hash(braid_format::Kind::Changeset, b"x");
        let hex = id.to_hex();
        let upper = "0A".repeat(20);
        let no_rev = |rev: &str| -> Result<ObjectId> { bail!("no repository for {rev}") };
        let action = parse(&[
            "braid",
            "hub",
            "verify",
            "report",
            "-R",
            "ada/demo",
            "--outcome",
            "fail",
            "--git-commit",
            &upper,
            "--suite",
            "ci",
        ])
        .unwrap();
        assert_eq!(action.hub_repo(), Some("ada/demo"));
        match asked(&action, no_rev).unwrap() {
            Asked::Report(report) => {
                assert_eq!(report.git_commit, Some("0a".repeat(20)));
                assert_eq!(
                    (report.outcome.as_str(), report.suite.as_str()),
                    (FAIL, "ci")
                );
            }
            other => panic!("{other:?}"),
        }
        let action = parse(&[
            "braid",
            "hub",
            "verify",
            "report",
            "--outcome",
            "pass",
            "--changeset",
            &hex,
            "--suite",
            "ci",
        ])
        .unwrap();
        assert!(
            matches!(asked(&action, no_rev).unwrap(), Asked::Report(VerifyReport { changeset: Some(found), .. }) if found == id)
        );
        // Exactly one of the two is needed.
        for args in [
            &[
                "braid",
                "hub",
                "verify",
                "report",
                "--outcome",
                "pass",
                "--suite",
                "ci",
            ][..],
            &[
                "braid",
                "hub",
                "verify",
                "report",
                "--outcome",
                "pass",
                "--suite",
                "ci",
                "--changeset",
                "x",
                "--git-commit",
                "y",
            ],
            &[
                "braid",
                "hub",
                "verify",
                "report",
                "--outcome",
                "maybe",
                "--suite",
                "ci",
                "--changeset",
                "x",
            ],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
        // What the hub would refuse is refused here.
        for (args, said) in [
            (
                vec!["--git-commit", "abc", "--suite", "ci"],
                "40 hex digits",
            ),
            (vec!["--changeset", "abc", "--suite", "ci"], "64 hex digits"),
            (vec!["--changeset", &hex, "--suite", "CI"], "is not a suite"),
        ] {
            let mut line = vec!["braid", "hub", "verify", "report", "--outcome", "pass"];
            line.extend(args);
            let err = asked(&parse(&line).unwrap(), no_rev).unwrap_err();
            assert!(err.to_string().contains(said), "{line:?}: {err}");
        }
        let policy = parse(&[
            "braid",
            "hub",
            "verify",
            "policy",
            "--mode",
            "report",
            "--reporter",
            "Ada",
            "--green-from",
            "report",
            "--suite",
            "ci",
        ])
        .unwrap();
        match asked(&policy, no_rev).unwrap() {
            Asked::Policy(change) => assert_eq!(
                change,
                PolicyChange {
                    mode: Some(MODE_REPORT),
                    suite: Some("ci".into()),
                    reporters: Some(vec!["ada".into()]),
                    green_from: Some(vec![SOURCE_REPORT.into()]),
                }
            ),
            other => panic!("{other:?}"),
        }
        let bad = parse(&[
            "braid",
            "hub",
            "verify",
            "policy",
            "--reporter",
            "bad--login",
        ])
        .unwrap();
        assert!(asked(&bad, no_rev).is_err());
        let many: Vec<String> = (0..11).map(|n| format!("--reporter=r{n}")).collect();
        let mut line = vec!["braid", "hub", "verify", "policy"];
        line.extend(many.iter().map(String::as_str));
        assert!(
            asked(&parse(&line).unwrap(), no_rev)
                .unwrap_err()
                .to_string()
                .contains("at most 10")
        );
        assert!(parse(&["braid", "hub", "verify", "policy", "--mode", "auto"]).is_err());
        assert!(parse(&["braid", "hub", "verify", "log", "--limit", "0"]).is_err());
        assert!(parse(&["braid", "hub", "verify", "log", "--limit", "101"]).is_err());
        let log = parse(&[
            "braid", "hub", "verify", "log", "--limit", "5", "--after", "9",
        ])
        .unwrap();
        assert!(matches!(
            asked(&log, no_rev).unwrap(),
            Asked::Log {
                after: Some(9),
                limit: Some(5)
            }
        ));
        // A reset names a changeset through the resolver.
        let reset = parse(&["braid", "hub", "verify", "reset", "--green", "main~1"]).unwrap();
        let found = asked(&reset, |rev: &str| -> Result<ObjectId> {
            assert_eq!(rev, "main~1");
            Ok(id)
        })
        .unwrap();
        assert!(
            matches!(found, Asked::Reset(VerifyReset { green: Some(green), expect: None }) if green == id)
        );
        assert!(asked(&reset, no_rev).is_err());
        let status = parse(&[
            "braid",
            "hub",
            "verify",
            "status",
            "--fail-on-red",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            asked(&status, no_rev).unwrap(),
            Asked::Status { fail_on_red: true }
        ));
    }
}
