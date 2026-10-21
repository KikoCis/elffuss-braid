// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid agents init`: the rules for coding agents, written into the
//! project's `AGENTS.md` between two marker lines, so that every agent
//! session, whatever runs it, knows to work in a braid workspace of its own
//! and to land through braid. A `CLAUDE.md` or a `GEMINI.md` that does not
//! include `AGENTS.md` gets the line that does.
//!
//! Text outside the markers is never touched, and running the command again
//! rewrites the section in place, so it is idempotent. Marker lines inside
//! fenced code blocks are not markers. Every file is checked before any is
//! written, and all of them are prepared before any is replaced.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use braid_repo::{Location, Repo};
use braid_wc::Workspace;
use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};

use crate::work::{branches, listed};
use crate::{Cli, open_repo, print_json};

/// The line that opens the braid section of `AGENTS.md`.
pub(crate) const START: &str = "<!-- braid:start -->";

/// The line that closes it.
pub(crate) const END: &str = "<!-- braid:end -->";

/// The file the section goes in.
const AGENTS_FILE: &str = "AGENTS.md";

/// The file Claude Code reads, which should include `AGENTS.md`.
const CLAUDE_FILE: &str = "CLAUDE.md";

/// The file Gemini CLI reads, which should include `AGENTS.md`.
const GEMINI_FILE: &str = "GEMINI.md";

/// The line that makes `CLAUDE.md` or `GEMINI.md` include `AGENTS.md`.
const INCLUDE: &str = "@AGENTS.md";

/// What the section's first comment says just before the branch it names.
const BRANCH_NOTE: &str = "(branch: ";

/// The end of every refusal: nothing is written when any check fails.
const NOTHING: &str = "nothing was written";

/// The long help of `agents init`.
pub(crate) const INIT_HELP: &str =
    "Write the rules for coding agents into the project's AGENTS.md, so that
every agent session, whatever runs it, works in a braid workspace of its
own and lands through braid.

The rules go in a section between two lines, <!-- braid:start --> and
<!-- braid:end -->: at the end of AGENTS.md, which is created if there is
none, and in place on later runs. Text outside the markers is never
touched, and running the command twice changes nothing. Marker lines
inside fenced code blocks are not markers. Markers that do not pair up (one
without the other, the end first, or either of them twice) are refused
before anything is written, and so is a file that ends inside a fenced
code block, where the section would be read as code.

The project's directory is the root of the repository or of the workspace
the current directory is in, or the current directory; --workspace names a
workspace to write in, --repo a repository whose root to write in, and
--dir any directory.

The section names the branch agents land on: --branch, which must be a
branch the repository has; else the one the section names already, if the
repository has it; else, inside a workspace, the one it tracks; else main,
master, or the repository's only branch. Without a braid repository, or
with one that has no branch yet, there is no branch to name: set braid up
first, or pass --branch. The section writes the name into shell commands,
so it takes ASCII letters, digits, '.', '_', '/' and '-' only, and does not
start with '-'.

Claude Code reads CLAUDE.md, and Gemini CLI reads GEMINI.md, instead of
AGENTS.md: each of them that exists and does not include AGENTS.md gets the
line @AGENTS.md at its end. --no-claude and --no-gemini leave them alone.
Neither is ever created.

Any of the three may be a symbolic link to a file inside the project, and
the file it points to is the one written. A link that leads outside the
project, a name that is not a regular file, a read-only file, and a file
with hard links other than these three names are refused. Files are
written through a temporary file and a rename, every one of them prepared
before any is replaced; a file with hard links is written in place, so
that its names stay one file.

With --dry-run nothing is written: the command makes the same checks, says
what it would do, and prints the section when AGENTS.md would change. With
--json it prints one object:

  dir              the project's directory
  branch           the branch the section names
  previous_branch  the branch it named before, when that was another;
                   otherwise null
  repository       the root of the braid repository, or null when there is
                   none
  dry_run          whether nothing was written
  created          the files created (or, with --dry-run, that would be)
  updated          the files changed
  unchanged        the files already as they should be
  paths            for each file in those lists, the file written: its own
                   path from dir, or the file its link points to

A CLAUDE.md or a GEMINI.md that does not exist, or that --no-claude or
--no-gemini leaves alone, is in none of the lists.";

#[derive(Debug, Subcommand)]
pub(crate) enum AgentsAction {
    /// Write the braid section of the project's AGENTS.md, and make its
    /// CLAUDE.md and GEMINI.md include AGENTS.md.
    #[command(long_about = INIT_HELP)]
    Init(InitArgs),
}

#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    /// The project's root directory (default: the root of the repository or
    /// the workspace the current directory is in, or else the current
    /// directory).
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,
    /// The branch agents land on, one the repository has (default: the one
    /// the section names already, if the repository has it; inside a
    /// workspace, the one it tracks; otherwise main, master, or the
    /// repository's only branch).
    #[arg(long, value_name = "BRANCH")]
    branch: Option<String>,
    /// Say what would change, and write nothing.
    #[arg(long)]
    dry_run: bool,
    /// Leave CLAUDE.md alone.
    #[arg(long)]
    no_claude: bool,
    /// Leave GEMINI.md alone.
    #[arg(long)]
    no_gemini: bool,
}

pub(crate) fn agents(cli: &Cli, action: &AgentsAction) -> Result<ExitCode> {
    let AgentsAction::Init(args) = action;
    init(cli, args)
}

/// What a file needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Create,
    Update,
    Unchanged,
}

/// One file the command looked at, and what it does to it.
struct Planned {
    /// The name the project knows it by: `AGENTS.md`, `CLAUDE.md` or
    /// `GEMINI.md`.
    name: &'static str,
    target: Target,
    action: Action,
    /// The new contents, unless unchanged.
    contents: Option<String>,
    /// What the text output says about it, once done.
    done: String,
    /// What it says with `--dry-run`.
    would: String,
}

