// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! BraidHub from the client's side (`docs/hub.md`, section 13): `braid
//! login` and `logout`, `braid hub publish`, `clone`, `fetch`, `status`,
//! `whoami` and `autopublish`, and what `land`, `update` and `change create
//! --land` do in a repository linked to a hub.
//!
//! - [`creds`]: hub URLs and tokens, from `hub.json` and the environment.
//! - [`link`]: `.braid/hub`, which links a repository to its hub.
//! - [`remote`]: the trait the transfer logic talks to, and its errors;
//!   [`http`] speaks it over HTTP.
//! - [`transfer`], [`fetch`], [`publish`](mod@publish), [`land`]: moving
//!   history.
//! - [`device`]: waiting for a device-flow sign-in; [`status`]: how
//!   branches compare.
//! - [`browse`], [`history`], [`explore`], [`social`], [`issue`],
//!   [`inbox`], [`fork`], [`request`], [`github`] and [`settings`]: reading
//!   a repository without cloning it, finding repositories, stars,
//!   watching and following, issues, the inbox, forks, change requests,
//!   GitHub sync and settings; [`social_common`]: what they share; [`api`]:
//!   any route of the API.

pub(crate) mod api;
pub(crate) mod browse;
pub(crate) mod creds;
pub(crate) mod device;
pub(crate) mod explore;
pub(crate) mod fetch;
pub(crate) mod fork;
pub(crate) mod github;
pub(crate) mod history;
pub(crate) mod http;
pub(crate) mod inbox;
pub(crate) mod issue;
pub(crate) mod land;
pub(crate) mod link;
pub(crate) mod publish;
pub(crate) mod remote;
pub(crate) mod request;
#[cfg(test)]
mod resilience_tests;
pub(crate) mod settings;
pub(crate) mod social;
pub(crate) mod social_common;
pub(crate) mod status;
#[cfg(test)]
pub(crate) mod testing;
pub(crate) mod transfer;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use braid_hubproto::api::RepoCreate;
use braid_hubproto::names::is_repo_name;
use braid_hubproto::{RepoPath, Token};
use braid_repo::{Repo, WorkspaceLink};
use braid_wc::Workspace;
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::work::{branches, count, hex, short};
use crate::{Cli, open_repo, print_json};
use creds::{Credentials, HubEnv, Saved, TokenSource};
use fetch::{Fetched, Outcome, RefFetched, is_tag};
use http::HttpHub;
use link::Link;
use remote::{HubError, Remote};

/// The long help of `login`.
pub(crate) const LOGIN_HELP: &str =
    "Sign in to a hub with GitHub's device flow, run through the hub, so that
no browser is needed on this machine: braid prints a web address and a code
on standard error; open the address anywhere, enter the code and approve.
braid polls the hub until then, at the interval it asks for, and saves the
token it gets.

The token is saved in hub.json in braid's configuration directory
($XDG_CONFIG_HOME/braid, or ~/.config/braid), which only you can read, and
that hub becomes the one `braid hub publish` and `braid hub clone` use. The
token reads, writes and lands on every repository the account can, for 90
days. BRAID_TOKEN and BRAID_HUB_URL take precedence over the file: an agent
or CI job can set them instead of signing in.

The hub is --hub, else BRAID_HUB_URL, else the hub signed in to last, else
https://braidhub.elffuss.com. Only https:// is taken, and http:// to this
machine.

With --json it prints hub, login and expires.";

/// The long help of `logout`.
pub(crate) const LOGOUT_HELP: &str =
    "Revoke the token saved for a hub, on the hub when it can be reached, and
delete it here. BRAID_TOKEN is left alone: unset it yourself.

With --json it prints hub, signed_in (whether a token was saved), revoked
(whether the hub revoked it) and detail (why it could not, or null).";

/// The long help of `hub publish`.
pub(crate) const PUBLISH_HELP: &str =
    "Create the repository on the hub signed in to, upload every branch and
tag, and link the repository to it: .braid/hub names the hub and the
repository, and from then on `braid land`, `braid update` and `braid change
create --land` go through the hub.

NAME defaults to the name of the repository's directory, made valid: 1 to
100 ASCII letters, digits, '.', '_' and '-'. The repository is private
unless --public. --mirror names a git repository the hub publishes the
default branch to after each landing, with the account's GitHub access.

What the branches and tags reach is uploaded, less what the hub has, in
requests no larger than the hub takes (it says so, at most 256 MiB), or
than BRAID_HUB_MAX_BODY bytes, for a proxy in front of the hub that takes
less; and each ref is created on the hub at its head.
A ref that names no changeset, such as an annotated tag imported from git,
cannot be created through the hub protocol: it is reported and skipped. If
an upload fails, run the command again: in a linked repository it uploads
what the hub lacks and creates the refs it misses.

With --json it prints hub, repo, url (the repository's address on the hub,
<hub>/<owner>/<name>), created (whether the repository was made now),
objects and bytes (what was uploaded), refs (each with name, id, outcome,
head_before and head) and skipped (each with name and reason).";

/// The long help of `hub clone`.
pub(crate) const CLONE_HELP: &str =
    "Make a repository in DIR (default: NAME, in the current directory) from
OWNER/NAME on the hub, linked to it, with every branch and tag, and a
first workspace in .braid/work/<name> on the hub's default branch, named by
--workspace, or else after the branch.

--paths takes path prefixes: only the trees under them come, with the
directories on the way to them. Such a sparse repository serves `braid
cat-object`, `braid log` and `braid change create --land`, but holds no
workspace, since a workspace is a whole checkout. Give DIR before --paths.

The hub is BRAID_HUB_URL, else the hub signed in to last, else
https://braidhub.elffuss.com. A public repository needs no sign-in.

