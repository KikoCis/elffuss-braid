// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid hub request`: change requests (section 19 of `docs/hub.md`) from
//! the command line.
//!
//! - `create` opens one: from the workspace's changesets, which it first
//!   puts on a new branch on the hub, or from a branch already there.
//! - `list`, `show` and `diff` read them; `comment` and `review` answer
//!   them.
//! - `update` gives one a new revision, from the workspace as it is now.
//! - `land` lands the snapshot that was read, and nothing else.
//! - `close`, `reopen` and `ready` change their state.
//! - `checkout` brings one's snapshot into this repository as a branch,
//!   through the request itself, so that whoever reads the repository it
//!   is on fetches it without reading the one it comes from.
//!
//! The repository is `-R OWNER/NAME`, else the one this repository is
//! linked to or, when that one is a fork, the repository it was forked
//! from, where its change requests go. Creating commands send an
//! `Idempotency-Key`, and send the request once more with the same key
//! when the answer was lost, so that nothing is made twice. Exit statuses:
//! 0 done, 1 refused, 2 conflicts, 3 when the hub could not be reached or
//! failed. Titles, texts, logins and names come from anyone: printed
//! without `--json`, they go through [`shown`].

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use braid_engine::read_changeset;
use braid_format::{Kind, ObjectId, Objects};
use braid_hubproto::RepoPath;
use braid_hubproto::api::{LandMode, LandRequest, Me, MissingRequest, RepoInfo};
use braid_hubproto::history::{ChangesetSummary, DiffPage};
use braid_hubproto::names::glob_matches;
use braid_hubproto::requests::{
    self as proto, HeadSpec, NewRequest, NewRequestComment, NewReview, Request, RequestComment,
    RequestComments, RequestLand, RequestLanded, RequestPatch, RequestUpdate, Requests, Review,
    Reviews,
};
use braid_hubproto::scope::{ParsedScope, Right};
use braid_repo::{Location, Repo, WorkspaceLink};
use braid_store::{RefUpdate, Store};
use braid_wc::Workspace;
use clap::{Args, Subcommand, ValueEnum};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::creds::{self, HubEnv};
use super::fetch::{fetch, haves, whole};
use super::http::{HttpHub, HttpRepo};
use super::land::{OffBranch, hub_stack, land, once_on_hub, send_stack};
use super::link::Link;
use super::remote::{HubError, Remote, shown};
use super::social_common::{
    CONFLICTS, REFUSED, agent, body_text, exit_for, idempotency_key, print,
};
use super::transfer::store_stream;
use crate::work::count;
use crate::{Cli, open_repo, print_json};

/// The longest slug a title gives a branch, in characters.
const MAX_SLUG: usize = 40;

/// How many names a new branch tries, `<slug>`, `<slug>-2` and so on,
/// before it gives up.
const MAX_NAMES: usize = 100;

/// The most comments, and the most reviews, `show` reads: a page of each.
const SHOWN: usize = 100;

/// The most pages of a diff `--json` reads.
const MAX_PAGES: u64 = 10_000;

/// The loop that settles a request's conflicts, for the long helps.
macro_rules! conflict_loop {
    () => {
        "When a request conflicts with its base (`braid hub request show N` says
so, and landing it exits with status 2), its author settles the conflicts
in the workspace it came from, and gives the request a new revision:

  braid hub sync-fork          bring what the base landed into the fork
                               (in a clone of the repository itself, skip
                               this: `braid update` fetches the base)
  braid update                 rebase the workspace onto it: exit status 2,
                               the conflicts are in the working copy
  (edit the files)             take the conflict markers out
  braid resolve PATH...        mark each conflict that is not about content
  braid commit                 finish the rebase
  braid hub request update N   the request's next revision: what the
                               workspace holds now"
    };
}

/// The long help of `hub request`.
pub(crate) const REQUEST_HELP: &str = concat!(
    "Open, review, update and land change requests: the work of a branch
proposed for a branch of a repository on the hub, reviewed, and landed
exactly as it was shown.

The repository is -R OWNER/NAME, else the one this repository is linked to
or, when that one is a fork, the repository it was forked from, where its
change requests go.

To contribute to a repository you may read but not write to, fork it and
clone the fork (`braid hub fork OWNER/NAME --clone`), commit in the fork's
workspace, then run `braid hub request create --title TITLE` there: braid
puts the workspace's changesets on a branch request/<slug> of the fork, and
opens the request on the repository it was forked from.

",
    conflict_loop!(),
    "

Exit statuses: 0 done, or landed; 1 refused (the snapshot moved, the base
no longer holds where the stack starts, not allowed, not found); 2
conflicts, printed as `braid land --json` prints them; 3 the hub could not
be reached, or failed.

create, comment and review send an Idempotency-Key, and send the request
once more with the same key when the answer was lost: the hub then gives
the first answer again rather than doing it twice. With --json each
command prints what the hub answered."
);

/// The long help of `hub request create`.
pub(crate) const CREATE_HELP: &str =
    "Open a change request on the repository: -R OWNER/NAME, else the one this
repository is linked to or, when that one is a fork, the repository it was
forked from.

Without --head, in a workspace: the workspace's changesets that its branch
on the hub lacks, as `braid land` finds them (with any changesets of the
local branch below them that the hub's branch lacks), are uploaded, put on
a new branch on the hub, and proposed for the base:

  request/<login>/<slug>  on the repository itself, when you may write to
                          it and your token may land there (a land:<glob>
                          right that matches the name; request/<slug> when
                          only that one matches)
  request/<slug>          otherwise, on the fork this repository is linked
                          to; in a clone of the repository itself, fork it
                          first (`braid hub fork OWNER/NAME --clone`)

<slug> comes from the title: lowercase ASCII letters, digits and '-', at
most 40 characters, with -2, -3 and so on when the name holds other work.
The workspace and its branch do not move. Like `braid land`, it refuses a
workspace with uncommitted changes or a rebase in progress.

With --head [OWNER/NAME:]BRANCH, the request proposes that branch as it is
on the hub: a branch of OWNER/NAME, else of the repository this one is
linked to (of the target, outside a linked repository).

A head in a private repository other than the target publishes what it
brings to everyone who reads the target: an admin of the head says so
with --expose, and only when that is meant.

It prints the request's number and its address; with --json, the request
as the hub answered it.";

/// The long help of `hub request update`.
pub(crate) const UPDATE_HELP: &str = concat!(
    "Give a change request a new revision, its author only. In a workspace,
the workspace's changesets that its branch on the hub lacks are uploaded
and put on the request's head branch: as a fast-forward when they build on
it; otherwise in its place, since the new revision replaces what the
request proposed. The hub then makes the revision. Reviews of earlier
revisions no longer count.

With --head, no workspace is needed: the new revision is what the head
branch holds on the hub now, or the changeset ID, which that branch must
hold on its first-parent chain.

",
    conflict_loop!(),
    "

With --json it prints the request as the hub answered it."
);

/// The long help of `hub request land`.
pub(crate) const LAND_HELP: &str = concat!(
    "Land a change request on its base: the snapshot --expect names, or else
the one it proposes now, which is read first and printed. What lands is
what was reviewed: when the request moved to another snapshot since, the
hub lands nothing, and the exit status is 1 (moved).

--mode rebase (the default) fast-forwards the base, or rebases the stack
onto it; ff-only refuses unless it is a fast-forward (exit status 1).
Landing needs write and a land:<glob> right that matches the base, and the
approvals the repository asks for.

Exit statuses: 0 landed; 1 refused (moved, not_on_branch when the base no
longer holds where the stack starts, not allowed, not found); 2 conflicts,
printed as `braid land --json` prints them: the request stays open, and its
author settles them; 3 the hub could not be reached, or failed.

",
    conflict_loop!(),
    "

With --json it prints {request, land}: the request as it is now, and the
landing as `POST …/land` answers it."
);

/// The long help of `hub request checkout`.
pub(crate) const CHECKOUT_HELP: &str =
    "Bring a change request's snapshot into this repository, as the branch
request/<N> (or --branch NAME). Its changesets, and what they need, come
through the request itself, so that whoever reads the repository the
request is on fetches it, even without reading the repository it comes
from. The branch is created, or moved forward; one with changesets the
snapshot lacks stays where it is, unless --force.

`braid workspace add NAME --track BRANCH` then makes a workspace on it;
`braid log`, `braid show` and `braid cat-object` read it as it is.

With --json it prints request, repo, branch, snapshot, before (where the
branch was, or null), outcome (created, moved or same) and objects (how
many came).";

/// `hub request`.
#[derive(Debug, Args)]
pub(crate) struct RequestArgs {
    #[command(subcommand)]
    pub(crate) action: RequestAction,
}

/// What `hub request` does.
#[derive(Debug, Subcommand)]
pub(crate) enum RequestAction {
    /// Open a change request: from the workspace's changesets, or from a
    /// branch on the hub.
    #[command(long_about = CREATE_HELP)]
    Create(CreateArgs),
    /// List a repository's change requests, newest first.
    List(ListArgs),
    /// Show a change request: what it proposes, whether it lands, its
    /// reviews and comments.
    Show(ShowArgs),
    /// Print the diff of a change request.
    Diff(DiffArgs),
    /// Comment on a change request, or on a line of its diff.
    Comment(CommentArgs),
    /// Review a change request: approve it, ask for changes, or comment.
    Review(ReviewArgs),
    /// Give a change request a new revision: the workspace as it is now.
    #[command(long_about = UPDATE_HELP)]
    Update(UpdateArgs),
    /// Land the snapshot of a change request that was read (exit status 2
    /// on conflicts).
    #[command(long_about = LAND_HELP)]
    Land(LandArgs),
    /// Close a change request without landing it.
    Close(NumberArgs),
    /// Open a closed change request again.
    Reopen(NumberArgs),
    /// Mark a draft change request ready for review.
    Ready(NumberArgs),
    /// Bring a change request's snapshot into this repository, as a branch.
    #[command(long_about = CHECKOUT_HELP)]
    Checkout(CheckoutArgs),
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

/// `-R/--hub-repo`: the repository whose change requests a command acts
/// on.
#[derive(Clone, Debug, Default, Args)]
pub(crate) struct TargetArg {
    /// The repository on the hub, OWNER/NAME (default: the one this
    /// repository is linked to or, when that is a fork, the repository it
    /// was forked from).
    #[arg(short = 'R', long = "hub-repo", value_name = "OWNER/NAME")]
    pub(crate) hub_repo: Option<String>,
}

/// `hub request create`.
#[derive(Debug, Args)]
pub(crate) struct CreateArgs {
    #[command(flatten)]
    pub(crate) repo: TargetArg,
    /// Its title, one line.
    #[arg(long, value_name = "TITLE")]
    pub(crate) title: String,
    #[command(flatten)]
    pub(crate) body: BodyArgs,
    /// Propose this branch, as it is on the hub, instead of the
    /// workspace's changesets: a branch of OWNER/NAME, else of the
    /// repository this one is linked to.
    #[arg(long, value_name = "[OWNER/NAME:]BRANCH")]
    pub(crate) head: Option<String>,
    /// The branch it proposes the work for (default: the target's default
    /// branch).
    #[arg(long, value_name = "BRANCH")]
    pub(crate) base: Option<String>,
    /// Publish what a private head brings to everyone who reads the
    /// target: needed, from an admin of the head, when the head is in a
    /// private repository other than the target.
    #[arg(long)]
    pub(crate) expose: bool,
    /// Open it as a draft, which does not land until it is marked ready.
    #[arg(long)]
    pub(crate) draft: bool,
    /// Ask this account to review it; given again, more (the target's
    /// writers only).
    #[arg(long = "reviewer", value_name = "LOGIN")]
    pub(crate) reviewers: Vec<String>,
}

/// Which change requests `request list` shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum StateArg {
    /// Those open.
    Open,
    /// Those closed or landed.
    Closed,
    /// Those landed.
    Landed,
    /// Every one.
    All,
}

impl StateArg {
    fn as_str(self) -> &'static str {
        match self {
            StateArg::Open => "open",
            StateArg::Closed => "closed",
            StateArg::Landed => "landed",
            StateArg::All => "all",
        }
    }
}

