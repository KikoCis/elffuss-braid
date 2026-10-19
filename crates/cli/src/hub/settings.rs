// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub settings`, `block`, `unblock` and `account`.
//!
//! `settings` shows a repository's settings, or changes those its flags
//! name, as `PATCH /v1/r/{owner}/{name}` does (section 4 of `docs/hub.md`):
//! its admins see and change them all. `block` and `unblock` block an
//! account from every repository of the account signed in, or, with `-R`,
//! from one repository it administers (section 22). `account` shows the
//! account's own settings, or changes them (section 3). Exit statuses as
//! the other hub commands: 0 done, 1 refused, 3 when the hub could not be
//! reached or failed.

use std::fmt::Write;
use std::process::ExitCode;

use anyhow::{Result, bail};
use braid_hubproto::api::{Me, RepoInfo, RepoPatch};
use braid_hubproto::moderation::{MePatch, TDM_ALLOWED, TDM_RESERVED};
use braid_hubproto::names::is_owner;
use clap::{Args, ValueEnum};
use serde_json::json;

use super::creds::{self, HubEnv};
use super::http::HttpHub;
use super::remote::{HubError, shown};
use super::social_common::{HubRepoArg, exit_for, hub_repo, print};
use crate::Cli;

/// What making a repository public shows, said when it is done.
const PUBLIC_NOTE: &str = "everything it ever stored is readable now by anyone who knows its id, not only what its branches hold: its whole history, branches deleted since, uploads that never landed, and its issues and change requests";

/// The hub signed in to, for the commands about the account: BRAID_HUB_URL,
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

/// A login given on the command line, checked as the hub checks it.
fn login_arg(login: &str) -> Result<String> {
    if !is_owner(login) {
        bail!(
            "{login:?} is not a login: 1 to 39 letters, digits and hyphens, with no hyphen at either end and no two in a row"
        );
    }
    Ok(login.to_ascii_lowercase())
}

/// On or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Switch {
    On,
    Off,
}

impl Switch {
    fn on(self) -> bool {
        self == Switch::On
    }
}

/// Text and data mining of a repository's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Mining {
    /// Anyone may mine it.
    Allowed,
    /// Its owner reserves it.
    Reserved,
}

impl Mining {
    fn as_str(self) -> &'static str {
        match self {
            Mining::Allowed => TDM_ALLOWED,
            Mining::Reserved => TDM_RESERVED,
        }
    }
}

/// Who may open change requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum RequestsFrom {
    /// Anyone who reads the repository.
    Readers,
    /// Its collaborators only.
    Collaborators,
    /// Nobody.
    Off,
}

impl RequestsFrom {
    fn as_str(self) -> &'static str {
        match self {
            RequestsFrom::Readers => "readers",
            RequestsFrom::Collaborators => "collaborators",
            RequestsFrom::Off => "off",
        }
    }
}

/// How the hub takes in what is pushed to a repository's GitHub mirror.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Follow {
    /// It publishes each landing, and takes nothing in by itself.
    Off,
    /// It also takes in what is pushed there while it has nothing new.
    FfOnly,
    /// It takes it in, merging when both sides moved.
    Merge,
}

impl Follow {
    fn as_str(self) -> &'static str {
        match self {
            Follow::Off => "off",
            Follow::FfOnly => "ff-only",
            Follow::Merge => "merge",
        }
    }
}

