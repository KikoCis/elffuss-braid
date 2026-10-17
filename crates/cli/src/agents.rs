// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid agents init`: the rules for coding agents, written into the
//! project's `AGENTS.md` between two marker lines, so that every agent
//! session, whatever runs it, knows to work in a braid workspace of its own
//! and to land through braid. A `CLAUDE.md` that does not include
//! `AGENTS.md` gets the line that does.
//!
//! Text outside the markers is never touched, and running the command again
//! rewrites the section in place, so it is idempotent.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use braid_repo::{Location, Repo};
use braid_wc::Workspace;
use clap::{Args, Subcommand};
use serde_json::json;

use crate::{Cli, print_json};

/// The line that opens the braid section of `AGENTS.md`.
pub(crate) const START: &str = "<!-- braid:start -->";

/// The line that closes it.
pub(crate) const END: &str = "<!-- braid:end -->";

/// The file the section goes in.
const AGENTS_FILE: &str = "AGENTS.md";

/// The file Claude Code reads, which should include `AGENTS.md`.
const CLAUDE_FILE: &str = "CLAUDE.md";

/// The line that makes `CLAUDE.md` include `AGENTS.md`.
const INCLUDE: &str = "@AGENTS.md";

/// What the section's first comment says just before the branch it names.
const BRANCH_NOTE: &str = "(branch: ";

/// The long help of `agents init`.
pub(crate) const INIT_HELP: &str =
    "Write the rules for coding agents into the project's AGENTS.md, so that
every agent session, whatever runs it, works in a braid workspace of its
own and lands through braid.

The rules go in a section between two lines, <!-- braid:start --> and
<!-- braid:end -->: at the end of AGENTS.md, which is created if there is
none, and in place on later runs. Text outside the markers is never
touched, and running the command twice changes nothing. Markers that do not
pair up (one without the other, the end first, or either of them twice)
are refused before anything is written.

The section names the branch agents land on, says that each session works
in a workspace of its own under .braid/work/, commits and lands with braid,
and reads exit status 2 as conflicts, and that nobody commits with git in
the shared checkout.

A CLAUDE.md in the directory that does not include AGENTS.md gets the line
@AGENTS.md at its end, so that Claude Code reads the section too;
--no-claude leaves it alone. CLAUDE.md is never created.

With --dry-run nothing is written: the command says what it would do, and
prints the section when AGENTS.md would change. With --json it prints one
object:

  dir         the project's directory
  branch      the branch the section names
  repository  the root of the braid repository, or null when there is none
  dry_run     whether nothing was written
  created     the files created (or, with --dry-run, that would be)
  updated     the files changed
  unchanged   the files already as they should be

A CLAUDE.md that does not exist, or that --no-claude leaves alone, is in
none of the lists.";

#[derive(Debug, Subcommand)]
pub(crate) enum AgentsAction {
    /// Write the braid section of the project's AGENTS.md, and make its
    /// CLAUDE.md include AGENTS.md.
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
    /// The branch agents land on (default: the one the section names
    /// already; inside a workspace, the one it tracks; otherwise main,
    /// master, or the repository's only branch).
    #[arg(long, value_name = "BRANCH")]
    branch: Option<String>,
    /// Say what would change, and write nothing.
    #[arg(long)]
    dry_run: bool,
    /// Leave CLAUDE.md alone.
    #[arg(long)]
    no_claude: bool,
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
    name: &'static str,
    path: PathBuf,
    action: Action,
    /// The new contents, unless unchanged.
    contents: Option<String>,
    /// What the text output says about it, once done.
    done: String,
    /// What it says with `--dry-run`.
    would: String,
}

