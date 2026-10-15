// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Workspace commands: `workspace`, `status`, `commit`, `land`, `update`,
//! `resolve`, `track` and `change create`.
//!
//! Commands that report conflicts exit with status 2, so that an agent can
//! tell them from failures (status 1) without parsing anything.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use braid_engine::{Conflict, ConflictKind, HunkSide};
use braid_format::{ChangeOp, EntryKind, ObjectId};
use braid_repo::{Location, Repo};
use braid_wc::{
    BEHIND_LIMIT, Blocker, ChangeRequest, CommitRequest, Expect, Identity, LandReport, Landed,
    Placed, RebaseProgress, UpdateReport, Updated, Workspace,
};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::{Cli, change_json, open_repo, print_json, resolve_changeset};

/// Exit status of a command that stopped on conflicts.
const CONFLICTS: u8 = 2;

/// The fields of the report `land`, `update` and `change create --land`
/// print with `--json`, for their long help.
macro_rules! report_fields {
    () => {
        "With --json, land and update print one object with these fields;
change create --land prints it as \"land\", without base and updated_paths.

  branch         the branch landed on, or updated from
  outcome        land: fast-forward, rebased, already-applied, nothing or
                 conflicts; update: up-to-date, moved, rebased or conflicts
  head_before    the branch's head before the command
  head           the branch's head after it (update never moves it)
  base           the workspace's base after the command
  landed         one {from, to} per changeset of the workspace, oldest
                 first: its id, and the id of the changeset that holds it
                 now (itself after a fast-forward, the rebased one after a
                 rebase), or null when it did not move: its changes were
                 there already, or a conflict stopped it or one before it
  merged_paths   paths both sides changed, merged line by line
  operation      the op log operation that moved the branch, or null
  updated_paths  paths of the working copy written or removed, sorted
  change         the changeset that conflicts, or null
  conflicts      one per conflict: path; kind, one of CONTENT,
                 MODIFY_DELETE, MODE, DF, DIR_REMOVED and CASE; and what
                 each side has at the path, null or an entry {kind, ...}:
                 base (the common ancestor), branch (what the branch has
                 now) and workspace (your change). A CONTENT conflict of
                 text files has hunks: the regions in conflict, each with
                 the same three sides as {start, end, text}, where start
                 and end are line numbers counted from 0 and end is
                 excluded (an empty range sits before line start)
  next           on conflicts, what to run next: braid update after land;
                 braid commit, once the files are fixed, after update; a
                 new change create on the branch after change create;
                 otherwise null

Ids in JSON are full; text output shows their first 12 hex digits."
    };
}

/// The long help of `status`.
pub(crate) const STATUS_HELP: &str = "Show how the workspace differs from its base and its branch.

With --json, status prints one object:

  workspace        the workspace's name
  path             its directory
  branch           the branch it lands on and updates from
  base             the changeset its working copy is at, or null
  head             the branch's head, or null when the branch does not exist
  behind           whether the branch has changesets the workspace does not
  behind_by        how many: the changesets from the branch's head down to
                   the one the workspace's own start from, along first
                   parents; null when there are more than 5000
  unlanded         the workspace's own changesets that the branch does not
                   have, oldest first; while a rebase is in progress, also
                   the changeset being rebased and those waiting after it
  clean            whether no change would be committed
  changes          what a commit would record, by path: kind (added,
                   modified, deleted or typechange), op, and blob and size
  added, modified, deleted, typechanged
                   the paths of changes, by kind
  needs_track      new files the secrets guard holds back until `braid
                   track`: names like .env*, *.pem or id_*, or files larger
                   than 1 MiB
  ready_to_commit  whether `braid commit` would record a changeset now:
                   there are changes, or a rebase to finish, and nothing
                   blocks it
  blocking         why `braid commit` would refuse, each {reason, path,
                   message}: markers (conflict markers remain in the file),
                   unresolved (a conflict that is not about content, not
                   marked resolved with `braid resolve`), both with the
                   conflict's kind, or needs_track; commit refuses with the
                   same list, unless --force
  rebase           null, or the rebase in progress: change (the changeset
                   being rebased), onto, pending (those waiting after it),
                   and conflicts, each {path, kind, markers, markers_remain,
                   resolved}

Ids in JSON are full; text output shows their first 12 hex digits.";

