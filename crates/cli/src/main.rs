// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid`, the command-line tool.

mod agents;
mod history;
mod hub;
mod revert;
mod sync;
mod verify;
mod work;

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use braid_format::changeset::decode_change_list;
use braid_format::{
    Change, ChangeOp, Changes, Changeset, EntryKind, IdHasher, Kind, ObjectId, Objects, Signature,
};
use braid_repo::Repo;
use braid_store::{FsckReport, Operation, RefUpdate};
use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};
use sync::Direction;

/// Version control for many agents landing changes at once.
#[derive(Debug, Parser)]
#[command(name = "braid", version, about, long_about = None)]
struct Cli {
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,

    /// The repository's root directory (default: found from the current
    /// directory, the nearest one, itself or a parent, that holds a `.braid`
    /// directory, or, inside a workspace, its `.braid` file, which names
    /// the repository).
    #[arg(long, global = true, value_name = "DIR")]
    repo: Option<PathBuf>,

    /// Who is acting: a label recorded in the op log with every ref this
    /// command moves, and as the `agent` field of every changeset it
    /// creates.
    #[arg(long, global = true, env = "BRAID_AGENT", default_value = "cli")]
    agent: String,

    /// Workspace to act on (default: the one containing the current
    /// directory).
    #[arg(long, global = true, value_name = "NAME")]
    workspace: Option<String>,

    /// How hard the store pushes writes to stable storage: `ordered` may
    /// lose the last operations on power loss, never consistency; `full`
    /// keeps everything a command acknowledged, at some cost.
    #[arg(
        long,
        global = true,
        value_enum,
        env = "BRAID_DURABILITY",
        default_value_t = DurabilityArg::Ordered
    )]
    durability: DurabilityArg,

    #[command(subcommand)]
    command: Command,

    /// The global flags given on the command line, as a hint repeats them:
    /// empty, or each flag and its value followed by a space.
    #[arg(skip)]
    given: String,
}

/// `--durability`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum DurabilityArg {
    /// Consistent after a crash; the last operations may be lost.
    Ordered,
    /// Everything acknowledged survives a power loss.
    Full,
}

impl Cli {
    /// The store options `--durability` asks for.
    pub(crate) fn store_options(&self) -> braid_store::Options {
        braid_store::Options {
            durability: match self.durability {
                DurabilityArg::Ordered => braid_store::Durability::Ordered,
                DurabilityArg::Full => braid_store::Durability::Full,
            },
            ..braid_store::Options::default()
        }
    }

    /// A command for a hint to print: `braid`, the global flags this
    /// command was given (`--repo`, `--agent`, `--workspace`), then `rest`,
    /// so that a hint followed as written acts on the same workspace, for
    /// the same agent.
    pub(crate) fn hint(&self, rest: &str) -> String {
        format!("braid {}{rest}", self.given)
    }
}

/// The global flags typed on the command line, as [`Cli::given`] holds
/// them. A value that came from the environment, such as `BRAID_AGENT`, is
/// left out: the environment that set it sets it again.
fn given_flags(cli: &Cli, matches: &ArgMatches) -> String {
    // A global flag is recorded where it was typed, before or after a
    // subcommand, and copied to the others.
    let typed = |id: &str| {
        let mut current = Some(matches);
        while let Some(found) = current {
            if found.value_source(id) == Some(ValueSource::CommandLine) {
                return true;
            }
            current = found.subcommand().map(|(_, sub)| sub);
        }
        false
    };
    let mut given = String::new();
    let repo = cli.repo.as_ref().map(|repo| repo.to_string_lossy());
    for (id, value) in [
        ("repo", repo.as_deref()),
        ("agent", Some(cli.agent.as_str())),
        ("workspace", cli.workspace.as_deref()),
    ] {
        if let Some(value) = value
            && typed(id)
        {
            given.push_str(&flag(id, value));
            given.push(' ');
        }
    }
    given
}

/// `--name value`, quoted for a POSIX shell where it needs to be, or
/// `--name=value` when the value starts with `-`.
fn flag(name: &str, value: &str) -> String {
    let word = shell_word(value);
    if value.starts_with('-') {
        format!("--{name}={word}")
    } else {
        format!("--{name} {word}")
    }
}