/// `hub settings`.
#[derive(Debug, Args)]
pub(crate) struct SettingsArgs {
    #[command(flatten)]
    pub(crate) repo: HubRepoArg,
    /// Make it readable by anyone: everything it ever stored becomes
    /// readable by id, not only what its branches hold now.
    #[arg(long, conflicts_with = "private")]
    pub(crate) public: bool,
    /// Make it readable only by its owner and those given access.
    #[arg(long)]
    pub(crate) private: bool,
    /// A new description.
    #[arg(long, value_name = "TEXT")]
    pub(crate) description: Option<String>,
    /// A new default branch.
    #[arg(long, value_name = "BRANCH")]
    pub(crate) default_branch: Option<String>,
    /// Its topics, separated by commas, in place of the old ones ("" for
    /// none).
    #[arg(long, value_name = "TOPICS")]
    pub(crate) topics: Option<String>,
    /// Text and data mining of its content.
    #[arg(long, value_enum)]
    pub(crate) tdm: Option<Mining>,
    /// Whether it takes issues.
    #[arg(long, value_enum, value_name = "ON|OFF")]
    pub(crate) issues: Option<Switch>,
    /// Who may open change requests.
    #[arg(long, value_enum)]
    pub(crate) requests_from: Option<RequestsFrom>,
    /// Whether it may be forked.
    #[arg(long, value_enum, value_name = "ON|OFF")]
    pub(crate) forks: Option<Switch>,
    /// The approvals a change request needs before it lands, 0 to 10.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(0..=10))]
    pub(crate) approvals: Option<u32>,
    /// How what is pushed to its GitHub mirror is taken in.
    #[arg(long, value_enum)]
    pub(crate) follow: Option<Follow>,
}

impl SettingsArgs {
    /// The change the flags ask for: none when they name nothing.
    fn patch(&self) -> RepoPatch {
        let private = match (self.public, self.private) {
            (true, _) => Some(false),
            (_, true) => Some(true),
            _ => None,
        };
        RepoPatch {
            private,
            description: self.description.clone(),
            default_branch: self.default_branch.clone(),
            topics: self.topics.as_deref().map(|topics| {
                topics
                    .split(',')
                    .map(str::trim)
                    .filter(|topic| !topic.is_empty())
                    .map(str::to_owned)
                    .collect()
            }),
            tdm: self.tdm.map(|tdm| tdm.as_str().to_owned()),
            issues: self.issues.map(Switch::on),
            requests_from: self.requests_from.map(|from| from.as_str().to_owned()),
            allow_forks: self.forks.map(Switch::on),
            require_approvals: self.approvals,
            mirror_follow: self.follow.map(|follow| follow.as_str().to_owned()),
            ..RepoPatch::default()
        }
    }
}

pub(crate) fn settings(cli: &Cli, args: &SettingsArgs) -> Result<ExitCode> {
    let (hub, path) = hub_repo(cli, args.repo.hub_repo.as_deref())?;
    let patch = args.patch();
    let route = path.api_path();
    let asked = if patch == RepoPatch::default() {
        hub.call_json::<RepoInfo>("GET", &route, &[], None)
    } else {
        let body = serde_json::to_value(&patch)?;
        hub.call_json::<RepoInfo>("PATCH", &route, &[], Some(&body))
    };
    let info = match asked {
        Ok(info) => info,
        Err(err) => return Ok(failed(&err)),
    };
    if args.public && !info.private {
        eprintln!("braid: {path} is public: {PUBLIC_NOTE}.");
    }
    print(cli, &serde_json::to_value(&info)?, || settings_text(&info))?;
    Ok(ExitCode::SUCCESS)
}

/// "on" or "off".
fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// What `settings` prints without `--json`. What the hub says goes
/// through [`shown`]: names and descriptions are anyone's text.
fn settings_text(info: &RepoInfo) -> String {
    let mut out = format!(
        "{}/{} ({})",
        shown(&info.owner),
        shown(&info.name),
        if info.private { "private" } else { "public" }
    );
    if !info.description.is_empty() {
        let _ = write!(out, "\nDescription: {}", shown(&info.description));
    }
    let _ = write!(out, "\nDefault branch: {}", shown(&info.default_branch));
    let topics = if info.topics.is_empty() {
        "none".to_owned()
    } else {
        shown(&info.topics.join(", "))
    };
    let _ = write!(out, "\nTopics: {topics}");
    if !info.tdm.is_empty() {
        let _ = write!(out, "\nText and data mining: {}", shown(&info.tdm));
    }
    match &info.mirror {
        Some(mirror) => {
            let _ = write!(out, "\nMirror: {}", shown(&mirror.url));
        }
        None => out.push_str("\nMirror: none"),
    }
    if let Some(settings) = &info.settings {
        let _ = write!(
            out,
            "\nIssues: {}\nChange requests from: {}\nForks: {}\nApprovals needed to land: {}",
            on_off(settings.issues),
            shown(&settings.requests_from),
            on_off(settings.allow_forks),
            settings.require_approvals
        );
        if let Some(follow) = &settings.mirror_follow {
            let _ = write!(out, "\nMirror followed: {}", shown(follow));
        }
    }
    out
}