fn init(cli: &Cli, args: &InitArgs) -> Result<ExitCode> {
    let place = Place::find(cli, args.dir.as_deref())?;
    let dir = &place.dir;
    let mut roots = vec![fs::canonicalize(dir).unwrap_or_else(|_| dir.clone())];
    if let Some(repo) = &place.repo
        && let Ok(root) = fs::canonicalize(repo.root())
    {
        roots.push(root);
    }
    let guides = guide_files(dir);

    let agents_target = match locate_file(dir, AGENTS_FILE, &roots, &guides)? {
        Located::Missing => Target::new_file(dir, AGENTS_FILE),
        Located::Dangling(to) => bail!(
            "{AGENTS_FILE} is a symbolic link to {}, which does not exist: {NOTHING}",
            to.display()
        ),
        Located::File(target) => target,
    };
    let current = agents_target.read()?;
    let scanned = match &current {
        Some(text) => scan_section(text).map_err(|why| {
            anyhow!(
                "{} has unbalanced braid markers: {why}. Fix them by hand: {NOTHING}",
                agents_target.shown
            )
        })?,
        None => Scanned::default(),
    };
    let named = match (&current, &scanned.section) {
        (Some(text), Some(range)) => named_branch(&text[range.clone()]),
        _ => None,
    };
    let choice = choose_branch(args.branch.as_deref(), named, &place)?;
    let mut planned = vec![plan_agents(
        agents_target,
        current.as_deref(),
        &scanned,
        &choice.branch,
    )?];
    for (name, skipped) in [(CLAUDE_FILE, args.no_claude), (GEMINI_FILE, args.no_gemini)] {
        if !skipped && let Some(file) = plan_include(dir, name, &roots, &guides, &planned)? {
            planned.push(file);
        }
    }

    if !args.dry_run {
        write_all(&planned)?;
    }

    if cli.json {
        let names = |action: Action| -> Vec<&str> {
            planned
                .iter()
                .filter(|file| file.action == action)
                .map(|file| file.name)
                .collect()
        };
        let paths: Map<String, Value> = planned
            .iter()
            .map(|file| (file.name.to_owned(), Value::from(file.target.shown.clone())))
            .collect();
        print_json(&json!({
            "dir": dir.to_string_lossy(),
            "branch": choice.branch,
            "previous_branch": choice.previous,
            "repository": place.repo.as_ref().map(|repo| repo.root().to_string_lossy()),
            "dry_run": args.dry_run,
            "created": names(Action::Create),
            "updated": names(Action::Update),
            "unchanged": names(Action::Unchanged),
            "paths": paths,
        }))?;
        return Ok(ExitCode::SUCCESS);
    }

    let mut out = io::stdout().lock();
    if let (Some(previous), true) = (&choice.previous, choice.missing) {
        let names = if args.dry_run { "would name" } else { "names" };
        writeln!(
            out,
            "The section named {previous}, which the repository does not have: it {names} {} now.",
            choice.branch
        )?;
    }
    for file in &planned {
        let said = if args.dry_run {
            &file.would
        } else {
            &file.done
        };
        writeln!(out, "{said}")?;
    }
    if place.repo.is_none() {
        writeln!(
            out,
            "There is no braid repository here yet: set braid up in the project root (`braid init`, then `braid git import .`) before agents start."
        )?;
    }
    if args.dry_run {
        writeln!(out, "Nothing was written.")?;
        if planned[0].action != Action::Unchanged {
            writeln!(out, "\nThe section {AGENTS_FILE} would hold:\n")?;
            write!(out, "{}", section(&choice.branch))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Where the command writes, and the repository and workspace that tells
/// it which branch to name.
struct Place {
    repo: Option<Repo>,
    /// The project's directory: where `AGENTS.md` is.
    dir: PathBuf,
    /// The branch of the workspace `dir` is, when it is one.
    workspace_branch: Option<String>,
}

impl Place {
    /// The directory to write in: the workspace `--workspace` names; else
    /// `--dir`; else the root of the repository or the workspace the
    /// current directory is in, if it belongs to the repository `--repo`
    /// names, if any; else the root of that repository; else the current
    /// directory.
    fn find(cli: &Cli, given: Option<&Path>) -> Result<Place> {
        if let Some(name) = &cli.workspace {
            if given.is_some() {
                bail!("--dir and --workspace both say where to write: give one of them");
            }
            let repo = open_repo(cli)?;
            let (dir, branch) = {
                let workspace = Workspace::open(&repo, name)?;
                (workspace.dir().to_path_buf(), workspace.state()?.branch)
            };
            if !dir.is_dir() {
                bail!(
                    "the directory of workspace {name}, {}, is gone",
                    dir.display()
                );
            }
            return Ok(Place {
                repo: Some(repo),
                dir,
                workspace_branch: Some(branch),
            });
        }
        let start = match given {
            Some(dir) => std::path::absolute(dir)?,
            None => std::env::current_dir()?,
        };
        if !start.is_dir() {
            bail!("{} is not a directory", start.display());
        }
        let location = match braid_repo::locate(&start) {
            Ok(location) => Some(location),
            Err(braid_repo::Error::NotFound) => None,
            Err(err) => return Err(err.into()),
        };
        let repo = match (&cli.repo, &location) {
            (Some(root), _) => Some(Repo::open_with(root, cli.store_options())?),
            (None, Some(Location::Root(dir))) => Some(Repo::open_with(dir, cli.store_options())?),
            (None, Some(Location::Workspace { link, .. })) => {
                Some(Repo::open_with(&link.repository, cli.store_options())?)
            }
            (None, None) => None,
        };
        // What the current directory is in counts only when it belongs to
        // the repository: --repo may name another.
        let belongs = |root: &Path| {
            cli.repo.is_none()
                || repo
                    .as_ref()
                    .is_some_and(|repo| same_dir(root, repo.root()))
        };
        let (top, workspace_branch) = match &location {
            Some(Location::Root(root)) if belongs(root) => (Some(root.clone()), None),
            Some(Location::Workspace { dir, link }) if belongs(&link.repository) => {
                // A workspace whose state is gone names no branch.
                let branch = repo.as_ref().and_then(|repo| {
                    Workspace::open(repo, &link.name)
                        .and_then(|workspace| workspace.state())
                        .ok()
                        .map(|state| state.branch)
                });
                (Some(dir.clone()), branch)
            }
            _ => (repo.as_ref().map(|repo| repo.root().to_path_buf()), None),
        };
        let dir = match (given, top) {
            (None, Some(top)) => top,
            _ => start,
        };
        Ok(Place {
            repo,
            dir,
            workspace_branch,
        })
    }
}

/// Whether two paths name the same directory.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The branch the section names, and what it named before.
#[derive(Debug)]
struct Choice {
    branch: String,
    /// The branch the section named before, when that was another.
    previous: Option<String>,
    /// Whether it changes because the repository does not have the branch
    /// the section named.
    missing: bool,
}

/// The branch the section names: the one asked for, which the repository
/// must have; the one the section names already, if the repository has
/// it; the workspace's; or the repository's main one. A branch the section
/// names but the repository does not have was written before the
/// project's history came in, or names a branch since removed: it is
/// chosen again rather than kept.
fn choose_branch(given: Option<&str>, named: Option<String>, place: &Place) -> Result<Choice> {
    let refs = place
        .repo
        .as_ref()
        .map(|repo| repo.store().refs())
        .transpose()?;
    let names: Option<Vec<&str>> = refs.as_ref().map(branches);
    // Whether the repository could have the branch: it has it, it has no
    // branch yet, or there is no repository to ask.
    let possible = |branch: &str| {
        names
            .as_ref()
            .is_none_or(|names| names.is_empty() || names.contains(&branch))
    };
    if let Some(given) = given {
        check_branch(given)?;
        if !possible(given) {
            bail!(
                "the repository has no branch {given:?}; it has {}. Pass --branch the branch agents land on",
                listed(names.as_deref().unwrap_or_default())
            );
        }
        return Ok(Choice {
            branch: given.to_owned(),
            previous: named.filter(|named| named != given),
            missing: false,
        });
    }
    if let Some(named) = named {
        if possible(&named) {
            return Ok(Choice {
                branch: named,
                previous: None,
                missing: false,
            });
        }
        let branch = main_branch(names.as_deref().unwrap_or_default())?;
        return Ok(Choice {
            branch,
            previous: Some(named),
            missing: true,
        });
    }
    let branch = match (&place.workspace_branch, names.as_deref()) {
        (Some(tracked), _) => {
            check_branch(tracked)?;
            tracked.clone()
        }
        (None, None) => bail!(
            "there is no braid repository here, so there is no branch to name yet: set braid up first (`braid init`, then `braid git import .`), or pass --branch"
        ),
        (None, Some([])) => bail!(
            "the braid repository has no branch yet: import the project's history first (`braid git import .`), or pass --branch"
        ),
        (None, Some(names)) => main_branch(names)?,
    };
    Ok(Choice {
        branch,
        previous: None,
        missing: false,
    })
}

/// The repository's main branch among its branches `names`: `main`,
/// `master`, or its only branch.
pub(crate) fn main_branch(names: &[&str]) -> Result<String> {
    if let Some(preferred) = ["main", "master"]
        .into_iter()
        .find(|preferred| names.contains(preferred))
    {
        return Ok(preferred.to_owned());
    }
    match names {
        [only] => {
            check_branch(only)?;
            Ok((*only).to_owned())
        }
        [] => bail!("the repository has no branch yet: pass --branch"),
        several => bail!(
            "cannot tell which branch agents land on: the repository has {}; pass --branch",
            listed(several)
        ),
    }
}

/// Whether the section can name `branch`. The section writes it into shell
/// commands and reads it back from its first comment, so it takes only a
/// set of characters that needs no quoting and holds no `)`: ASCII letters,
/// digits, `.`, `_`, `/` and `-`, not first. It must be a git branch name
/// outside braid's own namespaces, too.
pub(crate) fn is_section_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && branch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
        && braid_git::is_branch_name(branch)
        && braid_repo::is_branch(branch)
}

fn check_branch(branch: &str) -> Result<()> {
    if is_section_branch(branch) {
        Ok(())
    } else {
        bail!(
            "{branch:?} cannot name the branch agents land on: the section writes it into shell commands, so it takes a git branch name of ASCII letters, digits, '.', '_', '/' and '-', not starting with '-' and outside braid's own refs (braid/, git/, remote/, tags/)"
        )
    }
}

/// What `AGENTS.md` needs: to be created, to get the section, or to have
/// it rewritten in place.
fn plan_agents(
    target: Target,
    current: Option<&str>,
    scanned: &Scanned,
    branch: &str,
) -> Result<Planned> {
    let landing = format!("agents land on {branch}");
    let called = target.called(AGENTS_FILE);
    let (action, contents, done, would) = match (current, &scanned.section) {
        (None, _) => (
            Action::Create,
            Some(section(branch)),
            format!("Created {called} with the braid section: {landing}."),
            format!("Would create {called} with the braid section: {landing}."),
        ),
        (Some(text), Some(range)) => {
            let new = replace_section(text, range.clone(), &section(branch));
            if new == text {
                let said = format!("{called} has this braid section already: {landing}.");
                (Action::Unchanged, None, said.clone(), said)
            } else {
                (
                    Action::Update,
                    Some(new),
                    format!("Updated the braid section of {called}: {landing}."),
                    format!("Would update the braid section of {called}: {landing}."),
                )
            }
        }
        (Some(_), None) if scanned.open_fence.is_some() => bail!(
            "{} ends inside a fenced code block, which opens on line {} and is never closed: the section would be read as code there. Close the block, then run the command again: {NOTHING}",
            target.shown,
            scanned.open_fence.unwrap_or_default()
        ),
        (Some(text), None) => (
            Action::Update,
            Some(append_section(text, &section(branch))),
            format!("Added the braid section to {called}: {landing}."),
            format!("Would add the braid section to {called}: {landing}."),
        ),
    };
    Ok(Planned {
        name: AGENTS_FILE,
        target,
        action,
        contents,
        done,
        would,
    })
}

/// What `name`, `CLAUDE.md` or `GEMINI.md`, needs, or `None` when there is
/// no such file. `earlier` are the files planned before it, `AGENTS.md`
/// first: a file that is one of them under another name needs nothing.
fn plan_include(
    dir: &Path,
    name: &'static str,
    roots: &[PathBuf],
    guides: &[(PathBuf, FileId)],
    earlier: &[Planned],
) -> Result<Option<Planned>> {
    let planned = |target, action, contents, said: String, would: String| Planned {
        name,
        target,
        action,
        contents,
        done: said,
        would,
    };
    let same = |other: &str| {
        let said = if other == AGENTS_FILE {
            format!("{name} is {AGENTS_FILE} under another name: it holds the section itself.")
        } else {
            format!("{name} is {other} under another name.")
        };
        (said.clone(), said)
    };
    let target = match locate_file(dir, name, roots, guides)? {
        Located::Missing => return Ok(None),
        Located::Dangling(to) => {
            // A link to the AGENTS.md this run creates holds the section;
            // a link to anything else that is missing has nothing to add
            // the line to.
            let agents = &earlier[0];
            let to_agents = to == Path::new(AGENTS_FILE)
                || to == Path::new("./AGENTS.md")
                || dir.join(&to) == agents.target.path;
            if !to_agents || agents.action != Action::Create {
                return Ok(None);
            }
            let (said, would) = same(AGENTS_FILE);
            let target = Target {
                shown: agents.target.shown.clone(),
                ..Target::new_file(dir, name)
            };
            return Ok(Some(planned(target, Action::Unchanged, None, said, would)));
        }
        Located::File(target) => target,
    };
    if let Some(other) = earlier
        .iter()
        .find(|file| file.target.id.is_some() && file.target.id == target.id)
    {
        let (said, would) = same(other.name);
        return Ok(Some(planned(target, Action::Unchanged, None, said, would)));
    }
    let text = target.read()?.unwrap_or_default();
    if includes_agents(&text) {
        let said = format!("{} includes {AGENTS_FILE} already.", target.called(name));
        return Ok(Some(planned(
            target,
            Action::Unchanged,
            None,
            said.clone(),
            said,
        )));
    }
    if let Some(line) = open_fence(&text) {
        bail!(
            "{} ends inside a fenced code block, which opens on line {line} and is never closed: the line {INCLUDE} would be read as code there. Close the block, then run the command again: {NOTHING}",
            target.shown
        );
    }
    let called = target.called(name);
    Ok(Some(planned(
        target,
        Action::Update,
        Some(add_include(&text)),
        format!("Added {INCLUDE} to {called}."),
        format!("Would add {INCLUDE} to {called}."),
    )))
}

/// The braid section for agents that land on `branch`, from its start
/// marker to its end marker, each line ending in `\n`.
pub(crate) fn section(branch: &str) -> String {
    format!(
        "{START}
<!-- Written by `braid agents init` {BRANCH_NOTE}{branch}). Running it
     again rewrites everything between the braid markers. -->
## Version control with braid

Agents land their changes on `{branch}` through [braid](https://braid.elffuss.com),
each in a braid workspace of its own. Git stays the source of truth, and
`braid git sync` keeps the git remote in step.

- **Only where braid is set up:** in a checkout whose root has a `.braid`
  directory. Without one (a fresh clone, CI, another machine), skip this
  section and work with git, as the project's other rules say; set braid
  up only if the user asks for it (<https://braid.elffuss.com/agents.md>
  says how). With `.braid` but no `braid` command, braid is not on `PATH`:
  try `\"$HOME/.cargo/bin/braid\"`, or ask the user to install it.
- **Never edit files, `git add`, `git commit` or `git push` in the project
  root.** It is the shared checkout, where the user works: work only in
  your workspace.
- **Start** with a workspace named for your session: 1 to 64 ASCII
  letters, digits, `.`, `_` or `-`, starting with a letter or a digit
  (`braid workspace list` shows the names taken). The commands here call it
  `agent-1`: use your own name. From the project root, this creates it in
  `.braid/work/agent-1`:

  ```bash
  braid workspace add agent-1 --track {branch}
  cd .braid/work/agent-1
  ```

- **Work** in `.braid/work/agent-1`: edit, build and test there. Record and
  land from the workspace, naming your session with `--agent` every time:

  ```bash
  braid --agent agent-1 status --json
  braid --agent agent-1 commit -m \"Say what changed and why\"
  braid --agent agent-1 land --json
  ```

  If your shell does not stay in the workspace between commands, add
  `--workspace agent-1` and run them from the project root.
- **No git in the workspace.** Its `.git` file points nowhere, so `git`
  fails there. Never get around that (`git -C`, `GIT_DIR`): `git stash`,
  `reset`, `checkout`, `restore`, `clean`, `add` or `commit` would act on
  the user's checkout. A build or test script that finds the project with
  git fails in the workspace too: run its steps yourself. To review an
  edit, from the workspace's root, compare the file with the workspace's
  `base`, which `status --json` gives:
  `braid cat-object <base>:<path> | diff -u - <path>`. To undo it, write
  the base's version back with `braid cat-object <base>:<path> > <path>`,
  or delete a file you added.
- **Before each commit**, read `braid status`: it lists what `commit` would
  record, and what would stop it. There is no staging area: `commit`
  records every change in the workspace, or only the paths it is given
  (`commit -m \"...\" <path>...`), and `land` refuses while anything is left
  uncommitted. Building or running the project writes files too: those
  `.gitignore` does not list are new files like any other. Delete them, or
  list them in `.braidignore`, which must then list itself too, or it is a
  new file to commit: for Python's `__pycache__/`, run
  `printf '%s\\n' .braidignore __pycache__/ >> .braidignore` in the
  workspace's root. Only `.gitignore` and `.braidignore` apply in a
  workspace, not `.git/info/exclude` or a global excludes file. Braid runs
  no git hooks: before you land, run the project's checks yourself, as
  this file or `CLAUDE.md` gives them.
- **Conflicts:** `land` and `update` exit with status 2, the `outcome` in
  their JSON is `conflicts`, and `{branch}` does not move. After `land`, run
  `braid --agent agent-1 update`, which writes conflict markers into the
  files; fix them (`braid resolve <path>` settles conflicts that are not
  about content), then `commit`, which keeps the change's message unless
  `-m` gives another, and `land` again. In the JSON, `next` says what to
  run. Never `commit --force` during a rebase: it would commit the markers.
  Any other failure exits with status 1, and braid prints it as text on
  standard error, even with `--json`.
- **Before a new task**, `braid --agent agent-1 update` brings your
  workspace onto `{branch}`.
- **On a hub:** when `braid hub status` names a hub (the repository was
  published on [BraidHub](https://braidhub.elffuss.com) or cloned from it;
  elsewhere it says the repository is not linked), `land` and `update` go
  through the hub, where agents on other machines land too, and answer as
  they do here. When it names a mirror of `{branch}` on GitHub, the hub
  sends what lands there itself: skip the `git sync` below, and the user's
  checkout gets it with `git pull --ff-only`. Without the network,
  `land --local` and `update --local` work in this repository only. New
  projects you start for the user go on BraidHub only as
  <https://braidhub.elffuss.com/agents.md> says.
- **When you finish**, tell the user that what you landed is on braid's
  `{branch}` only: their checkout and the git remote get it once someone
  runs this from the project root.

  ```bash
  braid --agent agent-1 git sync \"$(git remote get-url origin)\" --branch {branch}
  git pull --ff-only
  ```

  Offer to run it: when to sync is the user's call. If the sync exits
  with 2 (`conflicts`), braid and the remote changed the same lines, and
  nothing moved: tell the user, since the merge is made with git, in their
  checkout, as <https://braid.elffuss.com/agents.md> shows under
  \"Keeping the git remote up to date\". With `remote_moved` (exit status
  1), sync again; with `rejected`, ask the user. If `git pull --ff-only`
  refuses, their checkout has commits of its own, and what to do is their
  call: `git pull --rebase`, `git push` and a new sync bring them into
  braid. Where `git remote get-url origin` fails, the project has no
  remote of that name: if `git remote` lists others, ask the user which to
  sync with, and write its name in place of `origin`; if it lists none,
  say so, and offer to make one next to the project first, from the
  project root:

  ```bash
  git init --bare -b {branch} \"$PWD.git\"
  git remote add origin \"$PWD.git\"
  git push -u origin {branch}
  ```

  Once everything has landed, `braid workspace remove agent-1`, from the
  project root, removes your workspace.
- Changesets are signed by `BRAID_AUTHOR` (`\"Name <email>\"`), or else by the
  `name` and `email` in `.braid/config`. With neither, `commit`, `land` and
  `update` fail with \"no author\", and `status` lists it first among what
  stops a commit: ask the user who signs, and write the answer into
  `.braid/config` in the project root, the one file there you may edit, as
  `name = <name>` and `email = <email>` lines. The merges that `git sync`
  makes are signed by its `--agent`, with that email.
- New files that look like secrets (`.env*`, `*.pem`, `id_*`) or are larger
  than 1 MiB wait for `braid track <path>`. Never track a secret: move it
  out of the workspace.
- `braid --agent agent-1 undo` reverts the newest operation in the
  repository, whoever made it: read `braid op log` first.
{END}
"
    )
}

/// Whether `line` is `marker`: alone on its line, indented by three spaces
/// at most, as Markdown lets an HTML comment be.
fn is_marker(line: &str, marker: &str) -> bool {
    let line = line.trim_end();
    let text = line.trim_start_matches(' ');
    line.len() - text.len() <= 3 && text == marker
}

/// The fence a line opens or closes, as CommonMark reads one: indented by
/// three spaces at most, then three or more backticks or tildes, with no
/// backtick after backticks. Gives the character, how many of it there
/// are, and the rest of the line.
fn fence(line: &str) -> Option<(u8, usize, &str)> {
    let line = line.trim_end_matches(['\n', '\r']);
    let text = line.trim_start_matches(' ');
    if line.len() - text.len() > 3 {
        return None;
    }
    let mark = *text.as_bytes().first()?;
    if mark != b'`' && mark != b'~' {
        return None;
    }
    let count = text.bytes().take_while(|byte| *byte == mark).count();
    let rest = &text[count..];
    (count >= 3 && !(mark == b'`' && rest.contains('`'))).then_some((mark, count, rest))
}

/// Follows the fenced code blocks of a Markdown text, a line at a time.
#[derive(Debug, Default)]
struct Fences {
    /// The block open after the last line: its fence's character and
    /// length, and the line it opened on.
    open: Option<(u8, usize, usize)>,
}

impl Fences {
    /// Whether line number `number` is code: a fence, or inside a block.
    fn code(&mut self, line: &str, number: usize) -> bool {
        match (self.open, fence(line)) {
            (Some((mark, count, _)), Some((found, length, rest)))
                if found == mark && length >= count && rest.trim().is_empty() =>
            {
                self.open = None;
                true
            }
            (Some(_), _) => true,
            (None, Some((mark, count, _))) => {
                self.open = Some((mark, count, number));
                true
            }
            (None, None) => false,
        }
    }
}

/// `line`, the first of a text, without the byte order mark an editor may
/// have put before it.
fn without_bom(line: &str) -> &str {
    line.strip_prefix('\u{feff}').unwrap_or(line)
}

/// The line a fenced code block that `text` never closes opens on.
fn open_fence(text: &str) -> Option<usize> {
    let mut fences = Fences::default();
    for (index, line) in text.lines().enumerate() {
        let line = if index == 0 { without_bom(line) } else { line };
        fences.code(line, index + 1);
    }
    fences.open.map(|(_, _, line)| line)
}

/// What a Markdown text holds of the braid section.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Scanned {
    /// The section: from the start of its start marker's line to the end of
    /// its end marker's line.
    pub(crate) section: Option<Range<usize>>,
    /// The line a fenced code block that the text never closes opens on.
    pub(crate) open_fence: Option<usize>,
}

/// Where the braid section is in `text`, with the markers inside fenced
/// code blocks left out, as examples rather than markers. An error that
/// says where they are when they do not pair up.
pub(crate) fn scan_section(text: &str) -> Result<Scanned, String> {
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    let mut fences = Fences::default();
    let mut offset = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        // A byte order mark is not part of the first line: the section
        // starts after it, so that it stays.
        let skip = if index == 0 {
            line.len() - without_bom(line).len()
        } else {
            0
        };
        let body = &line[skip..];
        if !fences.code(body, index + 1) {
            if is_marker(body, START) {
                starts.push((index + 1, offset + skip));
            } else if is_marker(body, END) {
                ends.push((index + 1, offset + line.len()));
            }
        }
        offset += line.len();
    }
    let section = match (starts.as_slice(), ends.as_slice()) {
        ([], []) => None,
        ([(first, from)], [(last, to)]) if first < last => Some(*from..*to),
        _ => {
            return Err(format!(
                "`{START}` is on {}, `{END}` on {}, and there must be one of each, the start first",
                line_list(&starts),
                line_list(&ends)
            ));
        }
    };
    Ok(Scanned {
        section,
        open_fence: fences.open.map(|(_, _, line)| line),
    })
}