/// `hub request list`.
#[derive(Debug, Args)]
pub(crate) struct ListArgs {
    #[command(flatten)]
    pub(crate) repo: TargetArg,
    /// Which requests, by state.
    #[arg(long, value_enum, default_value_t = StateArg::Open)]
    pub(crate) state: StateArg,
    /// Only those this account opened.
    #[arg(long, value_name = "LOGIN")]
    pub(crate) author: Option<String>,
    /// Only those proposed for this branch.
    #[arg(long, value_name = "BRANCH")]
    pub(crate) base: Option<String>,
    /// Only those this account is asked to review.
    #[arg(long, value_name = "LOGIN")]
    pub(crate) reviewer: Option<String>,
    /// The most requests to list, 1 to 100 (default: 30).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=100))]
    pub(crate) limit: Option<u32>,
    /// Where to start: the `next` of the page before.
    #[arg(long, value_name = "CURSOR")]
    pub(crate) after: Option<String>,
}

/// `hub request show`.
#[derive(Debug, Args)]
pub(crate) struct ShowArgs {
    /// The request's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: TargetArg,
    /// Show its diff too.
    #[arg(long)]
    pub(crate) diff: bool,
}

/// `hub request diff`.
#[derive(Debug, Args)]
pub(crate) struct DiffArgs {
    /// The request's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: TargetArg,
    /// The revision whose diff to show, from its merge base to its
    /// snapshot, or A..B for what changed from revision A to revision B
    /// (default: the current revision).
    #[arg(long, value_name = "REV")]
    pub(crate) rev: Option<String>,
    /// Only the diff of this file, from the repository's root.
    #[arg(long, value_name = "PATH")]
    pub(crate) path: Option<String>,
}

/// The side of a diff a line comment is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum SideArg {
    /// The version before.
    Old,
    /// The version after.
    New,
}

impl SideArg {
    fn as_str(self) -> &'static str {
        match self {
            SideArg::Old => proto::OLD,
            SideArg::New => proto::NEW,
        }
    }
}

/// `hub request comment`.
#[derive(Debug, Args)]
pub(crate) struct CommentArgs {
    /// The request's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: TargetArg,
    #[command(flatten)]
    pub(crate) body: BodyArgs,
    /// For a comment on a line of the diff: the file, as the diff names it.
    #[arg(long, value_name = "PATH", requires = "line")]
    pub(crate) path: Option<String>,
    /// For a comment on a line of the diff: the line, counted from 1.
    #[arg(long, value_name = "N", requires = "path")]
    pub(crate) line: Option<u64>,
    /// The side of the diff the line is on (default: new).
    #[arg(long, value_enum, requires = "line")]
    pub(crate) side: Option<SideArg>,
    /// The revision whose diff the line is in (default: the current one).
    #[arg(long, value_name = "N", requires = "line")]
    pub(crate) revision: Option<u64>,
}

/// What a review says.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub(crate) struct VerdictArgs {
    /// Approve the request's current revision.
    #[arg(long)]
    pub(crate) approve: bool,
    /// Ask for changes before it lands (say which, with a text).
    #[arg(long)]
    pub(crate) request_changes: bool,
    /// Only comment (with a text).
    #[arg(long)]
    pub(crate) comment: bool,
}

impl VerdictArgs {
    fn as_str(&self) -> &'static str {
        if self.approve {
            proto::APPROVE
        } else if self.request_changes {
            proto::REQUEST_CHANGES
        } else {
            proto::COMMENT
        }
    }
}

/// `hub request review`.
#[derive(Debug, Args)]
pub(crate) struct ReviewArgs {
    /// The request's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: TargetArg,
    #[command(flatten)]
    pub(crate) verdict: VerdictArgs,
    #[command(flatten)]
    pub(crate) body: BodyArgs,
    /// The revision you reviewed: the hub refuses the review (moved) when
    /// the request has moved past it since.
    #[arg(long, value_name = "N")]
    pub(crate) revision: Option<u64>,
}

/// `hub request update`.
#[derive(Debug, Args)]
pub(crate) struct UpdateArgs {
    /// The request's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: TargetArg,
    /// Without a workspace: what the head branch holds on the hub now, or
    /// the changeset ID on its first-parent chain.
    #[arg(long, value_name = "ID", num_args = 0..=1)]
    pub(crate) head: Option<Option<String>>,
}

/// How `request land` lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ModeArg {
    /// Fast-forward, or rebase the stack onto the base.
    Rebase,
    /// Fast-forward only; refuse otherwise.
    FfOnly,
}

impl From<ModeArg> for LandMode {
    fn from(mode: ModeArg) -> LandMode {
        match mode {
            ModeArg::Rebase => LandMode::Rebase,
            ModeArg::FfOnly => LandMode::FfOnly,
        }
    }
}

/// `hub request land`.
#[derive(Debug, Args)]
pub(crate) struct LandArgs {
    /// The request's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: TargetArg,
    /// The snapshot to land, the one you reviewed, in full: nothing lands
    /// when the request proposes another one now (default: the one it
    /// proposes when it is read).
    #[arg(long, value_name = "SNAPSHOT")]
    pub(crate) expect: Option<String>,
    /// How it lands.
    #[arg(long, value_enum, default_value_t = ModeArg::Rebase)]
    pub(crate) mode: ModeArg,
}

/// `hub request close`, `reopen` and `ready`.
#[derive(Debug, Args)]
pub(crate) struct NumberArgs {
    /// The request's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: TargetArg,
}

/// `hub request checkout`.
#[derive(Debug, Args)]
pub(crate) struct CheckoutArgs {
    /// The request's number.
    #[arg(value_name = "NUMBER")]
    pub(crate) number: u64,
    #[command(flatten)]
    pub(crate) repo: TargetArg,
    /// The branch to put its snapshot on (default: `request/<NUMBER>`).
    #[arg(long, value_name = "NAME")]
    pub(crate) branch: Option<String>,
    /// Move the branch even when it holds changesets the snapshot lacks
    /// (`braid undo` takes that back).
    #[arg(long)]
    pub(crate) force: bool,
}