/// The long help of `land`.
pub(crate) const LAND_HELP: &str = concat!(
    "Land the workspace's changesets on its branch, in one step: a
fast-forward when they were built on the head, a rebase onto it otherwise.
The working copy then follows the landed tree. On conflicts the branch does
not move and the exit status is 2; `braid update` brings the conflicts into
the working copy.

",
    report_fields!()
);

/// The long help of `update`.
pub(crate) const UPDATE_HELP: &str = concat!(
    "Bring the workspace onto its branch's head, rebasing its own changesets
without moving the branch. On conflicts the exit status is 2: the working
copy gets the head's tree with the clean paths and conflict markers, and a
rebase is in progress until `braid commit`.

",
    report_fields!()
);

/// The long help of `change create`.
pub(crate) const CREATE_HELP: &str = concat!(
    "Make a changeset on a base without a working copy, and optionally land
it (exit status 2 on conflicts). Prints JSON whether or not --json is
given: change, the new changeset's id; base; and land, the report of the
landing, or null.

",
    report_fields!()
);

#[derive(Debug, Subcommand)]
pub(crate) enum WorkspaceAction {
    /// Create a workspace and write its files.
    Add {
        /// The workspace's name: letters, digits, '.', '_' or '-'.
        name: String,
        /// Its directory, which must not exist or be empty (default:
        /// ./<name>).
        path: Option<PathBuf>,
        /// The changeset to start at: a ref, an id or a short id (default:
        /// the head of the tracked branch, or nothing if it does not exist
        /// yet).
        #[arg(long, value_name = "REV")]
        at: Option<String>,
        /// The branch the workspace lands on and updates from.
        #[arg(long, value_name = "BRANCH", default_value = "main")]
        track: String,
    },
    /// List the repository's workspaces.
    List,
    /// Remove a workspace: its directory and its state.
    Remove {
        /// The workspace's name.
        name: String,
        /// Remove it even with uncommitted changes, unlanded changesets or
        /// a rebase in progress.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct IdentityArgs {
    /// Who records changesets, as "Name <email>" (default: BRAID_AUTHOR,
    /// then name and email in .braid/config).
    #[arg(long, env = "BRAID_AUTHOR", value_name = "IDENTITY")]
    author: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct CommitArgs {
    /// The message. While a rebase is in progress it may be left out, and
    /// the rebased changeset's own is kept; one given replaces it.
    #[arg(short, long)]
    message: Option<String>,
    /// Commit even if conflict markers remain or conflicts are not marked
    /// resolved; new files waiting for `braid track` stay out.
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    identity: IdentityArgs,
    /// Commit only these files and directories (default: everything).
    paths: Vec<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct LandArgs {
    /// The branch to land on (default: the workspace's branch).
    #[arg(long, value_name = "BRANCH")]
    onto: Option<String>,
    #[command(flatten)]
    identity: IdentityArgs,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ChangeAction {
    /// Make a changeset on a base without a working copy, and optionally
    /// land it (exit status 2 on conflicts). Prints the ids as JSON.
    #[command(long_about = CREATE_HELP)]
    Create(CreateArgs),
}

#[derive(Debug, Args)]
pub(crate) struct CreateArgs {
    /// The changeset to build on: a ref, an id, a short id (6 hex digits or
    /// more), or "none" for a first changeset.
    #[arg(long, value_name = "REV")]
    base: String,
    /// Set a file: PATH=@FILE takes the content of FILE, PATH=@- reads
    /// standard input. Repeat for several.
    #[arg(long = "set", value_name = "PATH=@FILE")]
    sets: Vec<OsString>,
    /// Delete a file. Repeat for several.
    #[arg(long = "delete", value_name = "PATH")]
    deletes: Vec<OsString>,
    /// Require the base to hold this blob at PATH, or nothing ("absent"),
    /// before anything is made. Repeat for several.
    #[arg(long = "expect", value_name = "PATH=ID|absent")]
    expects: Vec<OsString>,
    /// The message.
    #[arg(short, long)]
    message: String,
    /// Land the changeset on this branch.
    #[arg(long, value_name = "BRANCH")]
    land: Option<String>,
    #[command(flatten)]
    identity: IdentityArgs,
}

/// The repository and the workspace a command acts on.
struct Target {
    repo: Repo,
    name: String,
    /// Where its `.braid` file was found, when it was found by looking.
    dir: Option<PathBuf>,
}

impl Target {
    /// The workspace named by `--workspace`, or the one containing the
    /// current directory.
    fn find(cli: &Cli) -> Result<Target> {
        if let Some(name) = &cli.workspace {
            return Ok(Target {
                repo: open_repo(cli)?,
                name: name.clone(),
                dir: None,
            });
        }
        let cwd = std::env::current_dir()?;
        match braid_repo::locate(&cwd)? {
            Location::Workspace { dir, link } => {
                let repo = match &cli.repo {
                    Some(root) => Repo::open(root)?,
                    None => Repo::open(&link.repository)?,
                };
                Ok(Target {
                    repo,
                    name: link.name,
                    dir: Some(dir),
                })
            }
            Location::Root(_) => Err(braid_wc::Error::NotAWorkspace.into()),
        }
    }

    fn open(&self) -> Result<Workspace<'_>> {
        Ok(match &self.dir {
            Some(dir) => Workspace::at(&self.repo, &self.name, dir)?,
            None => Workspace::open(&self.repo, &self.name)?,
        })
    }
}

fn identity(args: &IdentityArgs, repo: &Repo) -> Result<Identity> {
    Ok(Identity::resolve(args.author.as_deref(), repo)?)
}

/// Paths given on the command line, as paths of the workspace.
fn workspace_paths(workspace: &Workspace<'_>, given: &[PathBuf]) -> Result<Vec<Vec<u8>>> {
    let root = std::fs::canonicalize(workspace.dir())
        .with_context(|| format!("cannot open {}", workspace.dir().display()))?;
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
    given
        .iter()
        .map(|path| Ok(braid_wc::workspace_path(&root, &cwd, path)?))
        .collect()
}

fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

/// `1 path`, `3 paths`.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn hex(id: Option<ObjectId>) -> Value {
    id.map_or(Value::Null, |id| Value::String(id.to_hex()))
}

fn short(id: Option<ObjectId>) -> String {
    id.map_or_else(|| "(none)".to_owned(), |id| id.short())
}

pub(crate) fn workspace(cli: &Cli, action: &WorkspaceAction) -> Result<ExitCode> {
    match action {
        WorkspaceAction::Add {
            name,
            path,
            at,
            track,
        } => {
            let repo = open_repo(cli)?;
            let at = match at {
                Some(revision) => Some(resolve_changeset(&repo, revision)?),
                None => repo.store().refs()?.get(track),
            };
            let dir = match path {
                Some(path) => path.clone(),
                None => std::env::current_dir()?.join(name),
            };
            let (workspace, files) = Workspace::create(&repo, name, &dir, at, track)?;
            if cli.json {
                print_json(&json!({
                    "name": workspace.name(),
                    "path": workspace.dir().to_string_lossy(),
                    "branch": track,
                    "base": hex(at),
                    "files": files,
                }))?;
            } else {
                println!(
                    "Created workspace {} in {} on {track} at {}, {}",
                    workspace.name(),
                    workspace.dir().display(),
                    short(at),
                    count(files, "file")
                );
            }
        }
        WorkspaceAction::List => {
            let repo = open_repo(cli)?;
            let all = Workspace::list(&repo)?;
            if cli.json {
                let items: Vec<Value> = all
                    .iter()
                    .map(|(name, state)| {
                        json!({
                            "name": name,
                            "path": state.path.to_string_lossy(),
                            "branch": state.branch,
                            "base": hex(state.base),
                            "rebase": state.rebase.is_some(),
                            "present": state.path.join(braid_repo::DIR).is_file(),
                        })
                    })
                    .collect();
                print_json(&Value::Array(items))?;
            } else {
                for (name, state) in &all {
                    println!(
                        "{name}  {}  {}  {}{}",
                        state.branch,
                        short(state.base),
                        state.path.display(),
                        if state.rebase.is_some() {
                            "  (rebase in progress)"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
        WorkspaceAction::Remove { name, force } => {
            let repo = open_repo(cli)?;
            Workspace::remove(&repo, name, *force)?;
            if cli.json {
                print_json(&json!({ "name": name, "removed": true }))?;
            } else {
                println!("Removed workspace {name}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn status(cli: &Cli) -> Result<ExitCode> {
    let target = Target::find(cli)?;
    let workspace = target.open()?;
    let report = workspace.status()?;
    let state = &report.state;
    let status = &report.status;
    if cli.json {
        let by_kind = |kind: braid_wc::ChangeKind| -> Vec<String> {
            status
                .changes
                .iter()
                .filter(|change| change.kind == kind)
                .map(|change| show(&change.path))
                .collect()
        };
        let rebase = state.rebase.as_ref().map(|rebase| {
            json!({
                "change": rebase.change.to_hex(),
                "onto": rebase.onto.to_hex(),
                "pending": rebase.pending.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
                "conflicts": report.conflicts.iter().map(|(conflict, remain)| json!({
                    "path": show(&conflict.path),
                    "kind": conflict.code,
                    "markers": conflict.markers,
                    "markers_remain": remain,
                    "resolved": conflict.resolved,
                })).collect::<Vec<_>>(),
            })
        });
        print_json(&json!({
            "workspace": workspace.name(),
            "path": workspace.dir().to_string_lossy(),
            "branch": state.branch,
            "base": hex(state.base),
            "head": hex(report.head),
            "behind": report.behind(),
            "behind_by": report.behind_by,
            "clean": status.is_clean(),
            "unlanded": report.unlanded.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
            "changes": status.changes.iter().map(|change| {
                let mut value = change_json(&braid_format::Change {
                    path: change.path.clone(),
                    op: change.op.clone(),
                });
                value["kind"] = Value::String(change.kind.name().to_owned());
                value
            }).collect::<Vec<_>>(),
            "added": by_kind(braid_wc::ChangeKind::Added),
            "modified": by_kind(braid_wc::ChangeKind::Modified),
            "deleted": by_kind(braid_wc::ChangeKind::Deleted),
            "typechanged": by_kind(braid_wc::ChangeKind::TypeChanged),
            "needs_track": status.needs_track.iter().map(|p| show(p)).collect::<Vec<_>>(),
            "ready_to_commit": report.ready_to_commit(),
            "blocking": report.blocking.iter().map(blocker_json).collect::<Vec<_>>(),
            "rebase": rebase,
        }))?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "Workspace {} on {}, at {}",
        workspace.name(),
        state.branch,
        short(state.base)
    )?;
    if !report.unlanded.is_empty() {
        writeln!(
            out,
            "{} not landed on {}",
            count(report.unlanded.len(), "changeset"),
            state.branch
        )?;
    }
    if report.behind() {
        let ahead = match report.behind_by {
            Some(n) => count(n, "changeset"),
            None => format!("more than {BEHIND_LIMIT} changesets"),
        };
        writeln!(
            out,
            "{} is {ahead} ahead, at {}: run `braid update`",
            state.branch,
            short(report.head)
        )?;
    }
    if let Some(rebase) = &state.rebase {
        writeln!(
            out,
            "Rebase of {} in progress onto {}; resolve, then `braid commit`",
            rebase.change.short(),
            rebase.onto.short()
        )?;
        for (conflict, remain) in &report.conflicts {
            let note = if conflict.markers {
                if *remain { "markers remain" } else { "edited" }
            } else if conflict.resolved {
                "resolved"
            } else {
                "run `braid resolve` once settled"
            };
            writeln!(
                out,
                "  {:<13} {}  ({note})",
                conflict.code,
                show(&conflict.path)
            )?;
        }
    }
    if status.is_clean() {
        writeln!(out, "Nothing to commit.")?;
    } else {
        writeln!(out, "Changes:")?;
        for change in &status.changes {
            writeln!(out, "  {:<11} {}", change.kind.name(), show(&change.path))?;
        }
    }
    if !status.needs_track.is_empty() {
        writeln!(out, "New files not committed until `braid track <path>`:")?;
        for path in &status.needs_track {
            writeln!(out, "  {}", show(path))?;
        }
    }
    if !report.blocking.is_empty() {
        writeln!(out, "Not ready to commit:")?;
        for blocker in &report.blocking {
            writeln!(out, "  - {blocker}")?;
        }
    } else if report.ready_to_commit() {
        writeln!(out, "Ready to commit.")?;
    }
    Ok(ExitCode::SUCCESS)
}

/// One reason a commit would be refused, as data: what it is, the path,
/// the kind of conflict it comes from, and what to do.
fn blocker_json(blocker: &Blocker) -> Value {
    let mut value = json!({
        "reason": blocker.code(),
        "path": show(blocker.path()),
        "message": blocker.to_string(),
    });
    if let Some(kind) = blocker.conflict() {
        value["kind"] = Value::String(kind.to_owned());
    }
    value
}

fn progress_json(progress: &RebaseProgress) -> Value {
    match progress {
        RebaseProgress::None => Value::Null,
        RebaseProgress::Finished { tip } => json!({ "outcome": "finished", "tip": tip.to_hex() }),
        RebaseProgress::Stopped { change, conflicts } => json!({
            "outcome": "conflicts",
            "change": change.to_hex(),
            "conflicts": conflicts.iter().map(conflict_json).collect::<Vec<_>>(),
        }),
    }
}

pub(crate) fn commit(cli: &Cli, args: &CommitArgs) -> Result<ExitCode> {
    let target = Target::find(cli)?;
    let workspace = target.open()?;
    let request = CommitRequest {
        message: args.message.as_ref().map(|m| m.as_bytes().to_vec()),
        paths: workspace_paths(&workspace, &args.paths)?,
        author: None,
        committer: identity(&args.identity, &target.repo)?,
        agent: cli.agent.clone(),
        force: args.force,
    };
    let outcome = workspace.commit(&request)?;
    let stopped = matches!(outcome.rebase, RebaseProgress::Stopped { .. });
    if cli.json {
        print_json(&json!({
            "changeset": hex(outcome.changeset),
            "changes": outcome.changes,
            "message_replaced": outcome.message_replaced,
            "rebase": progress_json(&outcome.rebase),
        }))?;
    } else {
        match outcome.changeset {
            Some(id) => println!(
                "Committed {} ({})",
                id.short(),
                count(outcome.changes, "path")
            ),
            None => println!("Nothing left to record for this changeset"),
        }
        if outcome.message_replaced {
            println!("The message given replaces the rebased changeset's own.");
        }
        match &outcome.rebase {
            RebaseProgress::None => {}
            RebaseProgress::Finished { tip } => {
                println!(
                    "Rebase finished at {}; `braid land` when ready",
                    tip.short()
                );
            }
            RebaseProgress::Stopped { change, conflicts } => {
                println!("The next changeset, {}, conflicts too:", change.short());
                print_conflicts(conflicts);
            }
        }
    }
    Ok(if stopped {
        ExitCode::from(CONFLICTS)
    } else {
        ExitCode::SUCCESS
    })
}

/// What `land`, `update` and `change create --land` report, in one shape:
/// the fields their long help lists.
struct Report<'a> {
    branch: &'a str,
    outcome: &'static str,
    head_before: Option<ObjectId>,
    head: Option<ObjectId>,
    placed: &'a [Placed],
    merged_paths: &'a [Vec<u8>],
    operation: Option<u64>,
    change: Option<ObjectId>,
    conflicts: &'a [Conflict],
    next: Option<String>,
    /// For a workspace: its base afterwards, and the paths of its working
    /// copy written or removed.
    workspace: Option<(Option<ObjectId>, &'a [Vec<u8>])>,
}

impl<'a> Report<'a> {
    fn landing(report: &'a LandReport) -> Report<'a> {
        let (outcome, change, conflicts): (_, _, &[Conflict]) = match &report.landed {
            Landed::Nothing => ("nothing", None, &[]),
            Landed::FastForward(_) => ("fast-forward", None, &[]),
            Landed::Rebased(_) => ("rebased", None, &[]),
            Landed::AlreadyApplied => ("already-applied", None, &[]),
            Landed::Conflicts { change, conflicts } => ("conflicts", Some(*change), conflicts),
        };
        Report {
            branch: &report.branch,
            outcome,
            head_before: report.head_before,
            head: report.head(),
            placed: &report.placed,
            merged_paths: &report.merged_paths,
            operation: report.operation,
            change,
            conflicts,
            next: None,
            workspace: None,
        }
    }

    fn update(report: &'a UpdateReport) -> Report<'a> {
        let (outcome, change, conflicts): (_, _, &[Conflict]) = match &report.updated {
            Updated::UpToDate => ("up-to-date", None, &[]),
            Updated::Moved { .. } => ("moved", None, &[]),
            Updated::Rebased { .. } => ("rebased", None, &[]),
            Updated::Conflicts {
                change, conflicts, ..
            } => ("conflicts", Some(*change), conflicts),
        };
        Report {
            branch: &report.branch,
            outcome,
            // An update reads the head and never moves it.
            head_before: report.head,
            head: report.head,
            placed: &report.placed,
            merged_paths: &report.merged_paths,
            operation: None,
            change,
            conflicts,
            next: (!conflicts.is_empty()).then(|| "braid commit".to_owned()),
            workspace: Some((report.base, &report.updated_paths)),
        }
    }

    fn json(&self) -> Value {
        let mut value = json!({
            "branch": self.branch,
            "outcome": self.outcome,
            "head_before": hex(self.head_before),
            "head": hex(self.head),
            "landed": self.placed.iter().map(|placed| json!({
                "from": placed.from.to_hex(),
                "to": hex(placed.to),
            })).collect::<Vec<_>>(),
            "merged_paths": self.merged_paths.iter().map(|p| show(p)).collect::<Vec<_>>(),
            "operation": self.operation,
            "change": hex(self.change),
            "conflicts": self.conflicts.iter().map(conflict_json).collect::<Vec<_>>(),
            "next": self.next,
        });
        if let Some((base, updated)) = self.workspace {
            value["base"] = hex(base);
            value["updated_paths"] = updated.iter().map(|p| Value::String(show(p))).collect();
        }
        value
    }

    fn exit_code(&self) -> ExitCode {
        if self.conflicts.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(CONFLICTS)
        }
    }

    /// `, 1 path merged`, or nothing when none was.
    fn merged(&self) -> String {
        match self.merged_paths.len() {
            0 => String::new(),
            n => format!(" ({} merged)", count(n, "path")),
        }
    }

    /// Says how many paths of the working copy changed, if any did.
    fn print_updated(&self) {
        if let Some((_, updated)) = self.workspace
            && !updated.is_empty()
        {
            println!(
                "Updated {} in the working copy",
                count(updated.len(), "path")
            );
        }
    }
}

pub(crate) fn land(cli: &Cli, args: &LandArgs) -> Result<ExitCode> {
    let target = Target::find(cli)?;
    let workspace = target.open()?;
    let committer = identity(&args.identity, &target.repo)?;
    let landing = workspace.land(args.onto.as_deref(), &cli.agent, &committer)?;
    let mut report = Report::landing(&landing);
    report.workspace = Some((workspace.state()?.base, &landing.updated_paths));
    if !report.conflicts.is_empty() {
        report.next = Some("braid update".to_owned());
    }
    if cli.json {
        print_json(&report.json())?;
        return Ok(report.exit_code());
    }
    let branch = &landing.branch;
    let several = landing.placed.len() > 1;
    match &landing.landed {
        Landed::Nothing => println!("Nothing to land on {branch}"),
        Landed::FastForward(id) if several => println!(
            "Landed {} on {branch} (fast-forward), up to {}",
            count(landing.placed.len(), "changeset"),
            id.short()
        ),
        Landed::FastForward(id) => println!("Landed {} on {branch} (fast-forward)", id.short()),
        Landed::Rebased(id) if several => println!(
            "Landed {} on {branch}, rebased onto {}; the last is {}{}",
            count(landing.placed.len(), "changeset"),
            short(landing.head_before),
            id.short(),
            report.merged()
        ),
        Landed::Rebased(id) => println!(
            "Landed on {branch} as {}, rebased onto {}{}",
            id.short(),
            short(landing.head_before),
            report.merged()
        ),
        Landed::AlreadyApplied => println!("{branch} already has these changes"),
        Landed::Conflicts { change, conflicts } => {
            println!(
                "{} conflicts with {branch} at {}; {branch} did not move:",
                change.short(),
                short(landing.head_before),
            );
            print_conflicts(conflicts);
            println!("Run `braid update` to bring the conflicts into the working copy.");
        }
    }
    report.print_updated();
    Ok(report.exit_code())
}

pub(crate) fn update(cli: &Cli, args: &IdentityArgs) -> Result<ExitCode> {
    let target = Target::find(cli)?;
    let workspace = target.open()?;
    let committer = identity(args, &target.repo)?;
    let updated = workspace.update(&committer)?;
    let report = Report::update(&updated);
    if cli.json {
        print_json(&report.json())?;
        return Ok(report.exit_code());
    }
    match &updated.updated {
        Updated::UpToDate => println!("Up to date"),
        Updated::Moved { head } => println!("Moved to {}", head.short()),
        Updated::Rebased { head, tip } => {
            println!(
                "Rebased onto {}; now at {}{}",
                head.short(),
                tip.short(),
                report.merged()
            );
        }
        Updated::Conflicts {
            head,
            change,
            conflicts,
        } => {
            println!(
                "{} conflicts with {}; the working copy has the merge so far:",
                change.short(),
                head.short()
            );
            print_conflicts(conflicts);
            println!("Fix the files, `braid resolve` the other conflicts, then `braid commit`.");
        }
    }
    report.print_updated();
    Ok(report.exit_code())
}

pub(crate) fn resolve(cli: &Cli, paths: &[PathBuf]) -> Result<ExitCode> {
    let target = Target::find(cli)?;
    let workspace = target.open()?;
    let paths = workspace_paths(&workspace, paths)?;
    let remaining = workspace.resolve(&paths)?;
    if cli.json {
        print_json(&json!({
            "resolved": paths.iter().map(|p| show(p)).collect::<Vec<_>>(),
            "remaining": remaining.iter().map(|c| show(&c.path)).collect::<Vec<_>>(),
        }))?;
    } else if remaining.is_empty() {
        println!("Every conflict needing `braid resolve` is resolved");
    } else {
        println!("Still unresolved:");
        for conflict in &remaining {
            println!("  {:<13} {}", conflict.code, show(&conflict.path));
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn track(cli: &Cli, paths: &[PathBuf]) -> Result<ExitCode> {
    let target = Target::find(cli)?;
    let workspace = target.open()?;
    let paths = workspace_paths(&workspace, paths)?;
    if paths.iter().any(Vec::is_empty) {
        bail!("give files or directories inside the workspace, not its root");
    }
    let marks = workspace.track(&paths)?;
    if cli.json {
        print_json(&json!({ "tracked": marks.iter().map(|p| show(p)).collect::<Vec<_>>() }))?;
    } else {
        for path in &paths {
            println!("New files at {} will be committed", show(path));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The bytes of a command-line argument.
fn arg_bytes(arg: &OsStr) -> Result<Vec<u8>> {
    braid_repo::text::path_bytes(Path::new(arg))
        .with_context(|| format!("{} is not valid here", arg.to_string_lossy()))
}

pub(crate) fn change(cli: &Cli, action: &ChangeAction) -> Result<ExitCode> {
    // The output is JSON whether or not `--json` is given.
    let ChangeAction::Create(args) = action;
    let repo = open_repo(cli)?;
    let base = match args.base.as_str() {
        "none" => None,
        revision => Some(resolve_changeset(&repo, revision)?),
    };
    let mut stdin_used = false;
    let mut sets = Vec::with_capacity(args.sets.len());
    for set in &args.sets {
        let bytes = arg_bytes(set)?;
        let at = bytes
            .windows(2)
            .position(|pair| pair == b"=@")
            .with_context(|| {
                format!(
                    "--set takes PATH=@FILE or PATH=@-, not {}",
                    set.to_string_lossy()
                )
            })?;
        let (path, source) = (bytes[..at].to_vec(), &bytes[at + 2..]);
        let content = if source == b"-" {
            if stdin_used {
                bail!("only one --set can read standard input");
            }
            stdin_used = true;
            let mut content = Vec::new();
            io::stdin()
                .lock()
                .read_to_end(&mut content)
                .context("cannot read standard input")?;
            content
        } else {
            let file = braid_repo::text::path_from_bytes(source).context("invalid file name")?;
            std::fs::read(&file).with_context(|| format!("cannot read {}", file.display()))?
        };
        sets.push((path, content));
    }
    let deletes = args
        .deletes
        .iter()
        .map(|path| arg_bytes(path))
        .collect::<Result<Vec<_>>>()?;
    let mut expects = Vec::with_capacity(args.expects.len());
    for expect in &args.expects {
        let bytes = arg_bytes(expect)?;
        let at = bytes
            .iter()
            .rposition(|byte| *byte == b'=')
            .with_context(|| {
                format!(
                    "--expect takes PATH=ID or PATH=absent, not {}",
                    expect.to_string_lossy()
                )
            })?;
        let value = std::str::from_utf8(&bytes[at + 1..]).unwrap_or("");
        let expected = match value {
            "absent" => Expect::Absent,
            id => Expect::Blob(id.parse().with_context(|| {
                format!("--expect needs a full blob id or \"absent\", not {id:?}")
            })?),
        };
        expects.push((bytes[..at].to_vec(), expected));
    }
    let author = identity(&args.identity, &repo)?;
    let request = ChangeRequest {
        base,
        sets,
        deletes,
        expects,
        message: args.message.as_bytes().to_vec(),
        author: author.clone(),
        agent: cli.agent.clone(),
    };
    let change = braid_wc::create_change(repo.store(), &request)?;
    let landing = match &args.land {
        Some(branch) => Some(braid_wc::land_change(
            repo.store(),
            branch,
            change,
            &cli.agent,
            &author,
        )?),
        None => None,
    };
    let conflicted = matches!(
        landing.as_ref().map(|report| &report.landed),
        Some(Landed::Conflicts { .. })
    );
    // Always JSON: this command is for agents, which want the ids.
    let land = match &landing {
        Some(landing) => {
            let mut report = Report::landing(landing);
            if !report.conflicts.is_empty() {
                // The change was built on a base the branch has moved past:
                // it is made again on the new head.
                report.next = Some(format!("braid change create --base {}", landing.branch));
            }
            report.json()
        }
        None => Value::Null,
    };
    print_json(&json!({
        "change": change.to_hex(),
        "base": hex(base),
        "land": land,
    }))?;
    Ok(if conflicted {
        ExitCode::from(CONFLICTS)
    } else {
        ExitCode::SUCCESS
    })
}

fn entry_json(entry: &Option<EntryKind>) -> Value {
    match entry {
        None => Value::Null,
        Some(EntryKind::Tree { tree }) => json!({ "kind": "tree", "tree": tree.to_hex() }),
        Some(kind) => match ChangeOp::from_entry(kind) {
            Some(op) => {
                let mut value = change_json(&braid_format::Change {
                    path: Vec::new(),
                    op,
                });
                if let Some(object) = value.as_object_mut() {
                    object.remove("path");
                    if let Some(op) = object.remove("op") {
                        object.insert("kind".to_owned(), op);
                    }
                }
                value
            }
            None => Value::Null,
        },
    }
}

fn side_json(side: &HunkSide) -> Value {
    json!({
        "start": side.lines.start,
        "end": side.lines.end,
        "text": String::from_utf8_lossy(&side.text),
    })
}

/// A conflict as data, with its sides named from the caller's point of
/// view: the path, the kind's code, the entries of `base` (the common
/// ancestor), `branch` (what the branch has now) and `workspace` (the
/// change: the workspace's, or the one `change create` made), and for
/// content conflicts the hunks, with the same three sides.
pub(crate) fn conflict_json(conflict: &Conflict) -> Value {
    let mut value = json!({
        "path": show(&conflict.path),
        "kind": conflict.kind.code(),
        "base": entry_json(&conflict.base),
        "branch": entry_json(&conflict.ours),
        "workspace": entry_json(&conflict.theirs),
        "hunks": [],
    });
    match &conflict.kind {
        ConflictKind::Content { hunks } => {
            value["hunks"] = hunks
                .iter()
                .map(|hunk| {
                    json!({
                        "base": side_json(&hunk.base),
                        "branch": side_json(&hunk.ours),
                        "workspace": side_json(&hunk.theirs),
                    })
                })
                .collect();
        }
        ConflictKind::DirectoryRemoved { directory } => {
            value["directory"] = Value::String(show(directory));
        }
        ConflictKind::Case { existing } => {
            value["existing"] = Value::String(show(existing));
        }
        _ => {}
    }
    value
}

fn print_conflicts(conflicts: &[Conflict]) {
    for conflict in conflicts {
        let detail = match &conflict.kind {
            ConflictKind::Content { hunks } if !hunks.is_empty() => {
                format!("  ({})", count(hunks.len(), "region"))
            }
            ConflictKind::DirectoryRemoved { directory } => {
                format!("  (directory {} removed)", show(directory))
            }
            ConflictKind::Case { existing } => format!("  (collides with {})", show(existing)),
            _ => String::new(),
        };
        println!(
            "  {:<13} {}{detail}",
            conflict.kind.code(),
            show(&conflict.path)
        );
    }
}