/// `no line`, `line 3`, `lines 3 and 9`, `lines 3, 9 and 12`.
fn line_list(found: &[(usize, usize)]) -> String {
    let numbers: Vec<String> = found.iter().map(|(line, _)| line.to_string()).collect();
    match numbers.as_slice() {
        [] => "no line".to_owned(),
        [one] => format!("line {one}"),
        [rest @ .., last] => format!("lines {} and {last}", rest.join(", ")),
    }
}

/// The line end `text` uses: `\r\n` if it has any, otherwise `\n`.
fn line_end(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// `section`, written with the line ends of `text`.
fn in_line_ends_of(text: &str, section: &str) -> String {
    match line_end(text) {
        "\n" => section.to_owned(),
        end => section.replace('\n', end),
    }
}

/// `text` with the section at `range` replaced by `section`.
pub(crate) fn replace_section(text: &str, range: Range<usize>, section: &str) -> String {
    let section = in_line_ends_of(text, section);
    let mut out = String::with_capacity(text.len() - range.len() + section.len());
    out.push_str(&text[..range.start]);
    out.push_str(&section);
    out.push_str(&text[range.end..]);
    out
}

/// `text` with `section` at its end, after a blank line.
pub(crate) fn append_section(text: &str, section: &str) -> String {
    let end = line_end(text);
    let section = in_line_ends_of(text, section);
    let mut out = String::with_capacity(text.len() + section.len() + 2 * end.len());
    out.push_str(text);
    if !without_bom(text).is_empty() {
        if !out.ends_with('\n') {
            out.push_str(end);
        }
        if !without_bom(&out).trim_end_matches(['\n', '\r']).is_empty()
            && !ends_with_blank_line(&out)
        {
            out.push_str(end);
        }
    }
    out.push_str(&section);
    out
}

/// Whether the last line of `text`, which ends in a line end, is blank.
fn ends_with_blank_line(text: &str) -> bool {
    let body = text
        .strip_suffix('\n')
        .map(|body| body.strip_suffix('\r').unwrap_or(body))
        .unwrap_or(text);
    body.is_empty() || body.ends_with('\n')
}

/// The branch a section was written for, as its first comment says, if
/// the section can name it.
pub(crate) fn named_branch(section: &str) -> Option<String> {
    let line = section.lines().find(|line| line.contains(BRANCH_NOTE))?;
    let (_, rest) = line.split_once(BRANCH_NOTE)?;
    let (name, _) = rest.split_once(')')?;
    is_section_branch(name).then(|| name.to_owned())
}

/// `line` without its code spans: the text between two runs of the same
/// number of backticks. A run that nothing closes stays, as text.
fn without_code_spans(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find('`') {
        out.push_str(&rest[..at]);
        let run = rest[at..].bytes().take_while(|byte| *byte == b'`').count();
        let after = &rest[at + run..];
        match closing_run(after, run) {
            Some(end) => {
                out.push(' ');
                rest = &after[end..];
            }
            None => {
                out.push_str(&rest[at..at + run]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Where the first run of exactly `run` backticks in `text` ends.
fn closing_run(text: &str, run: usize) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = text[from..].find('`') {
        let start = from + at;
        let length = text[start..]
            .bytes()
            .take_while(|byte| *byte == b'`')
            .count();
        if length == run {
            return Some(start + length);
        }
        from = start + length;
    }
    None
}

/// Whether a `CLAUDE.md` or `GEMINI.md` includes `AGENTS.md`: a word
/// `@AGENTS.md` or `@./AGENTS.md` outside code, as Claude Code reads its
/// imports. Neither a code span nor a fenced code block imports anything.
pub(crate) fn includes_agents(text: &str) -> bool {
    let mut fences = Fences::default();
    text.lines().enumerate().any(|(index, line)| {
        let line = if index == 0 { without_bom(line) } else { line };
        !fences.code(line, index + 1)
            && without_code_spans(line).split_whitespace().any(|word| {
                let word = word
                    .trim_start_matches('(')
                    .trim_end_matches([',', ';', ':', ')']);
                word == INCLUDE || word == "@./AGENTS.md"
            })
    })
}

/// `text` with `@AGENTS.md` as its last line.
pub(crate) fn add_include(text: &str) -> String {
    let end = line_end(text);
    let mut out = text.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(end);
    }
    out.push_str(INCLUDE);
    out.push_str(end);
    out
}

/// What tells two names of one file apart from two files: the device and
/// the inode where there are hard links, the canonical path elsewhere.
#[cfg(unix)]
type FileId = (u64, u64);
#[cfg(not(unix))]
type FileId = PathBuf;

#[cfg(unix)]
fn file_id(meta: &fs::Metadata, _path: &Path) -> FileId {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn file_id(_meta: &fs::Metadata, path: &Path) -> FileId {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// How many names the file has.
#[cfg(unix)]
fn names_of(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(not(unix))]
fn names_of(_meta: &fs::Metadata) -> u64 {
    1
}

/// The guides of `dir` that are regular files, not symbolic links, with
/// what tells them apart: the names a file with hard links may have here.
fn guide_files(dir: &Path) -> Vec<(PathBuf, FileId)> {
    [AGENTS_FILE, CLAUDE_FILE, GEMINI_FILE]
        .iter()
        .filter_map(|name| {
            let path = dir.join(name);
            let meta = fs::symlink_metadata(&path).ok()?;
            meta.is_file().then(|| {
                let id = file_id(&meta, &path);
                (path, id)
            })
        })
        .collect()
}

/// The file a guide is, once checked: what is read and written.
#[derive(Clone, Debug)]
struct Target {
    /// Where the text is: the name itself, or the file its link points to.
    path: PathBuf,
    /// That file as the output names it: its path from the project's
    /// directory, or the whole path when it is elsewhere in the repository.
    shown: String,
    /// Whether the file exists.
    exists: bool,
    /// Whether it is written in place rather than replaced, since it has
    /// hard links, all of them guides of the project.
    in_place: bool,
    /// What tells it from other files, when it exists.
    id: Option<FileId>,
}

impl Target {
    /// A file that does not exist yet: `name` in `dir`.
    fn new_file(dir: &Path, name: &str) -> Target {
        Target {
            path: dir.join(name),
            shown: name.to_owned(),
            exists: false,
            in_place: false,
            id: None,
        }
    }

    /// How the output names the file known as `name`: by that name, and by
    /// the file its link points to when that is another.
    fn called(&self, name: &str) -> String {
        if self.shown == name {
            name.to_owned()
        } else {
            format!("{name} ({}, which it links to)", self.shown)
        }
    }

    /// The file's text, or `None` when it does not exist.
    fn read(&self) -> Result<Option<String>> {
        if !self.exists {
            return Ok(None);
        }
        let bytes =
            fs::read(&self.path).with_context(|| format!("cannot read {}", self.path.display()))?;
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| anyhow!("{} is not UTF-8 text: {NOTHING}", self.shown))
    }
}

/// What a guide's name leads to.
enum Located {
    /// Nothing has the name.
    Missing,
    /// A symbolic link to a file that does not exist; where it points.
    Dangling(PathBuf),
    /// A file braid may write.
    File(Target),
}

/// Finds the file `name` of `dir` is, and checks that braid may write it:
/// a regular file, or a symbolic link to one inside `roots`, the project's
/// directory and the repository's root; not read-only; and with no hard
/// link but the guides of `dir`. Every check runs before anything is read,
/// with or without `--dry-run`, so that a dry run fails as the real run
/// would, and a FIFO is never opened.
fn locate_file(
    dir: &Path,
    name: &'static str,
    roots: &[PathBuf],
    guides: &[(PathBuf, FileId)],
) -> Result<Located> {
    let path = dir.join(name);
    let own = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Located::Missing),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    let linked = own.file_type().is_symlink();
    let (path, meta) = if linked {
        let target = match fs::canonicalize(&path) {
            Ok(target) => target,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(Located::Dangling(fs::read_link(&path).unwrap_or_default()));
            }
            Err(err) => {
                return Err(err).with_context(|| format!("cannot follow {}", path.display()));
            }
        };
        let meta =
            fs::metadata(&target).with_context(|| format!("cannot read {}", target.display()))?;
        (target, meta)
    } else {
        (path, own)
    };
    let shown = if linked {
        match path.strip_prefix(&roots[0]) {
            Ok(inside) => inside.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    } else {
        name.to_owned()
    };
    if !meta.is_file() {
        let what = if linked {
            format!("{name}, a link to {shown},")
        } else {
            name.to_owned()
        };
        bail!("{what} is not a regular file: {NOTHING}");
    }
    if linked && !roots.iter().any(|root| path.starts_with(root)) {
        bail!(
            "{name} is a link to {}, outside the project: other projects may read that file too. Replace the link with a copy of the file, or point it inside the project: {NOTHING}",
            path.display()
        );
    }
    if meta.permissions().readonly() {
        bail!("{shown} is read-only: make it writable, then run the command again: {NOTHING}");
    }
    let id = file_id(&meta, &path);
    let mut known = vec![path.clone()];
    for (other, other_id) in guides {
        if *other_id == id && !known.contains(other) {
            known.push(other.clone());
        }
    }
    let names = names_of(&meta);
    if names > known.len() as u64 {
        bail!(
            "{shown} has hard links besides the {AGENTS_FILE}, {CLAUDE_FILE} and {GEMINI_FILE} here: writing it would change the file under those names too. Replace it with a copy, then run the command again: {NOTHING}"
        );
    }
    Ok(Located::File(Target {
        path,
        shown,
        exists: true,
        in_place: names > 1,
        id: Some(id),
    }))
}

/// Writes every planned file that changes. Each is first written whole to
/// a temporary file next to it, so that a failure there leaves every file
/// as it was; then each replaces its file with a rename, so that no file
/// is ever seen half written. A file with hard links is written in place,
/// last. A failure after the first file is replaced says which were.
fn write_all(planned: &[Planned]) -> Result<()> {
    let changing: Vec<(&Planned, &str)> = planned
        .iter()
        .filter_map(|file| file.contents.as_deref().map(|contents| (file, contents)))
        .collect();
    let mut staged: Vec<(&Planned, PathBuf)> = Vec::new();
    for (file, contents) in changing.iter().filter(|(file, _)| !file.target.in_place) {
        match stage(&file.target.path, contents) {
            Ok(temp) => staged.push((file, temp)),
            Err(err) => {
                for (_, temp) in &staged {
                    let _ = fs::remove_file(temp);
                }
                return Err(failed(file, &err, &[]));
            }
        }
    }
    let mut written: Vec<&str> = Vec::new();
    let mut renames = staged.into_iter();
    while let Some((file, temp)) = renames.next() {
        if let Err(err) = fs::rename(&temp, &file.target.path) {
            let _ = fs::remove_file(&temp);
            for (_, rest) in renames {
                let _ = fs::remove_file(rest);
            }
            return Err(failed(file, &err, &written));
        }
        written.push(&file.target.shown);
    }
    for (file, contents) in changing.iter().filter(|(file, _)| file.target.in_place) {
        if let Err(err) = write_in_place(&file.target.path, contents) {
            return Err(failed(file, &err, &written));
        }
        written.push(&file.target.shown);
    }
    Ok(())
}

/// The error for `file`, which could not be written, saying which files
/// were.
fn failed(file: &Planned, err: &io::Error, written: &[&str]) -> anyhow::Error {
    let shown = &file.target.shown;
    let done = match written {
        [] => NOTHING.to_owned(),
        [one] => format!("{one} was written, {shown} was not"),
        several => format!("{} were written, {shown} was not", listed(several)),
    };
    anyhow!("cannot write {shown}: {err}; {done}")
}

/// Writes `contents` to a new temporary file next to `path`, with the
/// permissions `path` has, and returns its path once it is on disk.
fn stage(path: &Path, contents: &str) -> io::Result<PathBuf> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::other("it has no directory"));
    };
    let name = name.to_string_lossy();
    let mut attempt = 0u32;
    let (temp, mut file) = loop {
        let temp = parent.join(format!(".{name}.braid-{}-{attempt}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => break (temp, file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists && attempt < 64 => {
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    };
    let mut written = || -> io::Result<()> {
        if let Ok(meta) = fs::metadata(path) {
            fs::set_permissions(&temp, meta.permissions())?;
        }
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    };
    match written() {
        Ok(()) => Ok(temp),
        Err(err) => {
            let _ = fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Writes `contents` over the file at `path`, keeping the file and so
/// every name it has.
fn write_in_place(path: &Path, contents: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABOVE: &str = "# Project\n\nRules of our own.\n\n";
    const BELOW: &str = "\n## More rules\n\nKept as they are.\n";

    fn find(text: &str) -> Result<Option<Range<usize>>, String> {
        scan_section(text).map(|scanned| scanned.section)
    }

    #[test]
    fn the_section_names_the_branch_and_holds_no_marker_inside() {
        let text = section("trunk");
        assert!(text.starts_with(&format!("{START}\n")));
        assert!(text.ends_with(&format!("\n{END}\n")));
        assert!(text.contains("--track trunk\n"));
        assert!(text.contains("land their changes on `trunk`"));
        assert!(text.contains("--branch trunk\n"));
        assert!(
            !text.contains("`main`") && !text.contains(" main"),
            "{text}"
        );
        assert_eq!(named_branch(&text).as_deref(), Some("trunk"));
        // The markers appear once each, on their own lines, and the
        // section's own code blocks are closed.
        assert_eq!(
            scan_section(&text),
            Ok(Scanned {
                section: Some(0..text.len()),
                open_fence: None
            })
        );
        assert!(text.lines().all(|line| line.len() <= 100), "{text}");
    }

    #[test]
    fn branches_the_section_can_name_read_back_exactly() {
        for good in [
            "main",
            "master",
            "trunk",
            "release-1.2",
            "feature/x.y_z-1",
            "v1.x/beta",
            "UPPER_case",
        ] {
            assert!(is_section_branch(good), "{good}");
            assert_eq!(named_branch(&section(good)).as_deref(), Some(good));
        }
        for bad in [
            "",
            "feat(x)",
            "a;b",
            "a;touch${IFS}pwned",
            "q$(id)",
            "x`id`",
            "it's",
            "a|b",
            "a>b",
            "a b",
            "-x",
            "--no-claude",
            "a..b",
            "x.lock",
            "tags/v1",
            "braid/git-map",
            "rama-ñ",
        ] {
            assert!(!is_section_branch(bad), "{bad}");
        }
        // A section an older braid wrote for a name it cannot hold names
        // nothing, rather than a part of it.
        let old =
            format!("{START}\n<!-- Written by `braid agents init` (branch: feat(x)). -->\n{END}\n");
        assert_eq!(named_branch(&old), None);
    }

    #[test]
    fn a_section_is_appended_after_a_blank_line() {
        let new = section("main");
        assert_eq!(append_section("", &new), new);
        assert_eq!(append_section("# Rules", &new), format!("# Rules\n\n{new}"));
        assert_eq!(
            append_section("# Rules\n", &new),
            format!("# Rules\n\n{new}")
        );
        assert_eq!(
            append_section("# Rules\n\n", &new),
            format!("# Rules\n\n{new}")
        );
        // Line ends follow the file's.
        let crlf = append_section("# Rules\r\n", &new);
        assert_eq!(
            crlf,
            format!("# Rules\r\n\r\n{}", new.replace('\n', "\r\n"))
        );
        let range = find(&crlf).unwrap().unwrap();
        assert_eq!(replace_section(&crlf, range, &new), crlf);
    }

    #[test]
    fn only_the_text_between_the_markers_changes() {
        let old = format!("{START}\nan older section\n{END}\n");
        let text = format!("{ABOVE}{old}{BELOW}");
        let range = find(&text).unwrap().unwrap();
        assert_eq!(&text[range.clone()], old);
        let new = replace_section(&text, range, &section("main"));
        assert_eq!(new, format!("{ABOVE}{}{BELOW}", section("main")));
        // Again: nothing changes.
        let range = find(&new).unwrap().unwrap();
        assert_eq!(replace_section(&new, range, &section("main")), new);
        // The older section named no branch.
        assert_eq!(named_branch(&old), None);
    }

    #[test]
    fn markers_may_be_indented_a_little_but_must_be_alone() {
        let text = format!("   {START}  \nx\n{END}\n");
        assert_eq!(find(&text), Ok(Some(0..text.len())));
        for not_markers in [
            format!("    {START}\n{END}\n"),
            format!("see `{START}`\n{END}\n"),
            format!("{START} and more\n{END}\n"),
        ] {
            assert!(find(&not_markers).is_err(), "{not_markers:?}");
        }
        assert_eq!(find("no markers at all\n"), Ok(None));
    }

    #[test]
    fn markers_in_fenced_code_blocks_are_examples() {
        let example = format!("```markdown\n{START}\n(the rules braid writes)\n{END}\n```\n");
        let text = format!("# Rules\n\n{example}");
        assert_eq!(
            scan_section(&text),
            Ok(Scanned {
                section: None,
                open_fence: None
            })
        );
        // Next to a real section, the example is left alone.
        let real = section("main");
        let both = format!("{text}\n{real}");
        let range = find(&both).unwrap().unwrap();
        assert_eq!(&both[range], real);
        // Tildes, longer fences, and a fence that only a fence as long
        // closes.
        for fenced in [
            format!("~~~\n{START}\n~~~\n"),
            format!("````text\n```\n{END}\n````\n"),
            format!("  ```\n{START}\n{END}\n  ```\n"),
        ] {
            assert_eq!(find(&fenced), Ok(None), "{fenced:?}");
        }
        // Four spaces make no fence, and backticks in a backtick fence's
        // info string make none either.
        assert!(find(&format!("    ```\n{START}\n")).is_err());
        assert!(find(&format!("``` a`b\n{START}\n")).is_err());
        // A block that is never closed hides what follows it, and says
        // where it opens.
        let open = format!("# Rules\n\n```bash\nmake test\n{START}\n{END}\n");
        assert_eq!(
            scan_section(&open),
            Ok(Scanned {
                section: None,
                open_fence: Some(3)
            })
        );
        assert_eq!(open_fence(&open), Some(3));
        assert_eq!(open_fence(&text), None);
    }

    #[test]
    fn a_byte_order_mark_before_the_start_marker_stays() {
        let section = section("main");
        let text = format!("\u{feff}{section}");
        let range = find(&text).unwrap().unwrap();
        assert_eq!(range, '\u{feff}'.len_utf8()..text.len());
        assert_eq!(replace_section(&text, range, &section), text);
        assert!(includes_agents("\u{feff}@AGENTS.md\n"));
        assert_eq!(
            append_section("\u{feff}", &section),
            format!("\u{feff}{section}")
        );
        assert_eq!(
            append_section("\u{feff}# Rules\n", &section),
            format!("\u{feff}# Rules\n\n{section}")
        );
    }

    #[test]
    fn unbalanced_markers_are_refused_with_their_lines() {
        let cases = [
            (
                format!("a\n{START}\nb\n"),
                "on line 2, `<!-- braid:end -->` on no line",
            ),
            (
                format!("{END}\n"),
                "on no line, `<!-- braid:end -->` on line 1",
            ),
            (
                format!("{END}\n{START}\n"),
                "on line 2, `<!-- braid:end -->` on line 1",
            ),
            (
                format!("{START}\n{START}\n{END}\n"),
                "on lines 1 and 2, `<!-- braid:end -->` on line 3",
            ),
            (
                format!("{START}\n{END}\n{START}\n{END}\n{END}\n"),
                "on lines 1 and 3, `<!-- braid:end -->` on lines 2, 4 and 5",
            ),
        ];
        for (text, said) in cases {
            let err = find(&text).unwrap_err();
            assert!(err.contains(said), "{text:?}: {err}");
        }
    }

    #[test]
    fn claude_md_includes() {
        for yes in [
            "@AGENTS.md\n",
            "# Notes\nSee @AGENTS.md for the rules.\n",
            "(@./AGENTS.md)\n",
            "  @AGENTS.md  ",
            "```text\nan example\n```\n@AGENTS.md\n",
            "Read `this` and @AGENTS.md\n",
        ] {
            assert!(includes_agents(yes), "{yes:?}");
        }
        for no in [
            "",
            "Read AGENTS.md\n",
            "`@AGENTS.md`\n",
            "@AGENTS.md.bak\n",
            "@docs/AGENTS.md\n",
            "``see @AGENTS.md here``\n",
            "# Claude notes\n\n```text\n@AGENTS.md\n```\n",
            "~~~\n@AGENTS.md\n",
        ] {
            assert!(!includes_agents(no), "{no:?}");
        }
        assert_eq!(add_include(""), "@AGENTS.md\n");
        assert_eq!(add_include("# Claude"), "# Claude\n@AGENTS.md\n");
        assert_eq!(add_include("# Claude\n"), "# Claude\n@AGENTS.md\n");
        assert_eq!(add_include("# Claude\r\n"), "# Claude\r\n@AGENTS.md\r\n");
        assert!(includes_agents(&add_include("# Claude\n")));
    }

    #[test]
    fn code_spans_are_left_out() {
        assert_eq!(without_code_spans("a `b` c"), "a   c");
        assert_eq!(without_code_spans("a ``b ` c`` d"), "a   d");
        assert_eq!(without_code_spans("a ` b"), "a ` b");
        assert_eq!(without_code_spans("é `x` ü"), "é   ü");
        assert_eq!(without_code_spans("``a` b"), "``a` b");
    }
}