pub(crate) fn request(cli: &Cli, args: &RequestArgs) -> Result<ExitCode> {
    let done = match &args.action {
        RequestAction::Create(args) => create(cli, args),
        RequestAction::List(args) => list(cli, args),
        RequestAction::Show(args) => show(cli, args),
        RequestAction::Diff(args) => diff(cli, args),
        RequestAction::Comment(args) => comment(cli, args),
        RequestAction::Review(args) => review(cli, args),
        RequestAction::Update(args) => update(cli, args),
        RequestAction::Land(args) => land_request(cli, args),
        RequestAction::Close(args) => change(
            cli,
            args,
            &RequestPatch {
                state: Some(proto::CLOSED.to_owned()),
                ..RequestPatch::default()
            },
            "Closed",
        ),
        RequestAction::Reopen(args) => change(
            cli,
            args,
            &RequestPatch {
                state: Some(proto::OPEN.to_owned()),
                ..RequestPatch::default()
            },
            "Reopened",
        ),
        RequestAction::Ready(args) => change(
            cli,
            args,
            &RequestPatch {
                draft: Some(false),
                ..RequestPatch::default()
            },
            "Marked ready for review:",
        ),
        RequestAction::Checkout(args) => checkout(cli, args),
    };
    match done {
        Ok(code) => Ok(code),
        // The hub refused, or could not be reached: said here, with the
        // exit status that tells the two apart.
        Err(err) => match hub_error(&err) {
            Some(hub) => {
                eprintln!("braid: error: {err:#}");
                Ok(ExitCode::from(exit_for(hub)))
            }
            None => Err(err),
        },
    }
}

/// The hub's error behind `err`, if a request to the hub is why it failed.
fn hub_error(err: &anyhow::Error) -> Option<&HubError> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<HubError>())
}