/// `text` as one word of a POSIX shell: as it is when that is safe, or
/// between single quotes.
pub(crate) fn shell_word(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-./:@%+=,".contains(&byte));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a repository.
    Init {
        /// Directory to create it in (default: the current directory).
        dir: Option<PathBuf>,
    },
    /// Set a project up for coding agents: the rules in its AGENTS.md.
    Agents {
        #[command(subcommand)]
        action: agents::AgentsAction,
    },
    /// Print the braid blob id of each file ("-" reads standard input).
    HashObject {
        /// Also store each file in the repository.
        #[arg(short, long)]
        write: bool,
        /// Files to hash.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Write an object's raw payload to standard output: a file's content,
    /// for one named REV:PATH.
    CatObject {
        /// A ref, an object id, a short id (6 hex digits or more), or
        /// REV:PATH, the file or directory at PATH, from the repository's
        /// root, in the changeset REV.
        revision: String,
    },
    /// List refs and the objects they point at.
    Refs,
    /// Move or delete a ref in one op log transaction.
    Ref {
        #[command(subcommand)]
        action: RefAction,
    },
    /// Inspect and rewind the op log.
    Op {
        #[command(subcommand)]
        action: OpAction,
    },
    /// Revert the newest operation that is not already undone.
    Undo,
    /// Show history, following first parents; after `--`, only the
    /// changesets that change one of the paths or anything below them.
    #[command(long_about = LOG_HELP)]
    Log {
        /// Where to start: a ref, a changeset id, or a short id (6 hex
        /// digits or more).
        #[arg(default_value = "main")]
        revision: String,
        /// How many changesets to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Paths, from the repository's root, after `--`.
        #[arg(last = true, value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Show the changeset that last changed each line of a file, along
    /// first parents.
    #[command(long_about = history::BLAME_HELP)]
    Blame {
        /// The revision: a ref, a changeset id, or a short id.
        revision: String,
        /// The file, from the repository's root.
        path: String,
    },
    /// Compute derived data ahead of time: generation numbers, skip
    /// pointers and changed-path filters.
    #[command(long_about = history::DERIVE_HELP)]
    Derive {
        /// Every changeset reachable from any ref.
        #[arg(long)]
        all: bool,
        /// Revisions whose history to derive (default: main, unless --all).
        revisions: Vec<String>,
    },
    /// Show one changeset: parents, root, signatures, agent, message, and
    /// the files it changes against its first parent, as A, M, D or T.
    Show {
        /// A ref, a changeset id, or a short id (6 hex digits or more).
        #[arg(default_value = "main")]
        revision: String,
    },
    /// Show the files that differ between two changesets: A (added), M
    /// (modified), D (deleted) or T (type changed), as `git diff
    /// --name-status` does.
    #[command(long_about = DIFF_HELP)]
    Diff {
        /// The changeset to compare from: a ref, an id, or a short id.
        from: String,
        /// The changeset to compare to: a ref, an id, or a short id.
        to: String,
    },
    /// Import from and export to git repositories.
    Git {
        #[command(subcommand)]
        action: GitAction,
    },
    /// Sign in to a hub, with GitHub's device flow.
    #[command(long_about = hub::LOGIN_HELP)]
    Login(hub::LoginArgs),
    /// Sign out of a hub: revoke the saved token, and delete it here.
    #[command(long_about = hub::LOGOUT_HELP)]
    Logout(hub::LoginArgs),
    /// Put the repository on a hub, clone one, fetch from it, and compare.
    Hub {
        #[command(subcommand)]
        action: hub::HubAction,
    },
    /// Check every record, index, op log entry and ref of the store.
    Fsck,
    /// Show what the store holds: objects by kind and by codec, with their
    /// sizes as stored and decoded.
    #[command(long_about = STATS_HELP)]
    Stats,
    /// Create, list and remove workspaces.
    Workspace {
        #[command(subcommand)]
        action: work::WorkspaceAction,
    },
    /// Show how the workspace differs from its base and its branch.
    #[command(long_about = work::STATUS_HELP)]
    Status(work::IdentityArgs),
    /// Record the workspace's changes as a changeset on its base.
    Commit(work::CommitArgs),
    /// Land the workspace's changesets on its branch (exit status 2 on
    /// conflicts).
    #[command(long_about = work::LAND_HELP)]
    Land(work::LandArgs),
    /// Bring the workspace onto its branch's head, rebasing its own
    /// changesets (exit status 2 on conflicts).
    #[command(long_about = work::UPDATE_HELP)]
    Update(work::UpdateArgs),
    /// Mark conflicts of a rebase in progress resolved.
    Resolve {
        /// Paths whose conflicts are settled.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Let new files that look like secrets or are large be committed.
    Track {
        /// Files, or directories whose new files may be committed.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Make changesets without a working copy.
    Change {
        #[command(subcommand)]
        action: work::ChangeAction,
    },
    /// Undo a changeset with a new one, and land it (exit status 2 on
    /// conflicts).
    #[command(long_about = revert::REVERT_HELP)]
    Revert(revert::RevertArgs),
    /// Run the tests on a branch's head, keep its green ref at the newest
    /// changeset that passed, and find what broke them (exit status 4 when
    /// the branch is red).
    #[command(long_about = verify::VERIFY_HELP)]
    Verify(verify::VerifyArgs),
    /// Kill a test run's process group when verify goes away (internal).
    #[command(name = verify::WATCHDOG, hide = true)]
    VerifyWatchdog {
        /// The run's scratch directory.
        dir: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum RefAction {
    /// Point a ref at an object.
    Set {
        /// Ref name.
        name: String,
        /// What to point at: a ref, an object id, or a short id (6 hex
        /// digits or more).
        target: String,
        /// Only move the ref if it currently points at this id ("none" if it
        /// must not exist yet).
        #[arg(long, value_name = "ID")]
        expect: Option<String>,
    },
    /// Delete a ref.
    Delete {
        /// Ref name.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum GitAction {
    /// Import every branch and tag of a git repository. Running it again
    /// imports only what is new.
    Import {
        /// The git repository (a working tree or a bare repository).
        path: PathBuf,
    },
    /// Write braid refs, and everything they reach, into an existing git
    /// repository. Git refs of the same names are overwritten.
    Export {
        /// The git repository.
        path: PathBuf,
        /// A braid ref to export; repeat for several (default: all).
        #[arg(long = "ref", value_name = "NAME")]
        refs: Vec<String>,
    },
    /// Keep a braid branch and a branch of a git remote in step: fetch and
    /// import the remote branch, then fast-forward the braid branch, or push
    /// it (merging first when both moved) as a fast-forward with a lease, so
    /// the remote's history is never rewritten. By default a remote that
    /// someone else wrote to is merged in; `--direction publish` or `follow`
    /// stops instead. Exits with 2 on conflicts, 3 when the branches are not
    /// in the relation `--direction` requires, and 1 when the push is refused.
    Sync {
        /// The git remote: a URL or a path.
        remote: String,
        /// The braid branch.
        #[arg(long, default_value = "main")]
        branch: String,
        /// The branch of the remote (default: the braid branch's name).
        #[arg(long, value_name = "NAME")]
        remote_branch: Option<String>,
        /// Which way changes may go: `both` merges when both sides moved;
        /// `publish` only pushes, and stops unless the remote branch is the
        /// braid branch or one of its ancestors; `follow` only takes the
        /// remote's commits in, and stops unless the braid branch is the
        /// remote branch or one of its ancestors.
        #[arg(long, value_enum, default_value_t = Direction::Both)]
        direction: Direction,
        /// Report what would happen without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum OpAction {
    /// Show the newest operations, newest first.
    Log {
        /// How many to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
}

/// The exit status of a command whose reader closed standard output early,
/// as a shell reports a process that SIGPIPE ended: 128 + 13.
const BROKEN_PIPE: i32 = 141;

fn main() -> ExitCode {
    quiet_on_broken_pipe();
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    cli.given = given_flags(&cli, &matches);
    match run(&cli) {
        Ok(code) => code,
        // A reader that stopped early is no failure: say nothing, as git
        // does. braid writes to no other pipe (git runs with no input), so a
        // broken pipe is standard output's.
        Err(err) if broken_pipe(&err) => ExitCode::from(BROKEN_PIPE as u8),
        Err(err) => {
            eprintln!("braid: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Whether `err` comes from a write to a reader that went away.
fn broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|err| err.kind() == io::ErrorKind::BrokenPipe)
            || cause
                .downcast_ref::<serde_json::Error>()
                .is_some_and(|err| err.io_error_kind() == Some(io::ErrorKind::BrokenPipe))
    })
}

/// Makes `print!` and `println!` exit quietly, with [`BROKEN_PIPE`], once
/// the reader of standard output went away (`braid log | head`), instead
/// of panicking. Standard error is left alone.
fn quiet_on_broken_pipe() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        if message.starts_with("failed printing to stdout")
            && (message.contains("Broken pipe") || message.contains("os error 32"))
        {
            std::process::exit(BROKEN_PIPE);
        }
        default(info);
    }));
}

/// Exits quietly with [`BROKEN_PIPE`] when a write to standard output failed
/// because its reader went away; any other error is returned.
fn stdout_written<T>(result: io::Result<T>) -> io::Result<T> {
    if let Err(err) = &result
        && err.kind() == io::ErrorKind::BrokenPipe
    {
        std::process::exit(BROKEN_PIPE);
    }
    result
}

fn run(cli: &Cli) -> Result<ExitCode> {
    if cli.agent == verify::AGENT
        && !matches!(
            cli.command,
            Command::Verify(_) | Command::VerifyWatchdog { .. }
        )
    {
        eprintln!("braid: warning: {}", verify::AGENT_WARNING);
    }
    match &cli.command {
        Command::Init { dir } => init(cli, dir.as_deref()),
        Command::Agents { action } => agents::agents(cli, action),
        Command::HashObject { write, paths } => hash_object(cli, paths, *write),
        Command::CatObject { revision } => cat_object(cli, revision),
        Command::Refs => refs(cli),
        Command::Ref { action } => ref_action(cli, action),
        Command::Op {
            action: OpAction::Log { limit },
        } => op_log(cli, *limit),
        Command::Undo => undo(cli),
        Command::Log {
            revision,
            limit,
            paths,
        } => log(cli, revision, *limit, paths),
        Command::Blame { revision, path } => history::blame(cli, revision, path),
        Command::Derive { all, revisions } => history::derive(cli, *all, revisions),
        Command::Show { revision } => show(cli, revision),
        Command::Diff { from, to } => diff(cli, from, to),
        Command::Git {
            action: GitAction::Import { path },
        } => git_import(cli, path),
        Command::Git {
            action: GitAction::Export { path, refs },
        } => git_export(cli, path, refs),
        Command::Git {
            action:
                GitAction::Sync {
                    remote,
                    branch,
                    remote_branch,
                    direction,
                    dry_run,
                },
        } => sync::git_sync(
            cli,
            remote,
            branch,
            remote_branch.as_deref(),
            *direction,
            *dry_run,
        ),
        Command::Login(args) => hub::login(cli, args),
        Command::Logout(args) => hub::logout(cli, args),
        Command::Hub { action } => hub::hub(cli, action),
        Command::Fsck => fsck(cli),
        Command::Stats => stats(cli),
        Command::Workspace { action } => work::workspace(cli, action),
        Command::Status(args) => work::status(cli, args),
        Command::Commit(args) => work::commit(cli, args),
        Command::Land(args) => work::land(cli, args),
        Command::Update(args) => work::update(cli, args),
        Command::Resolve { paths } => work::resolve(cli, paths),
        Command::Track { paths } => work::track(cli, paths),
        Command::Change { action } => work::change(cli, action),
        Command::Revert(args) => revert::revert(cli, args),
        Command::Verify(args) => verify::verify(cli, args),
        Command::VerifyWatchdog { dir } => verify::watchdog(dir),
    }
}

fn open_repo(cli: &Cli) -> Result<Repo> {
    let repo = match &cli.repo {
        Some(dir) => Repo::open_with(dir, cli.store_options()),
        None => Repo::discover_with(&std::env::current_dir()?, cli.store_options()),
    };
    Ok(repo?)
}

fn print_json(value: &Value) -> Result<()> {
    // Serialized first, so that a closed pipe shows up as an I/O error here
    // rather than inside serde_json's own error type.
    let mut text = serde_json::to_vec(value)?;
    text.push(b'\n');
    let mut out = io::stdout().lock();
    stdout_written(out.write_all(&text))?;
    stdout_written(out.flush())?;
    Ok(())
}

fn init(cli: &Cli, dir: Option<&Path>) -> Result<ExitCode> {
    let dir = match dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let repo = Repo::init_with(&dir, cli.store_options())?;
    if cli.json {
        print_json(&json!({ "root": repo.root().to_string_lossy() }))?;
    } else {
        println!(
            "Created an empty braid repository in {}",
            repo.root().join(braid_repo::DIR).display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

struct Hashed<'a> {
    path: &'a Path,
    id: ObjectId,
    size: u64,
}

fn hash_object(cli: &Cli, paths: &[PathBuf], write: bool) -> Result<ExitCode> {
    let repo = if write { Some(open_repo(cli)?) } else { None };
    let mut hashed = Vec::with_capacity(paths.len());
    for path in paths {
        let (id, size) = match &repo {
            // Streamed into the store: large files are cut into chunks as
            // they are read, never held in memory whole.
            Some(repo) if path.as_os_str() == "-" => repo
                .store()
                .put_blob_from(io::stdin().lock(), None)
                .context("cannot store standard input")?,
            Some(repo) => {
                let file =
                    File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
                let size = file.metadata().ok().map(|meta| meta.len());
                repo.store()
                    .put_blob_from(file, size)
                    .with_context(|| format!("cannot store {}", path.display()))?
            }
            None if path.as_os_str() == "-" => {
                hash_blob(io::stdin().lock()).context("cannot read standard input")?
            }
            None => {
                let file =
                    File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
                hash_blob(file).with_context(|| format!("cannot read {}", path.display()))?
            }
        };
        hashed.push(Hashed { path, id, size });
    }
    if let Some(repo) = &repo {
        repo.store().sync()?;
    }

    if cli.json {
        let items: Vec<Value> = hashed
            .iter()
            .map(|h| {
                json!({
                    "path": h.path.to_string_lossy(),
                    "kind": Kind::Blob.name(),
                    "id": h.id.to_hex(),
                    "size": h.size,
                })
            })
            .collect();
        print_json(&Value::Array(items))?;
    } else {
        let mut out = io::stdout().lock();
        for h in &hashed {
            writeln!(out, "{}  {}", h.id, h.path.display())?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Hashes a stream as a blob without holding it in memory.
fn hash_blob(mut reader: impl Read) -> io::Result<(ObjectId, u64)> {
    let mut hasher = IdHasher::new(Kind::Blob);
    let mut buf = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hasher.finalize(), size))
}

/// The object a revision names, or for `REV:PATH`, the object at PATH in
/// the tree of the changeset REV: the blob of a file or symlink, or the tree
/// of a directory. A revision that resolves as it is wins, so a ref with a
/// `:` in its name still names that ref.
fn resolve_object(repo: &Repo, revision: &str) -> Result<ObjectId> {
    let (rev, path) = match (repo.resolve(revision), revision.split_once(':')) {
        (Ok(id), _) => return Ok(id),
        (Err(braid_repo::Error::UnknownRevision(_)), Some(split)) => split,
        (Err(err), _) => return Err(err.into()),
    };
    let root = read_changeset(repo, &resolve_changeset(repo, rev)?)?.root;
    let path = path.strip_prefix("./").unwrap_or(path).trim_matches('/');
    if path.is_empty() {
        return Ok(root);
    }
    match braid_engine::entry_at(repo.store(), root, path.as_bytes())? {
        Some(
            EntryKind::File { blob, .. }
            | EntryKind::Executable { blob, .. }
            | EntryKind::Symlink { blob, .. },
        ) => Ok(blob),
        Some(EntryKind::Tree { tree }) => Ok(tree),
        Some(EntryKind::Gitlink { .. }) => {
            bail!("{path:?} is a submodule in {rev}: its content is in another repository")
        }
        None => bail!("{rev} has no file or directory at {path:?}"),
    }
}

fn cat_object(cli: &Cli, revision: &str) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let id = resolve_object(&repo, revision)?;
    if cli.json {
        // The size is in the record's header: nothing needs decoding.
        let Some((kind, size)) = repo.store().stat(&id)? else {
            bail!("object {id} is not in the store");
        };
        print_json(&json!({
            "id": id.to_hex(),
            "kind": kind.name(),
            "size": size,
        }))?;
    } else {
        // Streamed: a large blob is written a chunk at a time.
        let mut out = io::stdout().lock();
        if repo.store().read_to(&id, &mut out)?.is_none() {
            bail!("object {id} is not in the store");
        }
        out.flush()?;
    }
    Ok(ExitCode::SUCCESS)
}

fn refs(cli: &Cli) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let refs = repo.store().refs()?;
    if cli.json {
        let map: serde_json::Map<String, Value> = refs
            .iter()
            .map(|(name, id)| (name.to_owned(), Value::String(id.to_hex())))
            .collect();
        print_json(&json!({ "seq": refs.seq(), "refs": map }))?;
    } else {
        for (name, id) in refs.iter() {
            println!("{id}  {name}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn parse_expect(text: &str) -> Result<Option<ObjectId>> {
    if text == "none" {
        Ok(None)
    } else {
        Ok(Some(text.parse().with_context(|| {
            format!("--expect takes a full object id or \"none\", not {text:?}")
        })?))
    }
}

fn ref_action(cli: &Cli, action: &RefAction) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let (name, new, expect, description) = match action {
        RefAction::Set {
            name,
            target,
            expect,
        } => (
            name,
            Some(repo.resolve(target)?),
            expect.as_deref().map(parse_expect).transpose()?,
            format!("ref set {name}"),
        ),
        RefAction::Delete { name } => (name, None, None, format!("ref delete {name}")),
    };
    let op = repo
        .store()
        .transact(&cli.agent, &description, |refs| {
            let current = refs.get(name);
            if let Some(expected) = expect
                && expected != current
            {
                return Err(format!(
                    "ref {name} points at {}, not the expected {}",
                    show_id(current),
                    show_id(expected)
                ));
            }
            if new.is_none() && current.is_none() {
                return Err(format!("ref {name} does not exist"));
            }
            Ok(vec![RefUpdate::new(name.clone(), current, new)])
        })
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    print_operation(cli, &op)?;
    Ok(ExitCode::SUCCESS)
}

fn show_id(id: Option<ObjectId>) -> String {
    id.map_or_else(|| "nothing".to_owned(), |id| id.to_hex())
}

fn operation_json(op: &Operation) -> Value {
    json!({
        "seq": op.seq,
        "time": format_time(op.time_ms),
        "time_ms": op.time_ms,
        "agent": op.agent,
        "description": op.description,
        "undoes": op.undoes,
        "updates": op.updates.iter().map(|u| json!({
            "name": u.name,
            "old": u.old.map(|id| id.to_hex()),
            "new": u.new.map(|id| id.to_hex()),
        })).collect::<Vec<_>>(),
    })
}

fn print_operation(cli: &Cli, op: &Operation) -> Result<()> {
    if cli.json {
        return print_json(&operation_json(op));
    }
    let mut out = io::stdout().lock();
    write_operation(&mut out, op)?;
    Ok(())
}

fn write_operation(out: &mut impl Write, op: &Operation) -> io::Result<()> {
    writeln!(
        out,
        "#{}  {}  {}  {}",
        op.seq,
        format_time(op.time_ms),
        op.agent,
        op.description
    )?;
    for update in &op.updates {
        writeln!(
            out,
            "    {}: {} -> {}",
            update.name,
            short(update.old),
            short(update.new)
        )?;
    }
    Ok(())
}

fn short(id: Option<ObjectId>) -> String {
    id.map_or_else(|| "(none)".to_owned(), |id| id.short())
}

fn op_log(cli: &Cli, limit: usize) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let ops = repo.store().log(limit)?;
    if cli.json {
        print_json(&Value::Array(ops.iter().map(operation_json).collect()))?;
    } else {
        let mut out = io::stdout().lock();
        for op in &ops {
            write_operation(&mut out, op)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn undo(cli: &Cli) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let op = repo.store().undo_last(&cli.agent)?;
    print_operation(cli, &op)?;
    Ok(ExitCode::SUCCESS)
}

fn fsck(cli: &Cli) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let report: FsckReport = repo.store().fsck()?;
    let problems: Vec<String> = report.problems.iter().map(ToString::to_string).collect();
    if cli.json {
        print_json(&json!({
            "clean": report.is_clean(),
            "segments": report.segments,
            "sealed_segments": report.sealed_segments,
            "records": report.records,
            "delta_records": report.delta_records,
            "chunked_records": report.chunked_records,
            "objects": report.objects,
            "operations": report.operations,
            "refs": report.refs,
            "problems": problems,
        }))?;
    } else {
        println!(
            "{} objects in {} segments, {} operations, {} refs",
            report.objects, report.segments, report.operations, report.refs
        );
        println!(
            "{} records: {} deltas, {} chunked blobs",
            report.records, report.delta_records, report.chunked_records
        );
        for problem in &problems {
            println!("problem: {problem}");
        }
        println!(
            "{}",
            if report.is_clean() {
                "clean"
            } else {
                "NOT clean"
            }
        );
    }
    Ok(if report.is_clean() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The long help of `stats`.
const STATS_HELP: &str = "Show what the store holds: objects by kind and by codec, with their
sizes as stored and decoded, and the size of the files that hold them. It
reads the indexes and journals only, never a payload.

Codecs: raw (as it is), zstd (compressed on its own), delta (compressed
against a similar object, its base: the same path one change earlier),
chunked (a blob of 4 MiB or more, stored as the list of its chunks; the
chunks are blobs of their own, so content shared by several versions of a
file is stored once).

With --json, stats prints objects, stored_bytes (payloads as stored),
raw_bytes (distinct content: chunked blobs count through their chunks),
record_bytes (with record headers and padding), segment_bytes and
index_bytes (the files), kinds (objects, stored_bytes and raw_bytes for
each of blob, tree, changeset, chunklist and derived) and codecs (the same
for each of raw, zstd, dictionary, delta and chunked).";

fn stats(cli: &Cli) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let stats = repo.store().stats()?;
    let sizes = |objects: u64, stored: u64, raw: u64| json!({ "objects": objects, "stored_bytes": stored, "raw_bytes": raw });
    if cli.json {
        let kinds: serde_json::Map<String, Value> = stats
            .kinds
            .iter()
            .map(|k| {
                (
                    k.kind.name().to_owned(),
                    sizes(k.objects, k.stored_bytes, k.raw_bytes),
                )
            })
            .collect();
        let codecs: serde_json::Map<String, Value> = stats
            .codecs
            .iter()
            .map(|c| {
                (
                    c.codec.name().to_owned(),
                    sizes(c.objects, c.stored_bytes, c.raw_bytes),
                )
            })
            .collect();
        print_json(&json!({
            "objects": stats.objects,
            "stored_bytes": stats.stored_bytes,
            "raw_bytes": stats.raw_bytes,
            "record_bytes": stats.record_bytes,
            "segment_bytes": stats.segment_bytes,
            "index_bytes": stats.index_bytes,
            "kinds": kinds,
            "codecs": codecs,
        }))?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "{:<12} {:>10} {:>14} {:>14}",
        "", "objects", "stored", "raw"
    )?;
    for k in stats.kinds.iter().filter(|k| k.objects > 0) {
        writeln!(
            out,
            "{:<12} {:>10} {:>14} {:>14}",
            k.kind.name(),
            k.objects,
            k.stored_bytes,
            k.raw_bytes
        )?;
    }
    writeln!(out)?;
    for c in stats.codecs.iter().filter(|c| c.objects > 0) {
        writeln!(
            out,
            "{:<12} {:>10} {:>14} {:>14}",
            c.codec.name(),
            c.objects,
            c.stored_bytes,
            c.raw_bytes
        )?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "{} objects: {} bytes stored for {} bytes of distinct content",
        stats.objects, stats.stored_bytes, stats.raw_bytes
    )?;
    writeln!(
        out,
        "files: {} bytes of segments, {} bytes of indexes and journals",
        stats.segment_bytes, stats.index_bytes
    )?;
    Ok(ExitCode::SUCCESS)
}

/// A changeset named by a ref, its id or a short id.
pub(crate) fn resolve_changeset(repo: &Repo, revision: &str) -> Result<ObjectId> {
    Ok(repo.resolve_kind(revision, Kind::Changeset)?)
}

fn read_changeset(repo: &Repo, id: &ObjectId) -> Result<Changeset> {
    match repo.store().get(id)? {
        Some((Kind::Changeset, bytes)) => {
            Ok(Changeset::decode(&bytes)
                .with_context(|| format!("changeset {id} does not decode"))?)
        }
        Some((kind, _)) => bail!("{id} is a {kind}, not a changeset"),
        None => bail!("changeset {id} is not in the store"),
    }
}

fn read_changes(repo: &Repo, changeset: &Changeset) -> Result<Vec<Change>> {
    match &changeset.changes {
        Changes::Inline(changes) => Ok(changes.clone()),
        Changes::External { list, .. } => {
            let Some((_, bytes)) = repo.store().get(list)? else {
                bail!("change list {list} is not in the store");
            };
            Ok(decode_change_list(&bytes)
                .with_context(|| format!("change list {list} does not decode"))?)
        }
    }
}

fn signature_json(signature: &Signature) -> Value {
    json!({
        "name": String::from_utf8_lossy(&signature.name),
        "email": String::from_utf8_lossy(&signature.email),
        "seconds": signature.seconds,
        "offset_minutes": signature.offset_minutes,
        "time": format_signature_time(signature),
    })
}

/// The agent a changeset names in its `agent` field, if it names one.
fn agent_of(changeset: &Changeset) -> Option<String> {
    changeset
        .extras
        .iter()
        .find(|(key, _)| key.as_slice() == braid_engine::AGENT)
        .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
}

fn changeset_json(id: &ObjectId, changeset: &Changeset) -> Value {
    json!({
        "id": id.to_hex(),
        "parents": changeset.parents.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        "root": changeset.root.to_hex(),
        "author": signature_json(&changeset.author),
        "committer": signature_json(&changeset.committer),
        "agent": agent_of(changeset),
        "message": String::from_utf8_lossy(&changeset.message),
        "extras": changeset.extras.iter().map(|(k, v)| json!([
            String::from_utf8_lossy(k), String::from_utf8_lossy(v)
        ])).collect::<Vec<_>>(),
        "git_origin": changeset.git_origin.map(|id| id.to_hex()),
    })
}

fn subject(message: &[u8]) -> String {
    let text = String::from_utf8_lossy(message);
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// The long help of `log`.
const LOG_HELP: &str = "Show history from a revision, following first parents, newest first: the
short id, the author's date, the author with the agent, and the subject.

With paths after `--` (log main -- src/lib.rs docs), only the changesets
whose changes, against their first parent, touch one of the paths or
anything below them, as `git log --first-parent -- <path>` lists them.
Paths are given from the repository's root and renames are not followed.
Changed-path filters, derived data kept in the store, let the walk skip the
other changesets without reading them.

With --json, log prints each changeset as show does, without its changes.";

fn log(cli: &Cli, revision: &str, limit: usize, paths: &[String]) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let head = resolve_changeset(&repo, revision)?;
    let mut entries = Vec::new();
    if paths.is_empty() {
        let mut next = Some(head);
        while let Some(id) = next {
            if entries.len() == limit {
                break;
            }
            let changeset = read_changeset(&repo, &id)?;
            next = changeset.parents.first().copied();
            entries.push((id, changeset));
        }
    } else {
        for id in history::path_log(&repo, head, paths, limit)? {
            entries.push((id, read_changeset(&repo, &id)?));
        }
    }
    if cli.json {
        print_json(&Value::Array(
            entries
                .iter()
                .map(|(id, changeset)| changeset_json(id, changeset))
                .collect(),
        ))?;
    } else {
        let mut out = io::stdout().lock();
        for (id, changeset) in &entries {
            let agent = agent_of(changeset).map_or_else(String::new, |agent| format!(" ({agent})"));
            writeln!(
                out,
                "{}  {}  {}{agent}  {}",
                id.short(),
                format_signature_time(&changeset.author),
                String::from_utf8_lossy(&changeset.author.name),
                subject(&changeset.message)
            )?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn change_json(change: &Change) -> Value {
    let path = String::from_utf8_lossy(&change.path);
    match &change.op {
        ChangeOp::Delete => json!({ "path": path, "op": "delete" }),
        ChangeOp::File { blob, size } => {
            json!({ "path": path, "op": "file", "blob": blob.to_hex(), "size": size })
        }
        ChangeOp::Executable { blob, size } => {
            json!({ "path": path, "op": "executable", "blob": blob.to_hex(), "size": size })
        }
        ChangeOp::Symlink { blob, size } => {
            json!({ "path": path, "op": "symlink", "blob": blob.to_hex(), "size": size })
        }
        ChangeOp::Gitlink { commit } => json!({
            "path": path,
            "op": "gitlink",
            "commit": commit.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>(),
        }),
    }
}

/// How a path changed, as `git diff --name-status` says it: `A` added, `M`
/// modified (content or executable bit), `D` deleted, or `T` for a change
/// between a regular file, a symlink and a gitlink. `old` is what was there
/// before, if a file was.
fn change_status(old: Option<&ChangeOp>, new: &ChangeOp) -> &'static str {
    let class = |op: &ChangeOp| match op {
        ChangeOp::File { .. } | ChangeOp::Executable { .. } => 0,
        ChangeOp::Symlink { .. } => 1,
        ChangeOp::Gitlink { .. } => 2,
        ChangeOp::Delete => 3,
    };
    match (old, new) {
        (_, ChangeOp::Delete) => "D",
        (None, _) => "A",
        (Some(old), new) if class(old) == class(new) => "M",
        (Some(_), _) => "T",
    }
}

/// What the tree `old` has at each path where the tree `new` differs from
/// it: the change list of the diff back from `new` to `old`, one walk over
/// the difference.
fn old_entries(repo: &Repo, old: ObjectId, new: ObjectId) -> Result<HashMap<Vec<u8>, ChangeOp>> {
    Ok(braid_engine::diff(repo.store(), new, old)?
        .into_iter()
        .filter(|change| !matches!(change.op, ChangeOp::Delete))
        .map(|change| (change.path, change.op))
        .collect())
}

/// A change as JSON, with its status and the file it replaced.
fn change_with_status_json(change: &Change, old: Option<&ChangeOp>) -> Value {
    let mut value = change_json(change);
    value["status"] = Value::String(change_status(old, &change.op).to_owned());
    value["old"] = match old {
        Some(op) => {
            let mut old = change_json(&Change {
                path: change.path.clone(),
                op: op.clone(),
            });
            if let Some(fields) = old.as_object_mut() {
                fields.remove("path");
            }
            old
        }
        None => Value::Null,
    };
    value
}

/// Prints changes as `git diff --name-status` does: the status, a tab and
/// the path.
fn write_name_status(
    out: &mut impl Write,
    changes: &[Change],
    olds: &HashMap<Vec<u8>, ChangeOp>,
) -> io::Result<()> {
    for change in changes {
        let status = change_status(olds.get(&change.path), &change.op);
        writeln!(out, "{status}\t{}", String::from_utf8_lossy(&change.path))?;
    }
    Ok(())
}

/// The long help of `diff`.
const DIFF_HELP: &str = "Show the files that differ between two changesets, one line each: a
status, a tab and the path, as `git diff --name-status` prints them. The
status is A (added), M (modified: content or executable bit), D (deleted)
or T (type changed: between a regular file, a symlink and a gitlink). A
directory that became a file, or the other way round, is a D for every
file it had and an A for every file it has.

With --json, diff prints from and to, the two changesets' full ids, and
changes: one per path, sorted by path bytes, with path, status, op (file,
executable, symlink, gitlink or delete) with blob and size, and old, what
the path held before, or null.";

fn diff(cli: &Cli, from: &str, to: &str) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let from = resolve_changeset(&repo, from)?;
    let to = resolve_changeset(&repo, to)?;
    let (from_root, to_root) = (
        read_changeset(&repo, &from)?.root,
        read_changeset(&repo, &to)?.root,
    );
    let changes = braid_engine::diff(repo.store(), from_root, to_root)?;
    let olds = old_entries(&repo, from_root, to_root)?;
    if cli.json {
        print_json(&json!({
            "from": from.to_hex(),
            "to": to.to_hex(),
            "changes": changes
                .iter()
                .map(|change| change_with_status_json(change, olds.get(&change.path)))
                .collect::<Vec<_>>(),
        }))?;
    } else {
        write_name_status(&mut io::stdout().lock(), &changes, &olds)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn show(cli: &Cli, revision: &str) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let id = resolve_changeset(&repo, revision)?;
    let changeset = read_changeset(&repo, &id)?;
    let changes = read_changes(&repo, &changeset)?;
    // The change list is against the first parent: what that has at each
    // path tells added from modified.
    let parent_root = match changeset.parents.first() {
        Some(parent) => read_changeset(&repo, parent)?.root,
        None => braid_engine::EMPTY_TREE,
    };
    let olds = old_entries(&repo, parent_root, changeset.root)?;
    if cli.json {
        let mut value = changeset_json(&id, &changeset);
        value["changes"] = changes
            .iter()
            .map(|change| change_with_status_json(change, olds.get(&change.path)))
            .collect();
        print_json(&value)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut out = io::stdout().lock();
    writeln!(out, "changeset {id}")?;
    for parent in &changeset.parents {
        writeln!(out, "parent    {parent}")?;
    }
    writeln!(out, "root      {}", changeset.root)?;
    for (label, signature) in [
        ("author", &changeset.author),
        ("committer", &changeset.committer),
    ] {
        writeln!(
            out,
            "{label:<9} {} <{}>  {}",
            String::from_utf8_lossy(&signature.name),
            String::from_utf8_lossy(&signature.email),
            format_signature_time(signature)
        )?;
    }
    if let Some(agent) = agent_of(&changeset) {
        writeln!(out, "agent     {agent}")?;
    }
    writeln!(out)?;
    for line in String::from_utf8_lossy(&changeset.message).lines() {
        writeln!(out, "    {line}")?;
    }
    writeln!(out)?;
    write_name_status(&mut out, &changes, &olds)?;
    Ok(ExitCode::SUCCESS)
}

fn git_import(cli: &Cli, path: &Path) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let import = repo.git_import(path, &cli.agent)?;
    let report = &import.report;
    let refs: Vec<Value> = report
        .refs
        .iter()
        .map(|r| {
            json!({
                "git": String::from_utf8_lossy(&r.name),
                "braid": braid_repo::braid_ref_for_git(&r.name),
                "id": r.braid.to_hex(),
            })
        })
        .collect();
    if cli.json {
        print_json(&json!({
            "commits": report.commits,
            "trees": report.trees,
            "raw_trees": report.raw_trees,
            "blobs": report.blobs,
            "tags": report.tags,
            "refs": refs,
            "kept": import.kept.iter().map(|k| json!({
                "name": k.name,
                "braid": k.braid.to_hex(),
                "git": k.git.to_hex(),
                "reason": k.reason.code(),
            })).collect::<Vec<_>>(),
            "operation": import.operation.as_ref().map(|op| op.seq),
        }))?;
    } else {
        println!(
            "Imported {} commits, {} trees, {} blobs, {} tags; {} refs",
            report.commits,
            report.trees,
            report.blobs,
            report.tags,
            report.refs.len()
        );
        for kept in &import.kept {
            let why = match kept.reason {
                braid_repo::KeptReason::Ahead => "braid has work git does not have",
                braid_repo::KeptReason::Diverged => "braid and git have each moved on",
                braid_repo::KeptReason::NotAChangeset => "one side is not a changeset",
                braid_repo::KeptReason::Moved => "it moved while the import ran",
            };
            println!(
                "Kept {} at {}: {why} (git is at {}).",
                kept.name,
                kept.braid.short(),
                kept.git.short()
            );
        }
        if import.operation.is_none() {
            println!("Nothing changed.");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn git_export(cli: &Cli, path: &Path, names: &[String]) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let report = repo.git_export_as(path, names, &cli.agent)?;
    if cli.json {
        print_json(&json!({
            "commits": report.commits,
            "trees": report.trees,
            "blobs": report.blobs,
            "tags": report.tags,
            "refs": report.refs.iter().map(|(name, oid)| json!({
                "name": String::from_utf8_lossy(name),
                "id": oid.to_string(),
            })).collect::<Vec<_>>(),
        }))?;
    } else {
        println!(
            "Wrote {} commits, {} trees, {} blobs, {} tags; {} refs",
            report.commits,
            report.trees,
            report.blobs,
            report.tags,
            report.refs.len()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Formats a signature's time in its own offset: `YYYY-MM-DD HH:MM ±HHMM`.
fn format_signature_time(signature: &Signature) -> String {
    let local = signature
        .seconds
        .saturating_add(signature.offset_minutes.saturating_mul(60));
    let days = local.div_euclid(86_400);
    let of_day = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let sign = if signature.offset_minutes < 0 {
        '-'
    } else {
        '+'
    };
    let offset = signature.offset_minutes.unsigned_abs();
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} {sign}{:02}{:02}",
        of_day / 3600,
        of_day % 3600 / 60,
        offset / 60,
        offset % 60
    )
}

/// Formats milliseconds since the Unix epoch as `YYYY-MM-DD HH:MM:SS UTC`.
fn format_time(time_ms: i64) -> String {
    let seconds = time_ms.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_help_states_the_walk_limit() {
        let limit = format!("more than {}", braid_wc::BEHIND_LIMIT);
        assert!(work::STATUS_HELP.contains(&limit));
    }

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap_or_else(|err| panic!("{args:?}: {err}"))
    }

    /// The long help of the subcommand at `path`.
    fn long_help(path: &[&str]) -> String {
        let mut command = Cli::command();
        command.build();
        let mut current = &mut command;
        for name in path {
            current = current
                .find_subcommand_mut(name)
                .unwrap_or_else(|| panic!("no subcommand {name}"));
        }
        current.render_long_help().to_string()
    }

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn hub_commands_parse() {
        match parse(&[
            "braid", "hub", "clone", "ada/demo", "dir", "--paths", "src", "docs",
        ])
        .command
        {
            Command::Hub {
                action: hub::HubAction::Clone(args),
            } => {
                assert_eq!(args.source, "ada/demo");
                assert_eq!(args.dir, Some(PathBuf::from("dir")));
                assert_eq!(args.paths, ["src", "docs"]);
            }
            other => panic!("{other:?}"),
        }
        match parse(&[
            "braid",
            "hub",
            "publish",
            "demo",
            "--public",
            "--description",
            "A demo",
            "--mirror",
            "https://github.com/ada/demo.git",
        ])
        .command
        {
            Command::Hub {
                action: hub::HubAction::Publish(args),
            } => {
                assert_eq!(args.name.as_deref(), Some("demo"));
                assert!(args.public);
                assert_eq!(args.description.as_deref(), Some("A demo"));
                assert_eq!(
                    args.mirror.as_deref(),
                    Some("https://github.com/ada/demo.git")
                );
            }
            other => panic!("{other:?}"),
        }
        match parse(&["braid", "login", "--hub", "https://hub.example.org"]).command {
            Command::Login(args) => {
                assert_eq!(args.hub.as_deref(), Some("https://hub.example.org"))
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse(&["braid", "logout"]).command,
            Command::Logout(hub::LoginArgs { hub: None })
        ));
        assert!(matches!(
            parse(&["braid", "--json", "hub", "fetch"]).command,
            Command::Hub {
                action: hub::HubAction::Fetch
            }
        ));
        assert!(parse(&["braid", "hub", "status", "--json"]).json);
        match parse(&["braid", "land", "--local", "--onto", "main"]).command {
            Command::Land(args) => assert!(args.local),
            other => panic!("{other:?}"),
        }
        match parse(&["braid", "update", "--local"]).command {
            Command::Update(args) => assert!(args.local),
            other => panic!("{other:?}"),
        }
        match parse(&["braid", "update"]).command {
            Command::Update(args) => assert!(!args.local),
            other => panic!("{other:?}"),
        }
        match parse(&[
            "braid", "change", "create", "--base", "main", "-m", "x", "--land", "main", "--local",
        ])
        .command
        {
            Command::Change {
                action: work::ChangeAction::Create(args),
            } => {
                assert_eq!(args.land.as_deref(), Some("main"));
                assert!(args.local);
            }
            other => panic!("{other:?}"),
        }
        for bad in [
            &["braid", "hub", "clone"][..],
            &["braid", "hub", "clone", "a/b", "--paths"],
            &["braid", "hub", "publish", "a", "b"],
            &["braid", "login", "--hub"],
            &["braid", "hub"],
        ] {
            assert!(Cli::try_parse_from(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn help_texts_cover_the_hub() {
        let top = Cli::command().render_help().to_string();
        for name in ["login", "logout", "hub"] {
            assert!(top.contains(&format!("  {name} ")), "{name}: {top}");
        }
        for path in [&["land"][..], &["update"], &["change", "create"]] {
            let text = long_help(path);
            assert!(text.contains("linked to a hub"), "{path:?}: {text}");
            assert!(text.contains("BRAID_HUB=off"), "{path:?}: {text}");
            assert!(text.contains("--local"), "{path:?}: {text}");
        }
        assert!(long_help(&["land"]).contains("the hub's)"));
        let login = long_help(&["login"]);
        for needed in [
            "device flow",
            "BRAID_TOKEN",
            "BRAID_HUB_URL",
            "hub.json",
            "--hub",
        ] {
            assert!(login.contains(needed), "{needed}: {login}");
        }
        assert!(long_help(&["logout"]).contains("revoke"));
        let publish = long_help(&["hub", "publish"]);
        for needed in [
            "NAME",
            "--public",
            "--mirror",
            "256 MiB",
            "annotated tag",
            ".braid/hub",
        ] {
            assert!(publish.contains(needed), "{needed}: {publish}");
        }
        let clone = long_help(&["hub", "clone"]);
        for needed in ["OWNER/NAME", "--paths", ".braid/work", "--workspace"] {
            assert!(clone.contains(needed), "{needed}: {clone}");
        }
        let fetch = long_help(&["hub", "fetch"]);
        for needed in ["first-parent chain", "remote/hub/", "ahead or diverged"] {
            assert!(fetch.contains(needed), "{needed}: {fetch}");
        }
        let status = long_help(&["hub", "status"]);
        for needed in ["same", "behind", "ahead", "diverged"] {
            assert!(status.contains(needed), "{needed}: {status}");
        }
    }

    #[test]
    fn time_formatting() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_time(1_700_000_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(format_time(951_782_400_000), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_time(-1000), "1969-12-31 23:59:59 UTC");
    }
}