/// `hub block`.
#[derive(Debug, Args)]
pub(crate) struct BlockArgs {
    /// The account to block.
    #[arg(value_name = "LOGIN")]
    pub(crate) login: String,
    /// Block it from this repository alone, OWNER/NAME, which you
    /// administer (default: from every repository you own).
    #[arg(short = 'R', long = "hub-repo", value_name = "OWNER/NAME")]
    pub(crate) hub_repo: Option<String>,
}

pub(crate) fn block(cli: &Cli, args: &BlockArgs) -> Result<ExitCode> {
    block_or_not(cli, &args.login, args.hub_repo.as_deref(), true)
}

/// `hub unblock`.
#[derive(Debug, Args)]
pub(crate) struct UnblockArgs {
    /// The account to unblock.
    #[arg(value_name = "LOGIN")]
    pub(crate) login: String,
    /// Lift the block of this repository, OWNER/NAME, which you administer
    /// (default: your own block, on every repository you own).
    #[arg(short = 'R', long = "hub-repo", value_name = "OWNER/NAME")]
    pub(crate) hub_repo: Option<String>,
}

pub(crate) fn unblock(cli: &Cli, args: &UnblockArgs) -> Result<ExitCode> {
    block_or_not(cli, &args.login, args.hub_repo.as_deref(), false)
}

/// Blocks `login`, or lifts its block: the account's own, or, with
/// `repo`, that repository's.
fn block_or_not(cli: &Cli, login: &str, repo: Option<&str>, block: bool) -> Result<ExitCode> {
    let login = login_arg(login)?;
    let (hub, route, repo) = match repo {
        Some(text) => {
            let (hub, path) = hub_repo(cli, Some(text))?;
            let route = format!("{}/blocks/{login}", path.api_path());
            (hub, route, Some(path.to_string()))
        }
        None => (signed_in_hub()?, format!("/v1/me/blocks/{login}"), None),
    };
    let method = if block { "PUT" } else { "DELETE" };
    if let Err(err) = hub.call(method, &route, &[], None) {
        return Ok(failed(&err));
    }
    let mut value = json!({"login": login, "blocked": block});
    if let Some(repo) = &repo {
        value["repo"] = json!(repo);
    }
    print(cli, &value, || match (block, &repo) {
        (true, None) => format!("Blocked {login} from every repository you own."),
        (true, Some(repo)) => format!("Blocked {login} from {repo}."),
        (false, None) => format!("Unblocked {login}."),
        (false, Some(repo)) => format!("Unblocked {login} on {repo}."),
    })?;
    Ok(ExitCode::SUCCESS)
}

/// `hub account`.
#[derive(Debug, Args)]
pub(crate) struct AccountArgs {
    /// What text and data mining your new repositories start with.
    #[arg(long, value_enum)]
    pub(crate) tdm_default: Option<Mining>,
    /// Whether the repositories you starred show on your profile.
    #[arg(long, value_enum, value_name = "ON|OFF")]
    pub(crate) stars_public: Option<Switch>,
}

pub(crate) fn account(cli: &Cli, args: &AccountArgs) -> Result<ExitCode> {
    let hub = signed_in_hub()?;
    let patch = MePatch {
        tdm_default: args.tdm_default.map(|tdm| tdm.as_str().to_owned()),
        stars_public: args.stars_public.map(Switch::on),
    };
    let asked = if patch == MePatch::default() {
        hub.call_json::<Me>("GET", "/v1/me", &[], None)
    } else {
        let body = serde_json::to_value(&patch)?;
        hub.call_json::<Me>("PATCH", "/v1/me", &[], Some(&body))
    };
    let me = match asked {
        Ok(me) => me,
        Err(err) => return Ok(failed(&err)),
    };
    print(cli, &serde_json::to_value(&me)?, || account_text(&me))?;
    Ok(ExitCode::SUCCESS)
}