/// A text of several lines from the hub, as braid prints it: each line as
/// [`shown`] shows text, its line breaks and tabs kept.
fn shown_lines(text: &str) -> String {
    text.split('\n')
        .map(|line| line.split('\t').map(shown).collect::<Vec<_>>().join("\t"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text` as a value of a query string: everything but letters, digits
/// and `-._~` percent-encoded.
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

/// A ref's name as the end of a route: each part between slashes
/// [`encoded`], the slashes kept.
fn encoded_ref(name: &str) -> String {
    name.split('/').map(encoded).collect::<Vec<_>>().join("/")
}

/// Whether `a` and `b` name the same repository: owners and names compare
/// without regard to case, as the hub compares them.
fn same_repo(a: &RepoPath, b: &RepoPath) -> bool {
    a.owner.eq_ignore_ascii_case(&b.owner) && a.name.eq_ignore_ascii_case(&b.name)
}

/// The route of request `number` of `target`.
fn request_route(target: &RepoPath, number: u64) -> String {
    format!("{}/requests/{number}", target.api_path())
}

/// A changeset id given on the command line: 64 hex digits, as the hub
/// names snapshots.
fn full_id(text: &str, what: &str) -> Result<ObjectId> {
    text.parse()
        .map_err(|_| anyhow::anyhow!("{text:?} is not {what}: give its full id, 64 hex digits"))
}

/// The name a title gives a branch: its ASCII letters, lowercased, and its
/// digits, every other run of characters one `-`, without `-` at either
/// end, at most [`MAX_SLUG`] characters; `change` when nothing is left.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    // Only ASCII is left, so this cuts between characters.
    out.truncate(MAX_SLUG);
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "change".to_owned()
    } else {
        out.to_owned()
    }
}

/// `slug`, or for `n` above 1 `slug-n`, cut so that it keeps within
/// [`MAX_SLUG`] characters.
fn numbered(slug: &str, n: usize) -> String {
    if n <= 1 {
        return slug.to_owned();
    }
    let suffix = format!("-{n}");
    let keep = MAX_SLUG.saturating_sub(suffix.len()).min(slug.len());
    format!("{}{suffix}", slug[..keep].trim_end_matches('-'))
}

/// Sends a request that creates something with the `Idempotency-Key`
/// `key`, and sends it once more, with the same key, when its answer was
/// lost: the hub then gives the first answer again rather than doing it
/// twice.
fn create_with<T: DeserializeOwned>(
    hub: &HttpHub,
    route: &str,
    body: &Value,
    key: &str,
) -> Result<T, HubError> {
    let headers = [("Idempotency-Key", key)];
    match hub.call_json("POST", route, &headers, Some(body)) {
        Err(err) if err.may_have_happened() => hub.call_json("POST", route, &headers, Some(body)),
        answer => answer,
    }
}

/// [`create_with`], with a new key. When the answer was lost twice, the
/// error says that the hub may have made `what` all the same, and where to
/// look.
fn create_once<T: DeserializeOwned>(
    hub: &HttpHub,
    route: &str,
    body: &Value,
    what: &str,
    look: &str,
) -> Result<T> {
    create_with(hub, route, body, &idempotency_key()).map_err(|err| {
        if err.may_have_happened() {
            anyhow::Error::new(err).context(format!(
                "the hub may have made {what} before its answer was lost: {look} shows whether it did"
            ))
        } else {
            err.into()
        }
    })
}

/// Where a command acts.
struct Place {
    /// The hub.
    hub: HttpHub,
    /// Its URL, for the addresses of its pages.
    url: String,
    /// The repository whose change requests the command acts on.
    target: RepoPath,
    /// The link of the repository the command runs in, when it is linked.
    link: Option<Link>,
}

/// The repository `info` was forked from, when the hub names it: only to
/// those who may read it.
fn upstream_of(info: &RepoInfo) -> Option<RepoPath> {
    info.fork_of
        .as_ref()
        .and_then(|up| RepoPath::new(&up.owner, &up.name).ok())
}

/// The link of the repository the command runs in, when it is in one that
/// is linked to a hub.
fn linked(cli: &Cli) -> Result<Option<Link>> {
    match open_repo(cli) {
        Ok(repo) => Link::read(&repo),
        // Outside any repository, -R says where.
        Err(_) if cli.repo.is_none() => Ok(None),
        Err(err) => Err(err),
    }
}

/// Where a command acts: `given` (OWNER/NAME) on the hub signed in to;
/// else, in a repository linked to a hub, the repository it is linked to
/// or, when that is a fork, the repository it was forked from.
fn place_for(link: Option<Link>, given: Option<&str>) -> Result<Place> {
    if let Some(text) = given {
        let target: RepoPath = text
            .parse()
            .with_context(|| format!("{text:?} is not OWNER/NAME"))?;
        let env = HubEnv::from_process();
        let creds = env.credentials()?;
        let url = creds::hub_url(&env, &creds, None)?;
        let (hub, _) = super::client(&env, &url)?;
        return Ok(Place {
            hub,
            url,
            target,
            link,
        });
    }
    let link = link.context(
        "no repository named: give -R OWNER/NAME, or run this in a repository linked to a hub",
    )?;
    let hub = super::connect(&link)?;
    let info = hub.repo(link.repo.clone()).info()?;
    let target = upstream_of(&info).unwrap_or_else(|| link.repo.clone());
    Ok(Place {
        hub,
        url: link.url.clone(),
        target,
        link: Some(link),
    })
}

/// [`place_for`] the repository the command runs in.
fn place(cli: &Cli, given: Option<&str>) -> Result<Place> {
    place_for(linked(cli)?, given)
}

/// The workspace a command acts on, and its repository.
struct Found {
    repo: Repo,
    name: String,
    /// Where its `.braid` file was found, when it was found by looking.
    dir: Option<PathBuf>,
}

impl Found {
    /// The workspace `--workspace` names, or the one the current directory
    /// is in, as `braid land` finds it.
    fn find(cli: &Cli) -> Result<Found> {
        if let Some(name) = &cli.workspace {
            return Ok(Found {
                repo: open_repo(cli)?,
                name: name.clone(),
                dir: None,
            });
        }
        let cwd = std::env::current_dir()?;
        match braid_repo::locate(&cwd)? {
            Location::Workspace { dir, link } => {
                let repo = match &cli.repo {
                    Some(root) => Repo::open_with(root, cli.store_options())?,
                    None => Repo::open_with(&link.repository, cli.store_options())?,
                };
                Ok(Found {
                    repo,
                    name: link.name,
                    dir: Some(dir),
                })
            }
            Location::Root(_) => bail!(
                "not in a workspace: run this in the workspace whose changesets to propose, or name a branch on the hub with --head"
            ),
        }
    }

    fn open(&self) -> Result<Workspace<'_>> {
        Ok(match &self.dir {
            Some(dir) => Workspace::at(&self.repo, &self.name, dir)?,
            None => Workspace::open(&self.repo, &self.name)?,
        })
    }

    /// The repository's link, which these commands need.
    fn link(&self) -> Result<Link> {
        Link::read(&self.repo)?.context(
            "this repository is not linked to a hub: `braid hub clone` makes a linked one (of a fork of yours, `braid hub fork OWNER/NAME --clone`)",
        )
    }
}

/// What a workspace proposes: the changesets `braid land` would land, as
/// its branch stands on the hub.
struct Work {
    /// The changesets, oldest first: never none.
    changes: Vec<ObjectId>,
    /// The hub's head of the branch.
    head: Option<ObjectId>,
}

impl Work {
    /// The workspace's changesets that its branch on the hub lacks, after
    /// any changesets of the local branch below them that the hub's branch
    /// lacks, as `braid land` finds them, once fetched from `remote`. The
    /// caller has checked the workspace first, with [`Workspace::stack`],
    /// so that uncommitted changes and a rebase in progress are refused, as
    /// a landing refuses them, before the hub is asked anything.
    fn of(
        repo: &Repo,
        workspace: &Workspace<'_>,
        remote: &impl Remote,
        link: &Link,
        agent: &str,
    ) -> Result<Work> {
        let fetched = fetch(repo, remote, link, agent, None).context(
            "cannot fetch from the hub, so nothing was proposed: run the command again once it answers",
        )?;
        let stack = workspace.stack(None)?;
        let head = fetched.hub_head(&stack.branch);
        let changes = hub_stack(repo, &stack, head, &fetched)?;
        if changes.is_empty() {
            bail!(
                "nothing to propose: the workspace holds no changeset that {} lacks on the hub; commit the work first",
                stack.branch
            );
        }
        let most = remote.max_ids();
        if changes.len() > most {
            bail!(
                "the workspace holds a stack of {} changesets, more than the {most} the hub takes in one go, so nothing was sent",
                changes.len()
            );
        }
        // As in a landing: changesets below the workspace's own that the
        // hub's branch had once, and no longer has, were taken off it on
        // purpose, and a request would bring them back.
        if changes.len() > stack.changes.len()
            && let Some(seen) = once_on_hub(repo, &stack.branch, changes[0])?
        {
            bail!(
                "the hub's {} no longer has {} of the changesets below this workspace's own: it was at {} when this repository saw it, so someone took them off, and the request would bring them back; nothing was sent",
                stack.branch,
                count(changes.len() - stack.changes.len(), "changeset"),
                seen.short()
            );
        }
        Ok(Work { changes, head })
    }

    /// The changeset on top.
    fn last(&self) -> ObjectId {
        self.changes[self.changes.len() - 1]
    }

    /// Uploads to `remote` what it lacks of the changesets: what they
    /// reach beyond their base and the hub's head of the workspace's
    /// branch, those of the two `remote` holds.
    fn upload(&self, store: &Store, remote: &HttpRepo<'_>, link: &Link) -> Result<()> {
        let base = read_changeset(store, self.changes[0])?
            .parents
            .first()
            .copied();
        let known: Vec<ObjectId> = base.into_iter().chain(self.head).collect();
        let lacked = if known.is_empty() {
            Vec::new()
        } else {
            remote.lacks(&known)?
        };
        let have: Vec<ObjectId> = known
            .into_iter()
            .filter(|id| !lacked.contains(id))
            .collect();
        send_stack(store, remote, link, self.last(), &have, self.changes.len()).context(
            "cannot upload the changesets to the hub, so nothing was proposed: run the command again, which sends only what the hub still lacks",
        )
    }
}

/// The landing of `changes`, fast-forward only, on `branch`.
fn ff_only(branch: &str, changes: &[ObjectId], agent: &str) -> LandRequest {
    LandRequest {
        reference: branch.to_owned(),
        changes: changes.to_vec(),
        mode: LandMode::FfOnly,
        agent: Some(agent.to_owned()),
    }
}

/// Puts the work on a new branch of `remote`, `prefix` and the slug, with
/// `-2`, `-3` and so on when the name holds other work; a branch an earlier
/// try made, at the work's top, is taken up. A landing that only
/// fast-forwards never puts the work on top of someone else's. Returns the
/// branch's name.
fn new_branch(
    store: &Store,
    remote: &HttpRepo<'_>,
    prefix: &str,
    slug: &str,
    work: &Work,
    agent: &str,
) -> Result<String> {
    let last = work.last();
    let refs = remote.refs()?.refs;
    for n in 1..=MAX_NAMES {
        let name = format!("{prefix}{}", numbered(slug, n));
        match refs.get(&name) {
            Some(head) if *head == last => return Ok(name),
            Some(_) => continue,
            None => {}
        }
        let landed = land(
            store,
            remote,
            &ff_only(&name, &work.changes, agent),
            OffBranch::Stack,
        )
        .context("cannot put the changesets on a branch of the hub, so nothing was proposed")?;
        if matches!(landed.outcome.as_str(), "fast-forward" | "already-applied")
            && landed.head == Some(last)
        {
            return Ok(name);
        }
        // Made by someone else meanwhile: the next name.
    }
    bail!(
        "the names from {prefix}{slug} to {prefix}{} all hold other work on the hub: give another --title",
        numbered(slug, MAX_NAMES)
    )
}

/// Whether the caller may land on `name` in `target`: it writes there, as
/// `info` says, and its token has a `land:<glob>` right for `target` whose
/// glob matches `name`, as the hub decides.
fn may_land(me: &Me, target: &RepoPath, info: &RepoInfo, name: &str) -> bool {
    if !matches!(info.permission.as_str(), "write" | "admin") {
        return false;
    }
    let Some(token) = &me.token else {
        return false;
    };
    token
        .scopes
        .iter()
        .filter_map(|scope| ParsedScope::parse(scope).ok())
        .filter(|scope| scope.repo.matches(target))
        .flat_map(|scope| scope.rights)
        .any(|right| matches!(right, Right::Land(glob) if glob_matches(&glob, name)))
}

/// The prefix of the branch a new request of `login` goes on in `target`,
/// when the caller may put it there: `request/<login>/`, or `request/`
/// when only that one is open to its land rights.
fn prefix_on(me: &Me, target: &RepoPath, info: &RepoInfo, slug: &str) -> Option<String> {
    [format!("request/{}/", me.login), "request/".to_owned()]
        .into_iter()
        .find(|prefix| may_land(me, target, info, &format!("{prefix}{slug}")))
}

/// What `request create` prints in words.
fn created_text(target: &RepoPath, request: &Request, address: &str) -> String {
    format!(
        "Opened {target}#{}: {}\n  {} -> {}, {}\n  {}",
        request.number,
        shown(&request.title),
        head_name(target, request),
        shown(&request.base.reference),
        count(request.stack.len(), "changeset"),
        shown(address)
    )
}

fn create(cli: &Cli, args: &CreateArgs) -> Result<ExitCode> {
    let body = args.body.text()?;
    let new = |head: HeadSpec| NewRequest {
        title: args.title.clone(),
        body: body.clone(),
        head,
        base: args.base.clone(),
        expose: args.expose,
        draft: args.draft,
        reviewers: args.reviewers.clone(),
        agent: agent(cli),
    };
    if let Some(head) = &args.head {
        let place = place(cli, args.repo.hub_repo.as_deref())?;
        let (named, branch) = match head.split_once(':') {
            Some((repo, branch)) => (
                Some(
                    repo.parse::<RepoPath>()
                        .with_context(|| format!("{repo:?} is not OWNER/NAME"))?,
                ),
                branch,
            ),
            None => (None, head.as_str()),
        };
        if branch.is_empty() {
            bail!("--head names a branch: [OWNER/NAME:]BRANCH");
        }
        // A branch of this repository's, unless another is named.
        let repo = named
            .or_else(|| {
                place
                    .link
                    .as_ref()
                    .filter(|link| link.url == place.url)
                    .map(|link| link.repo.clone())
            })
            .filter(|repo| !same_repo(repo, &place.target));
        let asked = new(HeadSpec {
            repo: repo.map(|repo| repo.to_string()),
            reference: branch.to_owned(),
        });
        return open_request(cli, &place.hub, &place.url, &place.target, &asked);
    }

    let found = Found::find(cli)?;
    let workspace = found.open()?;
    workspace.stack(None)?;
    let link = found.link()?;
    let hub = super::connect(&link)?;
    let linked = hub.repo(link.repo.clone());
    let linked_info = linked.info()?;
    let target = match &args.repo.hub_repo {
        Some(text) => text
            .parse::<RepoPath>()
            .with_context(|| format!("{text:?} is not OWNER/NAME"))?,
        None => upstream_of(&linked_info).unwrap_or_else(|| link.repo.clone()),
    };
    let same = same_repo(&target, &link.repo);
    let target_info = if same {
        linked_info.clone()
    } else {
        hub.repo(target.clone()).info()?
    };
    let me = hub.me()?;
    let slug = slug(&args.title);
    // On the target itself when the caller may land there; on the fork
    // this repository is linked to otherwise.
    let (head_path, head_info, prefix) = match prefix_on(&me, &target, &target_info, &slug) {
        Some(prefix) => (target.clone(), &target_info, prefix),
        None if !same => (link.repo.clone(), &linked_info, "request/".to_owned()),
        None => bail!(
            "putting the work on a branch of {target} needs write there, and a land:<glob> right that matches request/{}/{slug}: fork it (`braid hub fork {target} --clone`), commit in the fork's workspace, and open the request from there",
            me.login
        ),
    };
    if head_info.private && !same_repo(&head_path, &target) && !args.expose {
        bail!(
            "{head_path} is private: a request from it publishes what it brings to everyone who reads {target}; an admin of {head_path} opens it with --expose, when that is meant"
        );
    }
    let work = Work::of(&found.repo, &workspace, &linked, &link, &cli.agent)?;
    let store = found.repo.store();
    let remote = hub.repo(head_path.clone());
    work.upload(store, &remote, &link)?;
    let branch = new_branch(store, &remote, &prefix, &slug, &work, &cli.agent)?;
    let asked = new(HeadSpec {
        repo: (!same_repo(&head_path, &target)).then(|| head_path.to_string()),
        reference: branch,
    });
    open_request(cli, &hub, &link.url, &target, &asked)
}

/// Opens the request `asked` on `target`, and prints it.
fn open_request(
    cli: &Cli,
    hub: &HttpHub,
    url: &str,
    target: &RepoPath,
    asked: &NewRequest,
) -> Result<ExitCode> {
    let route = format!("{}/requests", target.api_path());
    let mut made: Request = create_once(
        hub,
        &route,
        &serde_json::to_value(asked)?,
        "the request",
        &format!("`braid hub request list -R {target}`"),
    )?;
    made.html = None;
    let address = format!("{url}/{target}/requests/{}", made.number);
    print(cli, &serde_json::to_value(&made)?, || {
        created_text(target, &made, &address)
    })?;
    Ok(ExitCode::SUCCESS)
}

/// A request in a line: its number, its title, and what it is unless open.
fn line(request: &Request) -> String {
    let mut out = format!("#{} {}", request.number, shown(&request.title));
    if request.draft {
        out.push_str(" [draft]");
    }
    match request.state.as_str() {
        proto::LANDED => out.push_str(" (landed)"),
        proto::CLOSED => out.push_str(" (closed)"),
        _ => {}
    }
    out
}

/// Where a request's head is, for people: `owner/name:branch`, or the
/// branch alone in `target` itself, or in a repository the caller may not
/// read, which the hub does not name.
fn head_name(target: &RepoPath, request: &Request) -> String {
    let branch = shown(&request.head.reference);
    match &request.head.repo {
        Some(repo)
            if !(repo.owner.eq_ignore_ascii_case(&target.owner)
                && repo.name.eq_ignore_ascii_case(&target.name)) =>
        {
            format!("{}/{}:{branch}", shown(&repo.owner), shown(&repo.name))
        }
        _ => branch,
    }
}

fn list(cli: &Cli, args: &ListArgs) -> Result<ExitCode> {
    let place = place(cli, args.repo.hub_repo.as_deref())?;
    let target = &place.target;
    let mut query = vec![format!("state={}", args.state.as_str())];
    for (name, value) in [
        ("author", &args.author),
        ("base", &args.base),
        ("reviewer", &args.reviewer),
        ("after", &args.after),
    ] {
        if let Some(value) = value {
            query.push(format!("{name}={}", encoded(value)));
        }
    }
    if let Some(limit) = args.limit {
        query.push(format!("limit={limit}"));
    }
    let route = format!("{}/requests?{}", target.api_path(), query.join("&"));
    let page: Requests = place.hub.call_json("GET", &route, &[], None)?;
    print(cli, &serde_json::to_value(&page)?, || {
        let mut out = String::new();
        if page.requests.is_empty() {
            out.push_str("No change requests.");
        }
        for request in &page.requests {
            if !out.is_empty() {
                out.push('\n');
            }
            let _ = write!(
                out,
                "{}, by {}: {} -> {}",
                line(request),
                shown(&request.author),
                head_name(target, request),
                shown(&request.base.reference)
            );
        }
        if let Some(next) = &page.next {
            let _ = write!(
                out,
                "\nMore: braid hub request list -R {target} --state {} --after {}",
                args.state.as_str(),
                shown(next)
            );
        }
        out
    })?;
    Ok(ExitCode::SUCCESS)
}

/// Conflicts as `braid land` prints them, a line each: the kind, the path,
/// and the regions of a content conflict, the directory removed, or the
/// path collided with.
fn conflicts_text(conflicts: &[Value]) -> String {
    let mut out = String::new();
    for conflict in conflicts {
        let text = |key: &str| shown(conflict[key].as_str().unwrap_or(""));
        let regions = conflict["hunks"].as_array().map_or(0, Vec::len);
        let detail = if regions > 0 {
            format!("  ({})", count(regions, "region"))
        } else if let Some(directory) = conflict["directory"].as_str() {
            format!("  (directory {} removed)", shown(directory))
        } else if let Some(existing) = conflict["existing"].as_str() {
            format!("  (collides with {})", shown(existing))
        } else {
            String::new()
        };
        let _ = writeln!(out, "  {:<13} {}{detail}", text("kind"), text("path"));
    }
    out
}

/// Whether a request lands now, in words.
fn mergeable_text(request: &Request) -> String {
    let base = shown(&request.base.reference);
    match request.mergeable.state.as_str() {
        proto::CLEAN => format!("Lands cleanly on {base} as it stands."),
        proto::CONFLICTS => format!(
            "Conflicts with {base}:\n{}",
            conflicts_text(&request.mergeable.conflicts).trim_end()
        ),
        proto::NEEDS_UPDATE => {
            format!("{base} moved to a history its stack is not built on: its author updates it.")
        }
        _ => "Whether it lands cleanly is being worked out: show it again in a moment.".to_owned(),
    }
}

/// A changeset of a stack in a line: its short id, summary and author.
fn changeset_line(entry: &ChangesetSummary) -> String {
    format!(
        "  {}  {}  ({})",
        entry.id.short(),
        shown(&entry.summary),
        shown(&entry.author.name)
    )
}

/// A comment's heading: who wrote it and when, and what line it is about.
fn comment_heading(comment: &RequestComment) -> String {
    let mut out = format!(
        "{} on {}",
        shown(&comment.comment.author),
        shown(&comment.comment.created)
    );
    if let (Some(path), Some(line)) = (&comment.path, comment.line) {
        let _ = write!(
            out,
            ", on {}:{line} ({})",
            shown(path),
            shown(comment.side.as_deref().unwrap_or(proto::NEW))
        );
        if let Some(revision) = comment.revision {
            let _ = write!(out, " of revision {revision}");
        }
        if comment.outdated {
            out.push_str(", outdated");
        }
    }
    out
}

/// A comment's text, or why it is hidden.
fn comment_body(comment: &RequestComment) -> String {
    match &comment.comment.hidden {
        Some(reason) if comment.comment.body.is_empty() => format!("(hidden: {})", shown(reason)),
        _ => shown_lines(comment.comment.body.trim_end()),
    }
}

/// A request, its reviews and its comments, for people.
fn described(
    target: &RepoPath,
    request: &Request,
    reviews: &[Review],
    comments: &[RequestComment],
) -> String {
    let mut out = format!("{target}{}\n", line(request));
    let via = request
        .agent
        .as_deref()
        .map_or_else(String::new, |agent| format!(", via {}", shown(agent)));
    let _ = writeln!(
        out,
        "Opened by {} on {}{via}",
        shown(&request.author),
        shown(&request.created)
    );
    let _ = writeln!(
        out,
        "Proposes {} at {} for {}, revision {}",
        head_name(target, request),
        request.head.snapshot.short(),
        shown(&request.base.reference),
        request.revision
    );
    match request.state.as_str() {
        proto::LANDED => {
            if let Some(landed) = &request.landed {
                let _ = writeln!(
                    out,
                    "Landed as {} by {} on {}",
                    landed.head.short(),
                    shown(&landed.by),
                    shown(&landed.at)
                );
            }
        }
        proto::CLOSED => {
            let _ = writeln!(
                out,
                "Closed by {} on {}",
                shown(request.closed_by.as_deref().unwrap_or("?")),
                shown(request.closed.as_deref().unwrap_or("?"))
            );
        }
        _ => {
            let _ = writeln!(out, "{}", mergeable_text(request));
            if let Some(attempt) = &request.last_attempt {
                let _ = writeln!(
                    out,
                    "The last landing ended in {}: {} on {}",
                    shown(&attempt.outcome),
                    shown(&attempt.by),
                    shown(&attempt.at)
                );
            }
        }
    }
    let approvals = &request.approvals;
    let _ = write!(out, "Approvals: {}", approvals.count);
    if !approvals.by.is_empty() {
        let _ = write!(out, " ({})", shown(&approvals.by.join(", ")));
    }
    let _ = writeln!(out, ", {} needed", approvals.required);
    if !request.review_requests.is_empty() {
        let _ = writeln!(
            out,
            "Review asked of {}",
            shown(&request.review_requests.join(", "))
        );
    }
    if !request.closes.is_empty() {
        let numbers: Vec<String> = request.closes.iter().map(|n| format!("#{n}")).collect();
        let _ = writeln!(out, "Closes {}", numbers.join(", "));
    }
    if !request.head.available {
        out.push_str("What it brings is no longer held on the hub.\n");
    }
    if !request.stack.is_empty() {
        let _ = writeln!(out, "Stack, oldest first:");
        for entry in &request.stack {
            let _ = writeln!(out, "{}", changeset_line(entry));
        }
    }
    if !request.brings.is_empty() {
        let _ = writeln!(out, "Also brings:");
        for entry in &request.brings {
            let _ = writeln!(out, "{}", changeset_line(entry));
        }
    }
    if !request.body.trim().is_empty() {
        let _ = write!(out, "\n{}\n", shown_lines(request.body.trim_end()));
    }
    for review in reviews {
        let verdict = match review.verdict.as_str() {
            proto::APPROVE => "approves",
            proto::REQUEST_CHANGES => "asks for changes",
            _ => "comments",
        };
        let mut notes = format!("revision {}", review.revision);
        if review.binding {
            notes.push_str(", binding");
        }
        if review.stale {
            notes.push_str(", stale");
        }
        let _ = write!(
            out,
            "\n--- {} {verdict} on {} ({notes})",
            shown(&review.author),
            shown(&review.created)
        );
        if !review.body.trim().is_empty() {
            let _ = write!(out, "\n{}", shown_lines(review.body.trim_end()));
        }
        for comment in &review.comments {
            let _ = write!(
                out,
                "\n  {}: {}",
                comment_heading(comment),
                comment_body(comment)
            );
        }
        out.push('\n');
    }
    for comment in comments {
        let _ = write!(
            out,
            "\n--- {}\n{}\n",
            comment_heading(comment),
            comment_body(comment)
        );
    }
    out
}

/// The query of a request's diff: its revision or range, and its file.
fn diff_query(rev: Option<&str>, path: Option<&str>) -> Vec<String> {
    let mut query = Vec::new();
    if let Some(rev) = rev {
        query.push(format!("rev={}", encoded(rev)));
    }
    if let Some(path) = path {
        query.push(format!("path={}", encoded(path.trim_matches('/'))));
    }
    query
}

/// The diff `route` (`…/requests/{n}/diff`) answers, as text, as the hub
/// writes it.
fn diff_text(hub: &HttpHub, route: &str, query: &[String]) -> Result<Vec<u8>, HubError> {
    let route = if query.is_empty() {
        route.to_owned()
    } else {
        format!("{route}?{}", query.join("&"))
    };
    hub.call("GET", &route, &[("Accept", "text/x-diff")], None)
}

/// Every page of the diff `route` (`…/requests/{n}/diff`) answers, as one.
fn diff_pages(hub: &HttpHub, route: &str, query: &[String]) -> Result<DiffPage, HubError> {
    let mut all = DiffPage::default();
    let mut number = 1;
    while number <= MAX_PAGES {
        let mut parts = vec![format!("page={number}")];
        parts.extend(query.iter().cloned());
        let page: DiffPage =
            hub.call_json("GET", &format!("{route}?{}", parts.join("&")), &[], None)?;
        all.files.extend(page.files);
        match page.next_page {
            Some(next) if next > number => number = next,
            _ => break,
        }
    }
    Ok(all)
}

fn show(cli: &Cli, args: &ShowArgs) -> Result<ExitCode> {
    let place = place(cli, args.repo.hub_repo.as_deref())?;
    let route = request_route(&place.target, args.number);
    let mut request: Request = place.hub.call_json("GET", &route, &[], None)?;
    request.html = None;
    let comments: RequestComments =
        place
            .hub
            .call_json("GET", &format!("{route}/comments?limit={SHOWN}"), &[], None)?;
    let reviews: Reviews =
        place
            .hub
            .call_json("GET", &format!("{route}/reviews?limit={SHOWN}"), &[], None)?;
    let diff_route = format!("{route}/diff");
    if cli.json {
        let mut value = json!({
            "request": request,
            "comments": comments.comments,
            "next": comments.next,
            "reviews": reviews.reviews,
        });
        if args.diff {
            value["diff"] = serde_json::to_value(diff_pages(&place.hub, &diff_route, &[])?)?;
        }
        print_json(&value)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut text = described(
        &place.target,
        &request,
        &reviews.reviews,
        &comments.comments,
    );
    if let Some(next) = &comments.next {
        let _ = writeln!(
            text,
            "\nMore comments: braid hub api GET '{route}/comments?after={}'",
            shown(next)
        );
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", text.trim_end())?;
    if args.diff {
        let diff = diff_text(&place.hub, &diff_route, &[])?;
        writeln!(out)?;
        out.write_all(&diff)?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn diff(cli: &Cli, args: &DiffArgs) -> Result<ExitCode> {
    let place = place(cli, args.repo.hub_repo.as_deref())?;
    let route = format!("{}/diff", request_route(&place.target, args.number));
    let query = diff_query(args.rev.as_deref(), args.path.as_deref());
    if cli.json {
        print_json(&serde_json::to_value(diff_pages(
            &place.hub, &route, &query,
        )?)?)?;
        return Ok(ExitCode::SUCCESS);
    }
    let text = diff_text(&place.hub, &route, &query)?;
    let mut out = std::io::stdout().lock();
    out.write_all(&text)?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn comment(cli: &Cli, args: &CommentArgs) -> Result<ExitCode> {
    let Some(body) = args.body.text()? else {
        bail!("a comment needs its text: give --body, or --body-file (- reads standard input)");
    };
    let place = place(cli, args.repo.hub_repo.as_deref())?;
    let asked = NewRequestComment {
        body,
        path: args.path.clone(),
        line: args.line,
        side: args.side.map(|side| side.as_str().to_owned()),
        revision: args.revision,
        agent: agent(cli),
    };
    let route = format!("{}/comments", request_route(&place.target, args.number));
    let look = format!(
        "`braid hub request show {} -R {}`",
        args.number, place.target
    );
    let mut said: RequestComment = create_once(
        &place.hub,
        &route,
        &serde_json::to_value(&asked)?,
        "the comment",
        &look,
    )?;
    said.comment.html = None;
    print(cli, &serde_json::to_value(&said)?, || {
        match (&said.path, said.line) {
            (Some(path), Some(line)) => format!(
                "Commented on line {line} of {} in {}#{}.",
                shown(path),
                place.target,
                args.number
            ),
            _ => format!("Commented on {}#{}.", place.target, args.number),
        }
    })?;
    Ok(ExitCode::SUCCESS)
}

fn review(cli: &Cli, args: &ReviewArgs) -> Result<ExitCode> {
    let body = args.body.text()?;
    let verdict = args.verdict.as_str();
    if verdict != proto::APPROVE && body.as_deref().is_none_or(|text| text.trim().is_empty()) {
        bail!(
            "a review that asks for changes or comments says what: give --body, or --body-file (- reads standard input)"
        );
    }
    let place = place(cli, args.repo.hub_repo.as_deref())?;
    let asked = NewReview {
        verdict: verdict.to_owned(),
        body,
        comments: Vec::new(),
        revision: args.revision,
        agent: agent(cli),
    };
    let route = format!("{}/reviews", request_route(&place.target, args.number));
    let look = format!(
        "`braid hub request show {} -R {}`",
        args.number, place.target
    );
    let said: Review = create_once(
        &place.hub,
        &route,
        &serde_json::to_value(&asked)?,
        "the review",
        &look,
    )?;
    print(cli, &serde_json::to_value(&said)?, || {
        let what = match said.verdict.as_str() {
            proto::APPROVE => "Approved",
            proto::REQUEST_CHANGES => "Asked for changes on",
            _ => "Commented on",
        };
        let counts = if said.verdict == proto::COMMENT {
            ""
        } else if said.binding {
            ": it counts"
        } else {
            ": it does not count, since only the repository's writers other than the request's author approve or ask for changes"
        };
        format!(
            "{what} revision {} of {}#{}{counts}.",
            said.revision, place.target, args.number
        )
    })?;
    Ok(ExitCode::SUCCESS)
}

/// Changes request `number` as `patch` says, and prints it.
fn change(cli: &Cli, args: &NumberArgs, patch: &RequestPatch, done: &str) -> Result<ExitCode> {
    let place = place(cli, args.repo.hub_repo.as_deref())?;
    let route = request_route(&place.target, args.number);
    let mut request: Request =
        place
            .hub
            .call_json("PATCH", &route, &[], Some(&serde_json::to_value(patch)?))?;
    request.html = None;
    print(cli, &serde_json::to_value(&request)?, || {
        format!("{done} {}{}", place.target, line(&request))
    })?;
    Ok(ExitCode::SUCCESS)
}

/// Puts `changes` on `branch` of `remote` (`path`), the head of a change
/// request: as a fast-forward when they build on it, or when it is gone;
/// otherwise the branch is taken off where it stands, if it still stands
/// there, and made again at their top, since the new revision replaces
/// what the request proposed.
fn put_on_head(
    store: &Store,
    hub: &HttpHub,
    remote: &HttpRepo<'_>,
    path: &RepoPath,
    branch: &str,
    changes: &[ObjectId],
    agent: &str,
) -> Result<()> {
    let landing = ff_only(branch, changes, agent);
    let landed = land(store, remote, &landing, OffBranch::Stack)
        .context("cannot put the changesets on the request's branch, so it was not updated")?;
    match landed.outcome.as_str() {
        "fast-forward" | "already-applied" => return Ok(()),
        "refused" => {}
        other => bail!(
            "the hub answered `{}` to putting the changesets on {branch}: the request was not updated",
            shown(other)
        ),
    }
    let stood = landed.head_before.with_context(|| {
        format!("the hub refused a landing on {branch} without saying where it stands")
    })?;
    let route = format!(
        "{}/refs/{}?expect={}",
        path.api_path(),
        encoded_ref(branch),
        stood.to_hex()
    );
    hub.call("DELETE", &route, &[], None).with_context(|| {
        format!("cannot replace {branch} of {path}, the request's head, so it was not updated")
    })?;
    let again = land(store, remote, &landing, OffBranch::Stack).with_context(|| {
        format!(
            "{branch} of {path} was taken off to be made again, and could not be: run the command again, which makes it"
        )
    })?;
    if again.outcome != "fast-forward" {
        bail!(
            "{branch} of {path} came back elsewhere before braid made it again: the request was not updated; run the command again"
        );
    }
    Ok(())
}

/// Asks the hub for request `number`'s next revision: what the head
/// branch holds, or `head` on it; prints the request.
fn revise(
    cli: &Cli,
    hub: &HttpHub,
    target: &RepoPath,
    number: u64,
    head: Option<ObjectId>,
) -> Result<ExitCode> {
    let route = format!("{}/update", request_route(target, number));
    let asked = serde_json::to_value(RequestUpdate { head })?;
    let mut request: Request = hub.call_json("POST", &route, &[], Some(&asked))?;
    request.html = None;
    print(cli, &serde_json::to_value(&request)?, || {
        format!(
            "Updated {target}#{number} to revision {}: it proposes {} now, {}.",
            request.revision,
            request.head.snapshot.short(),
            count(request.stack.len(), "changeset")
        )
    })?;
    Ok(ExitCode::SUCCESS)
}

fn update(cli: &Cli, args: &UpdateArgs) -> Result<ExitCode> {
    if let Some(head) = &args.head {
        let place = place(cli, args.repo.hub_repo.as_deref())?;
        let head = head
            .as_deref()
            .map(|text| full_id(text, "a changeset"))
            .transpose()?;
        return revise(cli, &place.hub, &place.target, args.number, head);
    }
    let found = Found::find(cli)?;
    let workspace = found.open()?;
    workspace.stack(None)?;
    let link = found.link()?;
    let hub = super::connect(&link)?;
    let linked = hub.repo(link.repo.clone());
    let target = match &args.repo.hub_repo {
        Some(text) => text
            .parse::<RepoPath>()
            .with_context(|| format!("{text:?} is not OWNER/NAME"))?,
        None => upstream_of(&linked.info()?).unwrap_or_else(|| link.repo.clone()),
    };
    let number = args.number;
    let mut request: Request = hub.call_json("GET", &request_route(&target, number), &[], None)?;
    request.html = None;
    if request.state != proto::OPEN {
        bail!(
            "{target}#{number} is {}: only an open request takes a new revision",
            shown(&request.state)
        );
    }
    let me = hub.me()?;
    if !me.login.eq_ignore_ascii_case(&request.author) {
        bail!(
            "{target}#{number} is {}'s: only its author gives it a new revision",
            shown(&request.author)
        );
    }
    let Some(head) = &request.head.repo else {
        bail!(
            "the head of {target}#{number} is in a repository you may no longer read, so it cannot take a new revision"
        );
    };
    let head_path = RepoPath::new(&head.owner, &head.name).with_context(|| {
        format!(
            "the hub names the head's repository {}/{}",
            shown(&head.owner),
            shown(&head.name)
        )
    })?;
    let work = Work::of(&found.repo, &workspace, &linked, &link, &cli.agent)?;
    if work.last() == request.head.snapshot {
        print(cli, &serde_json::to_value(&request)?, || {
            format!(
                "{target}#{number} proposes {} already: nothing to update.",
                request.head.snapshot.short()
            )
        })?;
        return Ok(ExitCode::SUCCESS);
    }
    let store = found.repo.store();
    let remote = hub.repo(head_path.clone());
    work.upload(store, &remote, &link)?;
    put_on_head(
        store,
        &hub,
        &remote,
        &head_path,
        &request.head.reference,
        &work.changes,
        &cli.agent,
    )?;
    revise(cli, &hub, &target, number, Some(work.last()))
}

/// What `request land` prints in words.
fn landed_text(target: &RepoPath, number: u64, landed: &RequestLanded) -> String {
    let land = &landed.land;
    let base = shown(&land.reference);
    let head = land
        .head
        .map_or_else(|| "(none)".to_owned(), |id| id.short());
    match land.outcome.as_str() {
        "fast-forward" => format!("Landed {target}#{number} on {base} (fast-forward), at {head}."),
        "rebased" => format!("Landed {target}#{number} on {base}, rebased: {base} is at {head}."),
        "already-applied" => format!("{base} had the work of {target}#{number} already: landed."),
        "conflicts" => format!(
            "{target}#{number} does not land on {base}: {} conflicts with {base} at {}, which did not move:\n{}The request stays open: its author settles the conflicts, and updates it (`braid hub request update --help`).",
            land.change
                .map_or_else(|| "a changeset".to_owned(), |id| id.short()),
            land.head_before
                .map_or_else(|| "(none)".to_owned(), |id| id.short()),
            conflicts_text(&land.conflicts)
        ),
        "refused" => format!(
            "{base} refused the landing of {target}#{number}: it is not a fast-forward. Land it without --mode ff-only to rebase it, or have its author update it."
        ),
        other => format!(
            "The landing of {target}#{number} on {base} ended with {}.",
            shown(other)
        ),
    }
}

fn land_request(cli: &Cli, args: &LandArgs) -> Result<ExitCode> {
    let place = place(cli, args.repo.hub_repo.as_deref())?;
    let target = &place.target;
    let number = args.number;
    let route = request_route(target, number);
    let snapshot = match &args.expect {
        Some(text) => full_id(text, "a snapshot")?,
        None => {
            let request: Request = place.hub.call_json("GET", &route, &[], None)?;
            let snapshot = request.head.snapshot;
            if !cli.json {
                println!(
                    "Landing {target}#{number} at snapshot {snapshot}, as the hub shows it now."
                );
            }
            snapshot
        }
    };
    let asked = serde_json::to_value(RequestLand {
        snapshot,
        mode: args.mode.into(),
        agent: agent(cli),
    })?;
    let mut landed: RequestLanded = place
        .hub
        .call_json("POST", &format!("{route}/land"), &[], Some(&asked))
        .map_err(|err| {
            let what = if err.may_have_happened() {
                format!(
                    "cannot tell whether {target}#{number} landed, since the hub's answer was lost: `braid hub request show {number} -R {target}` says"
                )
            } else {
                format!("{target}#{number} did not land")
            };
            anyhow::Error::new(err).context(what)
        })?;
    landed.request.html = None;
    let code = match landed.land.outcome.as_str() {
        "fast-forward" | "rebased" | "already-applied" => ExitCode::SUCCESS,
        "conflicts" => ExitCode::from(CONFLICTS),
        _ => ExitCode::from(REFUSED),
    };
    print(cli, &serde_json::to_value(&landed)?, || {
        landed_text(target, number, &landed)
    })?;
    Ok(code)
}

/// The branch `request checkout` puts a snapshot on, checked: a name a
/// branch may have, outside braid's own namespaces.
fn checkout_branch(given: Option<&str>, number: u64) -> Result<String> {
    let branch = given.map_or_else(|| format!("request/{number}"), str::to_owned);
    if !braid_repo::is_branch(&branch) || !braid_git::is_branch_name(&branch) {
        bail!("{branch:?} cannot name a branch: give --branch another name");
    }
    Ok(branch)
}

fn checkout(cli: &Cli, args: &CheckoutArgs) -> Result<ExitCode> {
    let repo = open_repo(cli).context(
        "checkout brings a change request into a repository: run it in one, such as a clone of the repository the request is on",
    )?;
    let place = place_for(Link::read(&repo)?, args.repo.hub_repo.as_deref())?;
    let target = &place.target;
    let route = request_route(target, args.number);
    let request: Request = place.hub.call_json("GET", &route, &[], None)?;
    let number = request.number;
    if !request.head.available {
        bail!(
            "what {target}#{number} brings is no longer held on the hub: it lets go of it a while after a request closes"
        );
    }
    let snapshot = request.head.snapshot;
    let branch = checkout_branch(args.branch.as_deref(), number)?;
    let store = repo.store();
    let refs = store.refs()?;
    let before = refs.get(&branch);
    // A sparse clone of this hub takes the trees under its paths only.
    let paths = place
        .link
        .as_ref()
        .filter(|link| link.url == place.url)
        .and_then(|link| link.paths.clone());
    let sparse = paths.is_some();
    // What a ref points at is whole: a branch at the snapshot needs nothing.
    let mut objects = 0;
    if before != Some(snapshot) {
        let mut have = haves(store, &refs, &[])?;
        have.truncate(place.hub.repo(target.clone()).max_ids().max(1));
        let asked = MissingRequest {
            want: vec![snapshot],
            have,
            history: None,
            paths: paths.clone(),
            limit: None,
            after: None,
        };
        let stream = place
            .hub
            .post_for_objects(&format!("{route}/missing"), &asked)?;
        let received = store_stream(store, stream)?;
        if !matches!(store.stat(&snapshot)?, Some((Kind::Changeset, _))) {
            bail!(
                "the hub's object stream did not hold changeset {snapshot}, which was asked for: nothing moved here"
            );
        }
        let prefixes: Option<Vec<Vec<u8>>> =
            paths.map(|paths| paths.into_iter().map(String::into_bytes).collect());
        whole(store, &received, prefixes.as_deref())?;
        objects = received.objects;
    }
    let outcome = match before {
        None => "created",
        Some(at) if at == snapshot => "same",
        Some(at) => {
            let mut history = repo.history();
            let forward = history.first_parent_chain_contains(snapshot, at, usize::MAX)?;
            history.flush()?;
            if !forward && !args.force {
                bail!(
                    "{branch} is at {}, which the snapshot of {target}#{number} does not hold on its first-parent chain, so it stays there: give --branch another name, or --force to move it (`braid undo` takes that back)",
                    at.short()
                );
            }
            "moved"
        }
    };
    if outcome != "same" {
        let description = format!("hub request checkout {target}#{number}");
        store
            .transact(&cli.agent, &description, |current| {
                if current.get(&branch) != before {
                    return Err(format!(
                        "{branch} moved while the request came: run the command again"
                    ));
                }
                Ok(vec![RefUpdate::new(branch.clone(), before, Some(snapshot))])
            })
            .map_err(|err| anyhow::anyhow!("{err}"))?;
    }
    let value = json!({
        "request": number,
        "repo": target.to_string(),
        "branch": branch,
        "snapshot": snapshot.to_hex(),
        "before": before.map(|id| id.to_hex()),
        "outcome": outcome,
        "objects": objects,
    });
    print(cli, &value, || {
        let shell_branch = crate::shell_word(&branch);
        let next = if sparse {
            // A sparse repository holds no workspace.
            format!("`braid cat-object {shell_branch}:PATH` reads its files.")
        } else {
            let workspace = branch.replace('/', "-");
            let workspace = if WorkspaceLink::is_valid_name(&workspace) {
                workspace
            } else {
                "review".to_owned()
            };
            format!(
                "`braid workspace add {workspace} --track {shell_branch}` makes a workspace on it."
            )
        };
        let done = match outcome {
            "same" => format!(
                "{} is at the snapshot of {target}#{number} already, {}.",
                shown(&branch),
                snapshot.short()
            ),
            _ => format!(
                "Checked out {target}#{number} as {}, at {}: {} came.",
                shown(&branch),
                snapshot.short(),
                count(objects, "object")
            ),
        };
        format!("{done}\n{next}")
    })?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    #[test]
    fn slugs_and_names() {
        for (title, slug_of) in [
            ("Faster parsing!", "faster-parsing"),
            ("  Fix #12: the parser  ", "fix-12-the-parser"),
            ("Ñandú 2", "and-2"),
            ("???", "change"),
            ("", "change"),
            ("UPPER_case", "upper-case"),
        ] {
            assert_eq!(slug(title), slug_of, "{title:?}");
        }
        let long = slug(&"word ".repeat(30));
        assert!(long.len() <= MAX_SLUG, "{long}");
        assert!(!long.ends_with('-'), "{long}");
        // Numbered names keep within the length, and never end in two
        // dashes.
        assert_eq!(numbered("fix", 1), "fix");
        assert_eq!(numbered("fix", 2), "fix-2");
        let cut = numbered(&"a".repeat(MAX_SLUG), 17);
        assert_eq!(cut.len(), MAX_SLUG);
        assert!(cut.ends_with("a-17"), "{cut}");
        assert_eq!(
            numbered(&format!("{}-b", "a".repeat(37)), 10),
            format!("{}-10", "a".repeat(37))
        );
        // Ref names keep their slashes in a route, and lose nothing else.
        assert_eq!(encoded_ref("request/ada/fix-2"), "request/ada/fix-2");
        assert_eq!(encoded_ref("a b/c?d"), "a%20b/c%3Fd");
        assert_eq!(checkout_branch(None, 3).unwrap(), "request/3");
        assert!(checkout_branch(Some("review/x"), 3).is_ok());
        for bad in ["remote/hub/x", "tags/v1", "a..b", ""] {
            assert!(checkout_branch(Some(bad), 3).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn land_rights_as_the_hub_decides_them() {
        let target: RepoPath = "ada/demo".parse().unwrap();
        let info = |permission: &str| RepoInfo {
            owner: "ada".into(),
            name: "demo".into(),
            permission: permission.into(),
            ..RepoInfo::default()
        };
        let me = |scopes: &[(&str, &[&str])]| -> Me {
            serde_json::from_value(json!({
                "login": "carol",
                "token": {
                    "id": "t1",
                    "scopes": scopes
                        .iter()
                        .map(|(repo, rights)| json!({"repo": repo, "rights": rights}))
                        .collect::<Vec<_>>(),
                    "expires": "2099-01-01T00:00:00Z",
                },
            }))
            .unwrap()
        };
        let everything = me(&[("*", &["read", "write", "land:*"])]);
        assert_eq!(
            prefix_on(&everything, &target, &info("write"), "fix").as_deref(),
            Some("request/carol/")
        );
        // A glob of one level: the branch goes one level down.
        let one = me(&[("ada/demo", &["read", "write", "land:request/*"])]);
        assert_eq!(
            prefix_on(&one, &target, &info("write"), "fix").as_deref(),
            Some("request/")
        );
        // A reader, or a token for another repository, lands nowhere here.
        assert_eq!(prefix_on(&everything, &target, &info("read"), "fix"), None);
        let elsewhere = me(&[("bob/*", &["read", "write", "land:*"])]);
        assert_eq!(prefix_on(&elsewhere, &target, &info("admin"), "fix"), None);
        let main_only = me(&[("ada/demo", &["write", "land:main"])]);
        assert_eq!(prefix_on(&main_only, &target, &info("write"), "fix"), None);
        // The token of a CI job, which posts verdicts, lands nowhere.
        let verdicts = me(&[("ada/demo", &["read", "verify:main"])]);
        assert_eq!(prefix_on(&verdicts, &target, &info("write"), "fix"), None);
        // Beside a scope that lands it takes nothing away. Nor does a scope
        // with a right this braid cannot read, as a braid older than
        // `verify:` rights reads one: that scope is left out, and the
        // others count.
        let both = me(&[
            ("ada/demo", &["read", "verify:main"]),
            ("ada/demo", &["write", "land:request/*"]),
        ]);
        assert_eq!(
            prefix_on(&both, &target, &info("write"), "fix").as_deref(),
            Some("request/")
        );
        let unread = me(&[
            ("ada/demo", &["read", "someday:main"]),
            ("ada/demo", &["write", "land:request/*"]),
        ]);
        assert_eq!(
            prefix_on(&unread, &target, &info("write"), "fix").as_deref(),
            Some("request/")
        );
    }

    /// A request read off a connection: its `Idempotency-Key` and its body.
    fn read_request(stream: &TcpStream) -> (String, String) {
        let mut reader = BufReader::new(stream);
        let mut key = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                match name.trim().to_ascii_lowercase().as_str() {
                    "idempotency-key" => key = value.trim().to_owned(),
                    "content-length" => length = value.trim().parse().unwrap(),
                    _ => {}
                }
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        (key, String::from_utf8(body).unwrap())
    }

    /// Answers a request with `status` and the JSON `body`, and closes.
    fn answer(mut stream: TcpStream, status: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }

    #[test]
    fn idempotent_retry() {
        // A hub that keeps what each key made, as the hub does: the first
        // request makes request 7, and its answer is lost, the connection
        // closed before a word; the same key again gets that answer.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let made = Arc::new(Mutex::new(0));
        let server = {
            let seen = Arc::clone(&seen);
            let made = Arc::clone(&made);
            std::thread::spawn(move || {
                let mut kept: HashMap<String, String> = HashMap::new();
                for attempt in 0..2 {
                    let (stream, _) = listener.accept().unwrap();
                    let (key, body) = read_request(&stream);
                    seen.lock().unwrap().push((key.clone(), body));
                    let reply = kept
                        .entry(key)
                        .or_insert_with(|| {
                            *made.lock().unwrap() += 1;
                            r#"{"number":7}"#.to_owned()
                        })
                        .clone();
                    if attempt == 0 {
                        drop(stream);
                    } else {
                        answer(stream, "201 Created", &reply);
                    }
                }
            })
        };
        let hub = HttpHub::new(&format!("http://{address}"), None).unwrap();
        let body = json!({"title": "Faster", "head": {"ref": "request/faster"}});
        let opened: Value = create_with(&hub, "/v1/r/ada/demo/requests", &body, "k1").unwrap();
        server.join().unwrap();
        assert_eq!(opened["number"], 7);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], seen[1], "the same key and body, sent again");
        assert_eq!(seen[0].0, "k1");
        assert_eq!(serde_json::from_str::<Value>(&seen[0].1).unwrap(), body);
        assert_eq!(*made.lock().unwrap(), 1, "made once");

        // A refusal is an answer: it is not sent again.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            read_request(&stream);
            answer(
                stream,
                "409 Conflict",
                r#"{"error":"exists","detail":"change request 7 proposes request/faster for main already"}"#,
            );
            // Nothing else comes: the next connection is refused once the
            // listener is gone.
        });
        let hub = HttpHub::new(&format!("http://{address}"), None).unwrap();
        let refused = create_with::<Value>(&hub, "/v1/r/ada/demo/requests", &body, "k2");
        server.join().unwrap();
        match refused {
            Err(HubError::Api { status: 409, body }) => assert_eq!(body.error, "exists"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn requests_in_words() {
        let target: RepoPath = "ada/demo".parse().unwrap();
        let mut request: Request = serde_json::from_value(json!({
            "number": 3,
            "title": "Faster\u{1b}[2J",
            "state": "open",
            "author": "bob",
            "head": {
                "repo": {"owner": "bob", "name": "demo"},
                "ref": "request/faster",
                "snapshot": "11".repeat(32),
                "available": true,
            },
            "base": {"ref": "main"},
            "revision": 2,
            "created": "2026-10-18T12:00:00Z",
            "updated": "2026-10-18T12:00:00Z",
        }))
        .unwrap();
        assert_eq!(line(&request), "#3 Faster\\u{1b}[2J");
        assert_eq!(head_name(&target, &request), "bob/demo:request/faster");
        request.draft = true;
        request.state = proto::LANDED.into();
        assert_eq!(line(&request), "#3 Faster\\u{1b}[2J [draft] (landed)");
        // A head in the target, or that the caller may not read: the
        // branch alone.
        request.head.repo = None;
        assert_eq!(head_name(&target, &request), "request/faster");
        let text = described(&target, &request, &[], &[]);
        assert!(text.starts_with("ada/demo#3 Faster"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");
        let conflicts = conflicts_text(&[
            json!({"path": "a.txt", "kind": "CONTENT", "hunks": [{}, {}]}),
            json!({"path": "d/x", "kind": "DIR_REMOVED", "directory": "d", "hunks": []}),
        ]);
        assert_eq!(
            conflicts,
            "  CONTENT       a.txt  (2 regions)\n  DIR_REMOVED   d/x  (directory d removed)\n"
        );
    }
}