fn init(cli: &Cli, args: &InitArgs) -> Result<ExitCode> {
    let start = match &args.dir {
        Some(dir) => std::path::absolute(dir)?,
        None => std::env::current_dir()?,
    };
    if !start.is_dir() {
        bail!("{} is not a directory", start.display());
    }
    let place = Place::find(cli, &start)?;
    let dir = match (&args.dir, &place.top) {
        (None, Some(top)) => top.clone(),
        _ => start,
    };

    let agents_path = dir.join(AGENTS_FILE);
    let current = read_text(&agents_path)?;
    let found = match &current {
        Some(text) => find_section(text).map_err(|why| {
            anyhow!("{AGENTS_FILE} has unbalanced braid markers: {why}. Fix them by hand: nothing was written")
        })?,
        None => None,
    };
    let named = match (&current, &found) {
        (Some(text), Some(range)) => named_branch(&text[range.clone()]),
        _ => None,
    };
    let branch = choose_branch(args.branch.as_deref(), named, &place)?;
    let agents = plan_agents(agents_path, current.as_deref(), found, &branch);
    let claude = if args.no_claude {
        None
    } else {
        plan_claude(&dir)?
    };
    let planned: Vec<&Planned> = std::iter::once(&agents).chain(claude.as_ref()).collect();

    if !args.dry_run {
        for file in &planned {
            if let Some(contents) = &file.contents {
                write_file(&file.path, contents)?;
            }
        }
    }

    if cli.json {
        let names = |action: Action| -> Vec<&str> {
            planned
                .iter()
                .filter(|file| file.action == action)
                .map(|file| file.name)
                .collect()
        };
        print_json(&json!({
            "dir": dir.to_string_lossy(),
            "branch": branch,
            "repository": place.repo.as_ref().map(|repo| repo.root().to_string_lossy()),
            "dry_run": args.dry_run,
            "created": names(Action::Create),
            "updated": names(Action::Update),
            "unchanged": names(Action::Unchanged),
        }))?;
        return Ok(ExitCode::SUCCESS);
    }

    let mut out = io::stdout().lock();
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
            "There is no braid repository here yet: run `braid init` in the project root."
        )?;
    }
    if args.dry_run {
        writeln!(out, "Nothing was written.")?;
        if agents.action != Action::Unchanged {
            writeln!(out, "\nThe section {AGENTS_FILE} would hold:\n")?;
            write!(out, "{}", section(&branch))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The repository and the workspace the command runs in, if any.
struct Place {
    repo: Option<Repo>,
    /// The directory the repository or the workspace starts at.
    top: Option<PathBuf>,
    /// The branch of the workspace, inside one.
    workspace_branch: Option<String>,
}

impl Place {
    fn find(cli: &Cli, start: &Path) -> Result<Place> {
        let location = match braid_repo::locate(start) {
            Ok(location) => Some(location),
            Err(braid_repo::Error::NotFound) => None,
            Err(err) => return Err(err.into()),
        };
        let repo = match (&cli.repo, &location) {
            (Some(root), _) => Some(Repo::open(root)?),
            (None, Some(Location::Root(dir))) => Some(Repo::open(dir)?),
            (None, Some(Location::Workspace { link, .. })) => Some(Repo::open(&link.repository)?),
            (None, None) => None,
        };
        let (top, workspace_branch) = match &location {
            Some(Location::Root(dir)) => (Some(dir.clone()), None),
            Some(Location::Workspace { dir, link }) => {
                // A workspace whose state is gone names no branch.
                let branch = repo.as_ref().and_then(|repo| {
                    Workspace::open(repo, &link.name)
                        .and_then(|workspace| workspace.state())
                        .ok()
                        .map(|state| state.branch)
                });
                (Some(dir.clone()), branch)
            }
            None => (None, None),
        };
        Ok(Place {
            repo,
            top,
            workspace_branch,
        })
    }
}

/// The branch the section names: the one asked for, the one the section
/// names already, the workspace's, or the repository's main one.
fn choose_branch(given: Option<&str>, named: Option<String>, place: &Place) -> Result<String> {
    let branch = match (given, named, &place.workspace_branch, &place.repo) {
        (Some(given), ..) => given.to_owned(),
        (None, Some(named), ..) => named,
        (None, None, Some(tracked), _) => tracked.clone(),
        (None, None, None, Some(repo)) => repository_branch(repo)?,
        (None, None, None, None) => "main".to_owned(),
    };
    if !braid_git::is_branch_name(&branch) || !braid_repo::is_branch(&branch) {
        bail!("{branch:?} cannot name the branch agents land on");
    }
    Ok(branch)
}

/// The repository's main branch: `main`, `master`, or its only branch;
/// `main` when it has none yet.
fn repository_branch(repo: &Repo) -> Result<String> {
    let refs = repo.store().refs()?;
    let branches: Vec<&str> = refs
        .iter()
        .map(|(name, _)| name)
        .filter(|name| braid_repo::is_branch(name))
        .collect();
    if let Some(preferred) = ["main", "master"]
        .into_iter()
        .find(|preferred| branches.contains(preferred))
    {
        return Ok(preferred.to_owned());
    }
    match branches.as_slice() {
        [] => Ok("main".to_owned()),
        [only] => Ok((*only).to_owned()),
        several => bail!(
            "cannot tell which branch agents land on: the repository has {}; pass --branch",
            several.join(", ")
        ),
    }
}

/// What `AGENTS.md` needs: to be created, to get the section, or to have
/// it rewritten in place.
fn plan_agents(
    path: PathBuf,
    current: Option<&str>,
    found: Option<Range<usize>>,
    branch: &str,
) -> Planned {
    let landing = format!("agents land on {branch}");
    let (action, contents, done, would) = match (current, found) {
        (None, _) => (
            Action::Create,
            Some(section(branch)),
            format!("Created {AGENTS_FILE} with the braid section: {landing}."),
            format!("Would create {AGENTS_FILE} with the braid section: {landing}."),
        ),
        (Some(text), Some(range)) => {
            let new = replace_section(text, range, &section(branch));
            if new == text {
                let said = format!("{AGENTS_FILE} has this braid section already: {landing}.");
                (Action::Unchanged, None, said.clone(), said)
            } else {
                (
                    Action::Update,
                    Some(new),
                    format!("Updated the braid section of {AGENTS_FILE}: {landing}."),
                    format!("Would update the braid section of {AGENTS_FILE}: {landing}."),
                )
            }
        }
        (Some(text), None) => (
            Action::Update,
            Some(append_section(text, &section(branch))),
            format!("Added the braid section to {AGENTS_FILE}: {landing}."),
            format!("Would add the braid section to {AGENTS_FILE}: {landing}."),
        ),
    };
    Planned {
        name: AGENTS_FILE,
        path,
        action,
        contents,
        done,
        would,
    }
}

/// What `CLAUDE.md` needs, or `None` when there is no such file.
fn plan_claude(dir: &Path) -> Result<Option<Planned>> {
    let path = dir.join(CLAUDE_FILE);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    let planned = |action, contents, done: &str, would: &str| Planned {
        name: CLAUDE_FILE,
        path: path.clone(),
        action,
        contents,
        done: done.to_owned(),
        would: would.to_owned(),
    };
    let includes = "CLAUDE.md includes AGENTS.md already.";
    let already = planned(Action::Unchanged, None, includes, includes);
    // A CLAUDE.md that is AGENTS.md under another name, through a symbolic
    // link, holds the section itself.
    let agents = dir.join(AGENTS_FILE);
    let is_agents = match (fs::canonicalize(&path), fs::canonicalize(&agents)) {
        (Ok(claude), Ok(agents)) => claude == agents,
        _ => false,
    };
    if is_agents
        || (meta.file_type().is_symlink()
            && fs::read_link(&path).is_ok_and(|target| {
                target == Path::new(AGENTS_FILE) || target == Path::new("./AGENTS.md")
            }))
    {
        return Ok(Some(already));
    }
    let Some(text) = read_text(&path)? else {
        // A symbolic link to nothing: there is nothing to add the line to.
        return Ok(None);
    };
    if includes_agents(&text) {
        return Ok(Some(already));
    }
    Ok(Some(planned(
        Action::Update,
        Some(add_include(&text)),
        "Added @AGENTS.md to CLAUDE.md.",
        "Would add @AGENTS.md to CLAUDE.md.",
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
each in a braid workspace of its own. Braid is pre-alpha: git stays the
source of truth, and `braid git sync` keeps the git remote in step.

- **Never edit files, `git add`, `git commit` or `git push` in the project
  root.** It is the shared checkout: work only in your workspace.
- **Start** from the project root, with a workspace named for your session
  (letters, digits, `.`, `_` or `-`; `braid workspace list` shows the names
  taken). The commands here call it `agent-1`: use your own name.

  ```bash
  braid workspace add agent-1 .braid/work/agent-1 --track {branch}
  cd .braid/work/agent-1
  ```

- **Work** in `.braid/work/agent-1`: edit, build and test there. There is
  no `.git` in it, so `git` there acts on the shared checkout: use braid.
  Record and land from the workspace, naming your session with `--agent`
  (or `BRAID_AGENT`) every time:

  ```bash
  braid --agent agent-1 status --json
  braid --agent agent-1 commit -m \"Say what changed and why\"
  braid --agent agent-1 land --json
  ```

  If your shell does not stay in the workspace between commands, add
  `--workspace agent-1` and run them from the project root.
- **Conflicts:** `land` exits with status 2, prints `\"outcome\": \"conflicts\"`
  with `--json`, and leaves `{branch}` where it was. Run
  `braid --agent agent-1 update`, which writes conflict markers into the
  files; fix them (`braid resolve <path>` settles conflicts that are not
  about content), then `commit` (the message is kept) and `land` again. In
  the JSON, `next` says what to run.
- **Before a new task**, `braid --agent agent-1 update` brings your
  workspace onto `{branch}`.
- **Done:** once everything has landed, run `braid workspace remove agent-1`
  from the project root.
- Changesets are signed by `BRAID_AUTHOR` (`\"Name <email>\"`), or else by the
  `name` and `email` in `.braid/config`.
- New files that look like secrets (`.env*`, `*.pem`, `id_*`) or are larger
  than 1 MiB wait for `braid track <path>`. Never track a secret.
- `braid undo` reverts the newest operation in the repository, whoever made
  it: read `braid op log` first.
- `braid git sync \"$(git remote get-url origin)\" --branch {branch}` sends what
  landed to the git remote and brings in what others pushed there. Run it
  when the user asks for it.
- No `.braid` directory in the project root, or no `braid` command? Braid is
  not set up in this checkout: follow <https://braid.elffuss.com/agents.md>,
  or ask the user.
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

/// Where the braid section is in `text`: from the start of its start
/// marker's line to the end of its end marker's line. `None` when there are
/// no markers; an error that says where they are when they do not pair up.
pub(crate) fn find_section(text: &str) -> Result<Option<Range<usize>>, String> {
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    let mut offset = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if is_marker(line, START) {
            starts.push((index + 1, offset));
        } else if is_marker(line, END) {
            ends.push((index + 1, offset + line.len()));
        }
        offset += line.len();
    }
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([(first, from)], [(last, to)]) if first < last => Ok(Some(*from..*to)),
        _ => Err(format!(
            "`{START}` is on {}, `{END}` on {}, and there must be one of each, the start first",
            line_list(&starts),
            line_list(&ends)
        )),
    }
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
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push_str(end);
        }
        if !out.trim_end_matches(['\n', '\r']).is_empty() && !ends_with_blank_line(&out) {
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

/// The branch a section was written for, as its first comment says.
fn named_branch(section: &str) -> Option<String> {
    let (_, rest) = section.split_once(BRANCH_NOTE)?;
    let (name, _) = rest.split_once(')')?;
    braid_git::is_branch_name(name).then(|| name.to_owned())
}

/// Whether a `CLAUDE.md` includes `AGENTS.md`: a word `@AGENTS.md` or
/// `@./AGENTS.md`, as Claude Code reads its imports.
pub(crate) fn includes_agents(text: &str) -> bool {
    text.lines().any(|line| {
        line.split_whitespace().any(|word| {
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

/// A file's text, or `None` when there is no such file.
fn read_text(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| anyhow!("{} is not UTF-8 text: nothing was written", path.display())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Writes `contents` to `path` through a temporary file and a rename, so
/// that the file is never seen half written. A symbolic link stays one: the
/// file it points to is the one replaced, and keeps its permissions.
fn write_file(path: &Path, contents: &str) -> Result<()> {
    let target = match fs::canonicalize(path) {
        Ok(target) => target,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if fs::symlink_metadata(path).is_ok() {
                bail!(
                    "{} is a symbolic link to a file that does not exist",
                    path.display()
                );
            }
            path.to_path_buf()
        }
        Err(err) => return Err(err).with_context(|| format!("cannot open {}", path.display())),
    };
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        bail!("cannot write {}", target.display());
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
            Err(err) => {
                return Err(err).with_context(|| format!("cannot write in {}", parent.display()));
            }
        }
    };
    let mut written = || -> io::Result<()> {
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(&temp, meta.permissions())?;
        }
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, &target)
    };
    if let Err(err) = written() {
        let _ = fs::remove_file(&temp);
        return Err(err).with_context(|| format!("cannot write {}", target.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABOVE: &str = "# Project\n\nRules of our own.\n\n";
    const BELOW: &str = "\n## More rules\n\nKept as they are.\n";

    #[test]
    fn the_section_names_the_branch_and_holds_no_marker_inside() {
        let text = section("trunk");
        assert!(text.starts_with(&format!("{START}\n")));
        assert!(text.ends_with(&format!("\n{END}\n")));
        assert!(text.contains("--track trunk\n"));
        assert!(text.contains("land their changes on `trunk`"));
        assert!(text.contains("--branch trunk`"));
        assert!(
            !text.contains("`main`") && !text.contains(" main"),
            "{text}"
        );
        assert_eq!(named_branch(&text).as_deref(), Some("trunk"));
        // The markers appear once each, on their own lines.
        assert_eq!(find_section(&text), Ok(Some(0..text.len())));
        assert!(text.lines().all(|line| line.len() <= 100), "{text}");
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
        let range = find_section(&crlf).unwrap().unwrap();
        assert_eq!(replace_section(&crlf, range, &new), crlf);
    }

    #[test]
    fn only_the_text_between_the_markers_changes() {
        let old = format!("{START}\nan older section\n{END}\n");
        let text = format!("{ABOVE}{old}{BELOW}");
        let range = find_section(&text).unwrap().unwrap();
        assert_eq!(&text[range.clone()], old);
        let new = replace_section(&text, range, &section("main"));
        assert_eq!(new, format!("{ABOVE}{}{BELOW}", section("main")));
        // Again: nothing changes.
        let range = find_section(&new).unwrap().unwrap();
        assert_eq!(replace_section(&new, range, &section("main")), new);
        // The older section named no branch.
        assert_eq!(named_branch(&old), None);
    }

    #[test]
    fn markers_may_be_indented_a_little_but_must_be_alone() {
        let text = format!("   {START}  \nx\n{END}\n");
        assert_eq!(find_section(&text), Ok(Some(0..text.len())));
        for not_markers in [
            format!("    {START}\n{END}\n"),
            format!("see `{START}`\n{END}\n"),
            format!("{START} and more\n{END}\n"),
        ] {
            assert!(find_section(&not_markers).is_err(), "{not_markers:?}");
        }
        assert_eq!(find_section("no markers at all\n"), Ok(None));
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
            let err = find_section(&text).unwrap_err();
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
        ] {
            assert!(includes_agents(yes), "{yes:?}");
        }
        for no in [
            "",
            "Read AGENTS.md\n",
            "`@AGENTS.md`\n",
            "@AGENTS.md.bak\n",
            "@docs/AGENTS.md\n",
        ] {
            assert!(!includes_agents(no), "{no:?}");
        }
        assert_eq!(add_include(""), "@AGENTS.md\n");
        assert_eq!(add_include("# Claude"), "# Claude\n@AGENTS.md\n");
        assert_eq!(add_include("# Claude\n"), "# Claude\n@AGENTS.md\n");
        assert_eq!(add_include("# Claude\r\n"), "# Claude\r\n@AGENTS.md\r\n");
        assert!(includes_agents(&add_include("# Claude\n")));
    }
}