/// What `account` prints without `--json`.
fn account_text(me: &Me) -> String {
    let mut out = shown(&me.login);
    if let Some(settings) = &me.settings {
        let _ = write!(
            out,
            "\nStars: {}\nNew repositories start with text and data mining {}",
            if settings.stars_public {
                "shown on your profile"
            } else {
                "kept private"
            },
            shown(&settings.tdm_default)
        );
    }
    match &me.github_scope {
        Some(scope) if me.github_repo_access => {
            let _ = write!(out, "\nGitHub access: granted ({})", shown(scope));
        }
        _ => out.push_str("\nGitHub access: not granted"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_hubproto::api::{MeSettings, RepoSettingsInfo};

    #[test]
    fn flags_make_the_patch() {
        let args = SettingsArgs {
            repo: HubRepoArg::default(),
            public: false,
            private: true,
            description: None,
            default_branch: None,
            topics: Some(" rust, ,cli ".into()),
            tdm: Some(Mining::Reserved),
            issues: Some(Switch::Off),
            requests_from: Some(RequestsFrom::Collaborators),
            forks: Some(Switch::On),
            approvals: Some(2),
            follow: Some(Follow::FfOnly),
        };
        let patch = args.patch();
        assert_eq!(patch.private, Some(true));
        assert_eq!(
            patch.topics.as_deref(),
            Some(&["rust".to_owned(), "cli".to_owned()][..])
        );
        assert_eq!(patch.tdm.as_deref(), Some("reserved"));
        assert_eq!(patch.issues, Some(false));
        assert_eq!(patch.requests_from.as_deref(), Some("collaborators"));
        assert_eq!(patch.allow_forks, Some(true));
        assert_eq!(patch.require_approvals, Some(2));
        assert_eq!(patch.mirror_follow.as_deref(), Some("ff-only"));
        let none = SettingsArgs {
            private: false,
            topics: None,
            tdm: None,
            issues: None,
            requests_from: None,
            forks: None,
            approvals: None,
            follow: None,
            ..args
        };
        assert_eq!(none.patch(), RepoPatch::default());
        // "" clears the topics.
        let cleared = SettingsArgs {
            topics: Some(String::new()),
            ..none
        };
        assert_eq!(cleared.patch().topics, Some(Vec::new()));
    }

    #[test]
    fn texts_escape_what_the_hub_says() {
        let info = RepoInfo {
            owner: "ada".into(),
            name: "demo".into(),
            description: "bell\u{7}".into(),
            default_branch: "main".into(),
            settings: Some(RepoSettingsInfo {
                issues: false,
                requests_from: "readers".into(),
                allow_forks: true,
                require_approvals: 1,
                mirror_follow: None,
            }),
            ..RepoInfo::default()
        };
        let text = settings_text(&info);
        assert!(text.starts_with("ada/demo (public)"), "{text}");
        assert!(!text.contains('\u{7}'), "{text}");
        assert!(
            text.contains("Issues: off") && text.contains("Forks: on"),
            "{text}"
        );
        assert!(text.contains("Topics: none"), "{text}");
        let me = Me {
            login: "ada".into(),
            name: None,
            avatar_url: None,
            github_repo_access: false,
            token: None,
            settings: Some(MeSettings {
                tdm_default: "reserved".into(),
                stars_public: false,
            }),
            github_scope: None,
        };
        let text = account_text(&me);
        assert!(text.contains("Stars: kept private"), "{text}");
        assert!(text.contains("mining reserved"), "{text}");
        assert!(text.ends_with("GitHub access: not granted"), "{text}");
        assert_eq!(on_off(true), "on");
        assert!(login_arg("bad--login").is_err());
        assert_eq!(login_arg("Eve").unwrap(), "eve");
    }
}