With --json it prints hub, repo, root, paths, objects, refs (as `braid hub
fetch` prints them) and workspace (name, path, branch, base and files, or
null).";

/// The long help of `hub fetch`.
pub(crate) const FETCH_HELP: &str =
    "Bring the hub's branches and tags into the linked repository. A local
branch moves to the hub's head when it does not exist yet, or when its head
is on the first-parent chain of the hub's; a branch with changesets the hub
does not have stays where it is, and is reported as ahead or diverged. A
tag is created, and never moved. remote/hub/<ref> records where each of the
hub's refs was seen, so `braid log remote/hub/main` shows the hub's main
even when the local one was kept back. Tags that name no changeset are
skipped: the hub protocol carries changesets only.

`braid land` and `braid update` fetch by themselves in a linked repository.

With --json it prints hub, repo, seq (the hub's op log sequence number),
objects (how many came), operation (the local op log operation that moved
refs, or null) and refs, each with name, hub, before, after and outcome:
created, moved, same, ahead, diverged, or skipped with a reason.";

/// The long help of `hub status`.
pub(crate) const STATUS_HELP: &str =
    "Show the hub and the repository this one is linked to, who is signed in,
and how each branch compares with the hub's: same; behind, when the hub's
has changesets on top of this one (`braid hub fetch` brings them in);
ahead, when this one has changesets the hub lacks (`braid land` from a
workspace on the branch sends them); or diverged. A branch only the hub has
is behind, and one only this repository has is ahead.

A repository with a mirror shows where the hub publishes its default
branch, and how the last sync ended: diverged means someone pushed to the
mirror directly, and the hub's owner takes those commits in on the hub.

With --json it prints hub, repo, paths, login (null when not signed in),
token (environment, file or null), mirror (url, last_sync, outcome and
detail, or null), seq (the hub's op log sequence number) and branches, each
with name, local, hub and state.";

/// The long help of `hub whoami`.
pub(crate) const WHOAMI_HELP: &str =
    "Say who is signed in to the hub, as the hub sees the token: the account's
login and when the token expires (GET /v1/me). The exit status is 1 when no
token is saved for the hub and BRAID_TOKEN is not set for it, or when the
hub refuses the token because it expired or was revoked: then `braid login`
is needed. It is 3 when braid cannot tell, because the hub cannot be reached
or failed, or its answer is not one: then signing in again would not help.

The hub is the one `braid login` signs in to: --hub, else BRAID_HUB_URL,
else the hub signed in to last, else https://braidhub.elffuss.com.

With --json it prints hub, login, name (or null), expires and token (where
the token came from: environment or file).";

/// The long help of `hub autopublish`.
pub(crate) const AUTOPUBLISH_HELP: &str =
    "Say, or record, whether coding agents publish the new projects they start
for you to the hub, privately, so that agents on your other machines can
work on them too. With no argument it prints on, off or unset; on or off
records the answer, and prints it. The answer holds for every project and
every hub, and is kept in hub.json, next to the tokens.

braid never publishes by itself: an agent reads the answer, and runs `braid
hub publish` when it is on (the hub's guide for agents, /agents.md, says
when).

With --json it prints autopublish: on, off or unset.";

/// `login` and `logout`.
#[derive(Debug, Args)]
pub(crate) struct LoginArgs {
    /// The hub (default: BRAID_HUB_URL, else the hub signed in to last,
    /// else the public hub).
    #[arg(long, value_name = "URL")]
    pub(crate) hub: Option<String>,
}

/// `braid hub`.
#[derive(Debug, Subcommand)]
pub(crate) enum HubAction {
    /// Create the repository on the hub, upload every branch and tag, and
    /// link this repository to it.
    #[command(long_about = PUBLISH_HELP)]
    Publish(PublishArgs),
    /// Make a repository from one on the hub, linked to it.
    #[command(long_about = CLONE_HELP)]
    Clone(CloneArgs),
    /// Bring the hub's branches and tags in, moving local branches forward.
    #[command(long_about = FETCH_HELP)]
    Fetch,
    /// Show the link, who is signed in, and how each branch compares.
    #[command(long_about = STATUS_HELP)]
    Status,
    /// Say who is signed in to the hub (exit status 1 when no one is).
    #[command(long_about = WHOAMI_HELP)]
    Whoami(LoginArgs),
    /// Say, or record, whether agents publish the new projects they start.
    #[command(long_about = AUTOPUBLISH_HELP)]
    Autopublish(AutopublishArgs),
    /// Show a repository on the hub: its description, default branch,
    /// counts and README.
    Show(browse::ShowArgs),
    /// List a directory of a repository on the hub, without cloning it.
    Ls(browse::LsArgs),
    /// Print a file of a repository on the hub, without cloning it.
    Cat(browse::CatArgs),
    /// Show the changesets of a branch on the hub, newest first.
    Log(history::LogArgs),
    /// Show what a changeset on the hub changed.
    Diff(history::DiffArgs),
    /// Find public repositories on the hub.
    Explore(explore::ExploreArgs),
    /// Show an account on the hub: its repositories, stars and follows.
    User(explore::UserArgs),
    /// Star a repository.
    Star(social::StarArgs),
    /// Take back a star.
    Unstar(social::UnstarArgs),
    /// List the repositories you, or an account, starred.
    Stars(social::StarsArgs),
    /// Watch a repository: all of it, what you take part in, or nothing.
    Watch(social::WatchArgs),
    /// Stop watching a repository: back to what you take part in.
    Unwatch(social::UnwatchArgs),
    /// Follow an account.
    Follow(social::FollowArgs),
    /// Stop following an account.
    Unfollow(social::UnfollowArgs),
    /// List, show, open, comment on and close a repository's issues.
    Issue(issue::IssueArgs),
    /// List, create and delete a repository's labels.
    Label(issue::LabelArgs),
    /// Show your notifications; mark them read or done; mute a thread.
    Inbox(inbox::InboxArgs),
    /// Fork a repository into your account.
    Fork(fork::ForkArgs),
    /// Bring what the upstream of a fork landed into the fork.
    SyncFork(fork::SyncForkArgs),
    /// Show your jobs, such as forks being made, or wait for one.
    Jobs(fork::JobsArgs),
    /// Open, review, update and land change requests.
    #[command(long_about = request::REQUEST_HELP)]
    Request(request::RequestArgs),
    /// Grant the hub GitHub access, and sync your GitHub repositories to
    /// it.
    Github(github::GithubArgs),
    /// Show or change a repository's settings.
    Settings(settings::SettingsArgs),
    /// Block an account from your repositories, or from one of them.
    Block(settings::BlockArgs),
    /// Lift a block.
    Unblock(settings::UnblockArgs),
    /// Show or change your account's settings.
    Account(settings::AccountArgs),
    /// Send a request to the hub's API and print its answer.
    #[command(long_about = api::API_HELP)]
    Api(api::ApiArgs),
}

/// `hub autopublish`.
#[derive(Debug, Args)]
pub(crate) struct AutopublishArgs {
    /// Record the answer (default: print it).
    #[arg(value_enum)]
    pub(crate) answer: Option<Switch>,
}

/// An answer to `hub autopublish`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Switch {
    /// Agents publish the new projects they start, privately.
    On,
    /// Agents publish only when asked.
    Off,
}

/// `hub publish`.
#[derive(Debug, Args)]
pub(crate) struct PublishArgs {
    /// The repository's name on the hub (default: the directory's name,
    /// made valid).
    pub(crate) name: Option<String>,
    /// Let anyone read it.
    #[arg(long)]
    pub(crate) public: bool,
    /// A description, for people.
    #[arg(long, value_name = "TEXT")]
    pub(crate) description: Option<String>,
    /// A git repository the hub publishes the default branch to after each
    /// landing.
    #[arg(long, value_name = "URL")]
    pub(crate) mirror: Option<String>,
}

/// `hub clone`.
#[derive(Debug, Args)]
pub(crate) struct CloneArgs {
    // Not named `repo`: that is the id of the global --repo.
    /// The repository: OWNER/NAME.
    #[arg(value_name = "OWNER/NAME")]
    pub(crate) source: String,
    /// The directory to make it in, which must not exist or be empty
    /// (default: NAME).
    pub(crate) dir: Option<PathBuf>,
    /// Take only the trees under these path prefixes.
    #[arg(long, value_name = "PREFIX", num_args = 1..)]
    pub(crate) paths: Vec<String>,
}

pub(crate) fn hub(cli: &Cli, action: &HubAction) -> Result<ExitCode> {
    match action {
        HubAction::Publish(args) => publish(cli, args),
        HubAction::Clone(args) => clone(cli, args),
        HubAction::Fetch => fetch_command(cli),
        HubAction::Status => status_command(cli),
        HubAction::Whoami(args) => whoami(cli, args),
        HubAction::Autopublish(args) => autopublish(cli, args),
        HubAction::Show(args) => browse::show(cli, args),
        HubAction::Ls(args) => browse::ls(cli, args),
        HubAction::Cat(args) => browse::cat(cli, args),
        HubAction::Log(args) => history::log(cli, args),
        HubAction::Diff(args) => history::diff(cli, args),
        HubAction::Explore(args) => explore::explore(cli, args),
        HubAction::User(args) => explore::user(cli, args),
        HubAction::Star(args) => social::star(cli, args),
        HubAction::Unstar(args) => social::unstar(cli, args),
        HubAction::Stars(args) => social::stars(cli, args),
        HubAction::Watch(args) => social::watch(cli, args),
        HubAction::Unwatch(args) => social::unwatch(cli, args),
        HubAction::Follow(args) => social::follow(cli, args),
        HubAction::Unfollow(args) => social::unfollow(cli, args),
        HubAction::Issue(args) => issue::issue(cli, args),
        HubAction::Label(args) => issue::label(cli, args),
        HubAction::Inbox(args) => inbox::inbox(cli, args),
        HubAction::Fork(args) => fork::fork(cli, args),
        HubAction::SyncFork(args) => fork::sync_fork(cli, args),
        HubAction::Jobs(args) => fork::jobs(cli, args),
        HubAction::Request(args) => request::request(cli, args),
        HubAction::Github(args) => github::github(cli, args),
        HubAction::Settings(args) => settings::settings(cli, args),
        HubAction::Block(args) => settings::block(cli, args),
        HubAction::Unblock(args) => settings::unblock(cli, args),
        HubAction::Account(args) => settings::account(cli, args),
        HubAction::Api(args) => api::api(cli, args),
    }
}

/// A client for the hub at `url`, with the token the environment or the
/// credentials file give for it, if any.
fn client(env: &HubEnv, url: &str) -> Result<(HttpHub, TokenSource)> {
    let creds = env.credentials()?;
    let (token, source) = creds::token_for(env, &creds, url)?;
    let hub = HttpHub::new(url, token)?
        .with_max_body(env.max_body()?)
        .with_source(source)
        .with_withheld(creds::withheld_for(env, &creds, url));
    Ok((hub, source))
}

/// A client for the hub a repository is linked to.
pub(crate) fn connect(link: &Link) -> Result<HttpHub> {
    Ok(client(&HubEnv::from_process(), &link.url)?.0)
}

/// The hub `repo` lands on: its link, unless `local` or `BRAID_HUB=off`
/// keeps the command in this repository.
pub(crate) fn linked(repo: &Repo, local: bool) -> Result<Option<Link>> {
    if local || HubEnv::from_process().hub_off()? {
        return Ok(None);
    }
    Link::read(repo)
}

/// The link of a repository that must have one.
fn require_link(repo: &Repo) -> Result<Link> {
    Link::read(repo)?.context(
        "this repository is not linked to a hub: `braid hub publish` puts it on one, and `braid hub clone` makes a linked one",
    )
}

/// ` --hub <url>`, or nothing for the hub used by default.
fn hub_flag(url: &str) -> String {
    if url == braid_hubproto::DEFAULT_HUB_URL {
        String::new()
    } else {
        format!(" --hub {url}")
    }
}

pub(crate) fn login(cli: &Cli, args: &LoginArgs) -> Result<ExitCode> {
    let env = HubEnv::from_process();
    let path = env.credentials_path()?;
    let mut creds = Credentials::load(&path)?;
    let url = creds::hub_url(&env, &creds, args.hub.as_deref())?;
    let hub = HttpHub::new(&url, None)?;
    let start = hub.device_start()?;
    eprintln!(
        "To sign in to {url}, open {} and enter the code {}",
        remote::shown(&start.verification_uri),
        remote::shown(&start.user_code)
    );
    eprintln!(
        "Waiting for the approval; the code expires in {}.",
        count(start.expires_in.div_ceil(60), "minute")
    );
    let signed = device::wait_for_approval(&hub, &start, std::thread::sleep)?;
    let replaced = creds.hubs.insert(
        url.clone(),
        Saved {
            token: signed.token.expose(),
            login: signed.login.clone(),
            expires: signed.expires.clone(),
        },
    );
    creds.default = Some(url.clone());
    creds.save(&path)?;
    if env.token.is_some() {
        eprintln!("braid: note: BRAID_TOKEN is set, and commands use it rather than this token");
    }
    // The token this one replaces is revoked, so that signing in again
    // leaves no live token behind; one the hub refuses is gone already.
    let revoked_before = match replaced {
        Some(old) if old.token != signed.token.expose() => match revoke_token(&url, &old) {
            Ok(()) => true,
            Err(err) => {
                if !err
                    .downcast_ref::<HubError>()
                    .is_some_and(HubError::unauthorized)
                {
                    eprintln!(
                        "braid: note: the token saved before for {url} could not be revoked ({err:#}); it stops working when it expires"
                    );
                }
                false
            }
        },
        _ => false,
    };
    if cli.json {
        print_json(&json!({
            "hub": url,
            "login": signed.login,
            "expires": signed.expires,
        }))?;
    } else {
        println!(
            "Signed in to {url} as {}; the token expires {}",
            remote::shown(&signed.login),
            remote::shown(&signed.expires)
        );
        if revoked_before {
            println!("The token saved before was revoked on the hub.");
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn logout(cli: &Cli, args: &LoginArgs) -> Result<ExitCode> {
    let env = HubEnv::from_process();
    let path = env.credentials_path()?;
    let mut creds = Credentials::load(&path)?;
    let url = creds::hub_url(&env, &creds, args.hub.as_deref())?;
    if env.token.is_some() {
        eprintln!("braid: note: BRAID_TOKEN is set; logout leaves it alone");
    }
    let Some(saved) = creds.hubs.remove(&url) else {
        if cli.json {
            print_json(&json!({
                "hub": url,
                "signed_in": false,
                "revoked": false,
                "detail": null,
            }))?;
        } else {
            println!("Not signed in to {url}: no token is saved for it");
        }
        return Ok(ExitCode::SUCCESS);
    };
    let revoked = revoke(&url, &saved);
    if creds.default.as_deref() == Some(url.as_str()) {
        creds.default = None;
    }
    creds.save(&path)?;
    if cli.json {
        print_json(&json!({
            "hub": url,
            "signed_in": true,
            "revoked": revoked.is_ok(),
            "detail": revoked.as_ref().err(),
        }))?;
    } else {
        match &revoked {
            Ok(()) => {
                println!("Signed out of {url}: the hub revoked the token, and it was deleted here")
            }
            Err(why) => println!(
                "Signed out of {url}: the token was deleted here, but the hub did not revoke it ({why}); it stops working when it expires{}",
                if saved.expires.is_empty() {
                    String::new()
                } else {
                    format!(", {}", saved.expires)
                }
            ),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn whoami(cli: &Cli, args: &LoginArgs) -> Result<ExitCode> {
    let env = HubEnv::from_process();
    let creds = env.credentials()?;
    let url = creds::hub_url(&env, &creds, args.hub.as_deref())?;
    let (token, source) = creds::token_for(&env, &creds, &url)?;
    if token.is_none() {
        match creds::withheld_for(&env, &creds, &url) {
            Some(meant) => bail!(
                "no one is signed in to {url}: BRAID_TOKEN is for {meant}; set BRAID_HUB_URL={url} to use it with this hub, or run `braid login{}`",
                hub_flag(&url)
            ),
            None => bail!(
                "no one is signed in to {url}: run `braid login{}`, or set BRAID_TOKEN",
                hub_flag(&url)
            ),
        }
    }
    let hub = HttpHub::new(&url, token)?.with_source(source);
    let me = match hub.me() {
        Ok(me) => me,
        Err(HubError::Api { status: 401, body }) => match source {
            TokenSource::Environment => bail!(
                "the hub at {url} refused BRAID_TOKEN ({}): it expired or was revoked; set another, or unset it and run `braid login{}`",
                remote::shown(&body.detail),
                hub_flag(&url)
            ),
            _ => bail!(
                "the hub at {url} refused the token saved for it ({}): it expired or was revoked; run `braid login{}`",
                remote::shown(&body.detail),
                hub_flag(&url)
            ),
        },
        Err(err @ HubError::Refused { .. }) => return Err(err.into()),
        // Neither yes nor no: the hub did not say whether the token is
        // good, so signing in again would not help. Its own exit status
        // keeps `braid login` for when it does.
        Err(err) => {
            eprintln!(
                "braid: error: cannot tell who is signed in to {url}: {err}; try again once the hub answers"
            );
            return Ok(ExitCode::from(CANNOT_TELL));
        }
    };
    let expires = me.token.as_ref().map(|info| info.expires.clone());
    if cli.json {
        print_json(&json!({
            "hub": url,
            "login": me.login,
            "name": me.name,
            "expires": expires,
            "token": source.code(),
        }))?;
        return Ok(ExitCode::SUCCESS);
    }
    let from = match source {
        TokenSource::Environment => " (BRAID_TOKEN)",
        _ => "",
    };
    let login = remote::shown(&me.login);
    match expires {
        Some(expires) => println!(
            "Signed in to {url} as {login}{from}; the token expires {}",
            remote::shown(&expires)
        ),
        None => println!("Signed in to {url} as {login}{from}"),
    }
    Ok(ExitCode::SUCCESS)
}

/// The exit status of `hub whoami` when the hub did not say whether the
/// token is good: it could not be reached, failed, or did not answer as
/// the protocol says.
const CANNOT_TELL: u8 = 3;

fn autopublish(cli: &Cli, args: &AutopublishArgs) -> Result<ExitCode> {
    let env = HubEnv::from_process();
    let creds = match args.answer {
        Some(answer) => {
            let path = env.credentials_path()?;
            let mut creds = Credentials::load(&path)?;
            creds.autopublish = Some(answer == Switch::On);
            creds.save(&path)?;
            creds
        }
        None => env.credentials()?,
    };
    let answer = match creds.autopublish {
        Some(true) => "on",
        Some(false) => "off",
        None => "unset",
    };
    if cli.json {
        print_json(&json!({ "autopublish": answer }))?;
    } else {
        println!("{answer}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Revokes a saved token on its hub, saying why it could not.
fn revoke(url: &str, saved: &Saved) -> Result<(), String> {
    revoke_token(url, saved).map_err(|err| format!("{err:#}"))
}

/// Revokes a saved token on its hub: `GET /v1/me` names its id, and
/// `DELETE /v1/tokens/{id}` revokes it.
fn revoke_token(url: &str, saved: &Saved) -> Result<()> {
    let token = Token::parse(&saved.token)
        .map_err(|_| anyhow::anyhow!("the saved token is not a braid hub token"))?;
    let own_id = token.id().to_owned();
    let hub = HttpHub::new(url, Some(token))?;
    let me = hub.me()?;
    let id = me.token.map_or(own_id, |info| info.id);
    hub.delete_token(&id)?;
    Ok(())
}

/// A repository name made from `text`, such as a directory's name: every
/// character other than ASCII letters, digits, `.`, `_` and `-` becomes
/// `-`, a run of `-` one, and a leading `.` or `-`, a trailing `-` and a
/// trailing `.git` go; at most 100 characters stay.
pub(crate) fn repo_name_from(text: &str) -> String {
    let mut name = String::new();
    for c in text.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            c
        } else {
            '-'
        };
        if !(c == '-' && name.ends_with('-')) {
            name.push(c);
        }
    }
    let mut name = name.trim_start_matches(['.', '-']).to_owned();
    loop {
        let trimmed = name.trim_end_matches('-').len();
        name.truncate(trimmed);
        if name.to_ascii_lowercase().ends_with(".git") {
            name.truncate(name.len() - 4);
        } else {
            break;
        }
    }
    // Only ASCII is left, so this cuts between characters.
    name.truncate(100);
    name
}

fn publish(cli: &Cli, args: &PublishArgs) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let env = HubEnv::from_process();
    let existing = Link::read(&repo)?;
    let creds = env.credentials()?;
    let url = match &existing {
        Some(link) => link.url.clone(),
        None => creds::hub_url(&env, &creds, None)?,
    };
    let (token, _) = creds::token_for(&env, &creds, &url)?;
    if token.is_none() {
        bail!(
            "publishing needs signing in to {url}: run `braid login{}`, or set BRAID_TOKEN",
            hub_flag(&url)
        );
    }
    if let Some(mirror) = &args.mirror
        && let Some((_, rest)) = mirror.split_once("://")
        && rest
            .split('/')
            .next()
            .is_some_and(|host| host.contains('@'))
    {
        bail!(
            "the mirror URL holds a user name or password: give it without, as the hub pushes with the account's GitHub access"
        );
    }
    let hub = HttpHub::new(&url, token)?.with_max_body(env.max_body()?);
    let (link, created) = match existing {
        Some(link) => {
            if args.public || args.description.is_some() || args.mirror.is_some() {
                bail!(
                    "this repository is published already, as {} on {}: --public, --description and --mirror apply only when it is created",
                    link.repo,
                    link.url
                );
            }
            if let Some(name) = &args.name
                && *name != link.repo.name
            {
                bail!(
                    "this repository is published already, as {} on {}",
                    link.repo,
                    link.url
                );
            }
            (link, false)
        }
        None => {
            let name = match &args.name {
                Some(name) if is_repo_name(name) => name.clone(),
                Some(name) => bail!(
                    "{name:?} is not a repository name: 1 to 100 of letters, digits, '.', '_' and '-', not starting with '.' or ending with \".git\""
                ),
                None => {
                    let root =
                        fs::canonicalize(repo.root()).unwrap_or_else(|_| repo.root().to_path_buf());
                    let dir = root
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let name = repo_name_from(&dir);
                    if !is_repo_name(&name) {
                        bail!(
                            "cannot make a repository name from the directory's name {dir:?}: give NAME"
                        );
                    }
                    name
                }
            };
            let made = hub.create_repo(&RepoCreate {
                name: name.clone(),
                private: !args.public,
                description: args.description.clone().unwrap_or_default(),
                import: None,
                mirror: args.mirror.clone(),
            });
            let info = match made {
                Ok(info) => info,
                Err(HubError::Api { status: 409, body }) if body.error == "exists" => {
                    taken_up(&hub, &name)?
                }
                Err(err) if err.may_have_happened() => {
                    return Err(anyhow::Error::new(err).context(format!(
                        "the hub may have made {name} before its answer was lost: run `braid hub publish` again, which takes it up if it holds nothing yet"
                    )));
                }
                Err(err) => return Err(err.into()),
            };
            let path = RepoPath::new(&info.owner, &info.name)
                .context("the hub named the new repository in a way braid cannot read")?;
            let link = Link {
                url: url.clone(),
                repo: path,
                paths: None,
            };
            link.write(&repo)?;
            (link, true)
        }
    };
    let remote = hub.repo(link.repo.clone());
    let published = publish::publish_refs(&repo, &remote, &cli.agent).with_context(|| {
        if created {
            format!(
                "{} was created on the hub and linked here, but not all of it is there: run `braid hub publish` again",
                link.repo
            )
        } else {
            format!("cannot publish to {}", link.repo)
        }
    })?;
    // Where the hub's refs are now, for `braid hub status`.
    fetch::fetch(&repo, &remote, &link, &cli.agent, None)?;

    let address = format!("{}/{}", link.url, link.repo);
    if cli.json {
        print_json(&json!({
            "hub": link.url,
            "repo": link.repo.to_string(),
            "url": address,
            "created": created,
            "objects": published.sent.objects,
            "bytes": published.sent.bytes,
            "refs": published.refs.iter().map(|r| json!({
                "name": r.name,
                "id": r.id.to_hex(),
                "outcome": r.outcome,
                "head_before": hex(r.head_before),
                "head": hex(r.head),
            })).collect::<Vec<_>>(),
            "skipped": published.skipped.iter().map(|(name, reason)| json!({
                "name": name,
                "reason": reason,
            })).collect::<Vec<_>>(),
        }))?;
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{} {} at {address}: {} uploaded ({})",
        if created { "Published" } else { "Updated" },
        link.repo,
        count(published.sent.objects, "object"),
        count(published.sent.bytes, "byte")
    );
    for r in &published.refs {
        let what = match (r.outcome.as_str(), r.head_before) {
            ("fast-forward", None) => format!("created at {}", r.id.short()),
            ("fast-forward", Some(before)) => format!("{} -> {}", before.short(), r.id.short()),
            ("already-applied", _) => format!("on the hub already, at {}", short(r.head)),
            ("refused", _) if is_tag(&r.name) => format!(
                "kept: the hub's names {}, and a tag never moves",
                short(r.head)
            ),
            ("refused", _) => format!(
                "refused: the hub's is at {}, which is not on this one's first-parent chain; `braid hub fetch` brings it in",
                short(r.head)
            ),
            (other, _) => other.to_owned(),
        };
        println!("  {:<24} {what}", remote::shown(&r.name));
    }
    for (name, reason) in &published.skipped {
        println!("  {:<24} skipped: {reason}", remote::shown(name));
    }
    if created {
        println!(
            "Linked: `braid land` and `braid update` go through the hub now (--local keeps them here)."
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// The caller's repository `name`, which the hub says exists already: one
/// an earlier `braid hub publish` made before its answer was lost is taken
/// up when the caller owns it and it holds no ref yet. Any other is
/// refused, so that nothing is linked to, or uploaded into, a repository
/// with a history of its own.
fn taken_up(hub: &HttpHub, name: &str) -> Result<braid_hubproto::api::RepoInfo> {
    let me = hub.me()?;
    let path = RepoPath::new(&me.login, name)
        .with_context(|| format!("the hub names the account {:?}", remote::shown(&me.login)))?;
    let repository = hub.repo(path.clone());
    let info = repository.info()?;
    let refs = repository.refs()?;
    if !info.owner.eq_ignore_ascii_case(&me.login) || !refs.refs.is_empty() {
        bail!(
            "{path} exists on the hub already{}: give another NAME, or, if it is yours and holds nothing you need, delete it on the hub first",
            if refs.refs.is_empty() {
                ""
            } else {
                ", with refs of its own"
            }
        );
    }
    Ok(info)
}

/// What `hub clone` did to the directory before making the repository.
#[derive(Clone, Copy, Debug)]
enum Made {
    Created,
    WasEmpty,
}

/// Makes `dir`, or checks that it is empty.
fn prepare(dir: &Path) -> Result<Made> {
    match fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                bail!("{} exists and is not empty", dir.display());
            }
            Ok(Made::WasEmpty)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
            Ok(Made::Created)
        }
        Err(err) => Err(err).with_context(|| format!("cannot read {}", dir.display())),
    }
}

/// Takes back what a failed clone made in `dir`.
fn unmake(dir: &Path, made: Made) {
    match made {
        Made::Created => {
            let _ = fs::remove_dir_all(dir);
        }
        Made::WasEmpty => {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let _ = match entry.file_type() {
                        Ok(kind) if kind.is_dir() => fs::remove_dir_all(&path),
                        _ => fs::remove_file(&path),
                    };
                }
            }
        }
    }
}

/// A workspace a clone made.
struct NewWorkspace {
    name: String,
    path: PathBuf,
    branch: String,
    base: Option<braid_format::ObjectId>,
    files: usize,
}

/// The branch a clone's first workspace tracks: the hub's default branch
/// when the repository has it, or has no branch at all; else `main`,
/// `master`, its only branch, or its first.
fn first_branch(names: &[&str], default_branch: &str) -> String {
    if names.contains(&default_branch)
        || (names.is_empty() && braid_git::is_branch_name(default_branch))
    {
        return default_branch.to_owned();
    }
    ["main", "master"]
        .into_iter()
        .find(|preferred| names.contains(preferred))
        .or_else(|| names.first().copied())
        .unwrap_or("main")
        .to_owned()
}

/// Makes a clone's first workspace, the way `braid workspace add` makes
/// one: in `.braid/work/<name>`, at the head of the branch it tracks.
fn first_workspace(cli: &Cli, repo: &Repo, default_branch: &str) -> Result<NewWorkspace> {
    let refs = repo.store().refs()?;
    let branch = first_branch(&branches(&refs), default_branch);
    let name = match &cli.workspace {
        Some(name) => name.clone(),
        None if WorkspaceLink::is_valid_name(&branch) => branch.clone(),
        None => "work".to_owned(),
    };
    let base = refs.get(&branch);
    let dir = Workspace::default_dir(repo, &name);
    let (workspace, files) = Workspace::create(repo, &name, &dir, base, &branch)?;
    Ok(NewWorkspace {
        name,
        path: workspace.dir().to_path_buf(),
        branch,
        base,
        files,
    })
}

fn clone(cli: &Cli, args: &CloneArgs) -> Result<ExitCode> {
    let path: RepoPath = args.source.parse()?;
    let paths = if args.paths.is_empty() {
        None
    } else {
        Some(
            args.paths
                .iter()
                .map(|path| link::path_prefix(path))
                .collect::<Result<Vec<_>>>()?,
        )
    };
    let env = HubEnv::from_process();
    let creds = env.credentials()?;
    let url = creds::hub_url(&env, &creds, None)?;
    let (hub, _) = client(&env, &url)?;
    let remote = hub.repo(path.clone());
    // Asked first, so that nothing is made for a repository the hub does
    // not show.
    let info = remote.info()?;
    let dir = args
        .dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(&path.name));
    let made = prepare(&dir)?;
    let link = Link {
        url: url.clone(),
        repo: path,
        paths,
    };
    let cloned = (|| -> Result<_> {
        let repo = Repo::init_with(&dir, cli.store_options())?;
        link.write(&repo)?;
        let fetched = fetch::fetch(&repo, &remote, &link, &cli.agent, None)?;
        let workspace = match link.paths {
            Some(_) => None,
            None => Some(first_workspace(cli, &repo, &info.default_branch)?),
        };
        let root = fs::canonicalize(repo.root()).unwrap_or_else(|_| repo.root().to_path_buf());
        Ok((root, fetched, workspace))
    })();
    let (root, fetched, workspace) = match cloned {
        Ok(cloned) => cloned,
        Err(err) => {
            unmake(&dir, made);
            return Err(err);
        }
    };
    if cli.json {
        print_json(&json!({
            "hub": link.url,
            "repo": link.repo.to_string(),
            "root": root.to_string_lossy(),
            "paths": link.paths,
            "objects": fetched.objects,
            "refs": refs_json(&fetched),
            "workspace": workspace.as_ref().map(|ws| json!({
                "name": ws.name,
                "path": ws.path.to_string_lossy(),
                "branch": ws.branch,
                "base": hex(ws.base),
                "files": ws.files,
            })),
        }))?;
        return Ok(ExitCode::SUCCESS);
    }
    let branch_count = fetched
        .refs
        .iter()
        .filter(|r| !is_tag(&r.name) && r.after.is_some())
        .count();
    let tag_count = fetched
        .refs
        .iter()
        .filter(|r| is_tag(&r.name) && r.after.is_some())
        .count();
    println!(
        "Cloned {} from {} into {}: {}, {} and {}",
        link.repo,
        link.url,
        root.display(),
        count(fetched.objects, "object"),
        count(branch_count as u64, "branch"),
        count(tag_count as u64, "tag")
    );
    print_skipped(&fetched);
    match (&workspace, &link.paths) {
        (Some(ws), _) => println!(
            "Workspace {} in {} on {} at {}, {}",
            ws.name,
            ws.path.display(),
            remote::shown(&ws.branch),
            short(ws.base),
            count(ws.files as u64, "file")
        ),
        (None, Some(paths)) => println!(
            "Sparse: only the trees under {} came, so no workspace was made (a workspace is a whole checkout)",
            remote::shown(&paths.join(", "))
        ),
        (None, None) => {}
    }
    Ok(ExitCode::SUCCESS)
}

/// The refs of a fetch, as JSON.
fn refs_json(fetched: &Fetched) -> Vec<Value> {
    fetched
        .refs
        .iter()
        .map(|r| {
            let mut value = json!({
                "name": r.name,
                "hub": r.hub.to_hex(),
                "before": hex(r.before),
                "after": hex(r.after),
                "outcome": r.outcome.code(),
            });
            if let Outcome::Skipped(reason) = r.outcome {
                value["reason"] = Value::String(reason.to_owned());
            }
            value
        })
        .collect()
}

/// What became of a ref, for people.
fn describe(r: &RefFetched) -> String {
    match r.outcome {
        Outcome::Created => format!("created at {}", r.hub.short()),
        Outcome::Moved => format!("{} -> {}", short(r.before), r.hub.short()),
        Outcome::Same => format!("up to date at {}", r.hub.short()),
        Outcome::Ahead => format!(
            "kept at {}: it has changesets the hub does not have, on top of the hub's {}; `braid land` from a workspace on it sends them",
            short(r.before),
            r.hub.short()
        ),
        Outcome::Diverged if is_tag(&r.name) => format!(
            "kept at {}: the hub's names {}, and a tag never moves",
            short(r.before),
            r.hub.short()
        ),
        Outcome::Diverged => format!(
            "kept at {}: it and the hub's, at {}, each have changesets the other does not",
            short(r.before),
            r.hub.short()
        ),
        Outcome::Skipped(reason) => format!("skipped: {reason}"),
    }
}

fn print_skipped(fetched: &Fetched) {
    for r in &fetched.refs {
        if matches!(r.outcome, Outcome::Skipped(_)) {
            println!("  {:<24} {}", remote::shown(&r.name), describe(r));
        }
    }
}

fn fetch_command(cli: &Cli) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let link = require_link(&repo)?;
    let hub = connect(&link)?;
    let remote = hub.repo(link.repo.clone());
    let fetched = fetch::fetch(&repo, &remote, &link, &cli.agent, None)?;
    if cli.json {
        print_json(&json!({
            "hub": link.url,
            "repo": link.repo.to_string(),
            "seq": fetched.seq,
            "objects": fetched.objects,
            "operation": fetched.operation,
            "refs": refs_json(&fetched),
        }))?;
        return Ok(ExitCode::SUCCESS);
    }
    let changed: Vec<&RefFetched> = fetched
        .refs
        .iter()
        .filter(|r| r.outcome != Outcome::Same)
        .collect();
    if fetched.objects == 0 && changed.is_empty() {
        println!("Up to date with {}", link.repo);
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "Fetched {} from {}",
        count(fetched.objects, "object"),
        link.repo
    );
    for r in changed {
        println!("  {:<24} {}", remote::shown(&r.name), describe(r));
    }
    Ok(ExitCode::SUCCESS)
}

fn status_command(cli: &Cli) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let link = require_link(&repo)?;
    let env = HubEnv::from_process();
    let (hub, source) = client(&env, &link.url)?;
    let login = if hub.has_token() {
        Some(hub.me().map(|me| me.login))
    } else {
        None
    };
    let remote = hub.repo(link.repo.clone());
    let info = remote.info()?;
    let refs = remote.refs()?;
    let states = status::compare(&repo, &refs.refs)?;
    if cli.json {
        print_json(&json!({
            "hub": link.url,
            "repo": link.repo.to_string(),
            "paths": link.paths,
            "login": login.as_ref().and_then(|login| login.as_ref().ok()),
            "token": source.code(),
            "mirror": info.mirror,
            "seq": refs.seq,
            "branches": states.iter().map(|s| json!({
                "name": s.name,
                "local": hex(s.local),
                "hub": hex(s.hub),
                "state": s.state.code(),
            })).collect::<Vec<_>>(),
        }))?;
        return Ok(ExitCode::SUCCESS);
    }
    match &link.paths {
        Some(paths) => println!(
            "Linked to {} on {}, sparse: {}",
            link.repo,
            link.url,
            paths.join(", ")
        ),
        None => println!("Linked to {} on {}", link.repo, link.url),
    }
    let from = match source {
        TokenSource::Environment => " (BRAID_TOKEN)",
        _ => "",
    };
    match &login {
        Some(Ok(login)) => println!("Signed in as {}{from}", remote::shown(login)),
        Some(Err(err)) => println!("Signed in, but {err}"),
        None => println!(
            "Not signed in: public repositories only (`braid login{}` signs in)",
            hub_flag(&link.url)
        ),
    }
    if let Some(mirror) = &info.mirror {
        let state = match (&mirror.outcome, &mirror.last_sync) {
            (Some(outcome), Some(when)) => format!("{outcome} at {when}"),
            (Some(outcome), None) => outcome.clone(),
            _ => "not synced yet".to_owned(),
        };
        println!(
            "Mirror of {}: {} ({})",
            remote::shown(&info.default_branch),
            remote::shown(&mirror.url),
            remote::shown(&state)
        );
        if let Some(detail) = &mirror.detail {
            println!("  {}", remote::shown(detail));
        }
    }
    for s in &states {
        let heads = match (s.local, s.hub) {
            (Some(here), Some(there)) if here == there => here.short(),
            (here, there) => format!("here {}, hub {}", short(here), short(there)),
        };
        println!(
            "  {:<24} {:<9} {heads}",
            remote::shown(&s.name),
            s.state.code()
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_directories() {
        for (dir, name) in [
            ("demo", "demo"),
            ("My Project", "My-Project"),
            (".hidden", "hidden"),
            ("repo.git", "repo"),
            ("a.git.git", "a"),
            ("x  y", "x-y"),
            ("ñandú", "and"),
            ("trailing-", "trailing"),
        ] {
            assert_eq!(repo_name_from(dir), name, "{dir}");
            assert!(is_repo_name(name));
        }
        assert_eq!(repo_name_from(&"a".repeat(150)).len(), 100);
        assert_eq!(repo_name_from("???"), "");
    }

    #[test]
    fn the_first_workspace_tracks_the_default_branch() {
        assert_eq!(first_branch(&["dev", "trunk"], "trunk"), "trunk");
        assert_eq!(first_branch(&[], "trunk"), "trunk");
        assert_eq!(first_branch(&["dev", "master"], "trunk"), "master");
        assert_eq!(first_branch(&["dev", "main", "master"], "trunk"), "main");
        assert_eq!(first_branch(&["dev", "zeta"], "trunk"), "dev");
    }
}
