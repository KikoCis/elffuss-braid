// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid`, the command-line tool.

mod sync;
mod work;

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use braid_format::changeset::decode_change_list;
use braid_format::{
    Change, ChangeOp, Changes, Changeset, IdHasher, Kind, ObjectId, Objects, Signature,
};
use braid_repo::Repo;
use braid_store::{FsckReport, Operation, RefUpdate};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

/// Version control for many agents landing changes at once.
#[derive(Debug, Parser)]
#[command(name = "braid", version, about, long_about = None)]
struct Cli {
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,

    /// Repository root (default: the current directory or its nearest parent
    /// holding a `.braid` directory).
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

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a repository.
    Init {
        /// Directory to create it in (default: the current directory).
        dir: Option<PathBuf>,
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
    /// Write an object's raw payload to standard output.
    CatObject {
        /// A ref, an object id, or a short id (6 hex digits or more).
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
    /// Show history, following first parents.
    Log {
        /// Where to start: a ref, a changeset id, or a short id (6 hex
        /// digits or more).
        #[arg(default_value = "main")]
        revision: String,
        /// How many changesets to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Show one changeset: parents, root, signatures, message, changed paths.
    Show {
        /// A ref, a changeset id, or a short id (6 hex digits or more).
        #[arg(default_value = "main")]
        revision: String,
    },
    /// Import from and export to git repositories.
    Git {
        #[command(subcommand)]
        action: GitAction,
    },
    /// Check every record, index, op log entry and ref of the store.
    Fsck,
    /// Create, list and remove workspaces.
    Workspace {
        #[command(subcommand)]
        action: work::WorkspaceAction,
    },
    /// Show how the workspace differs from its base and its branch.
    Status,
    /// Record the workspace's changes as a changeset on its base.
    Commit(work::CommitArgs),
    /// Land the workspace's changesets on its branch (exit status 2 on
    /// conflicts).
    #[command(long_about = work::LAND_HELP)]
    Land(work::LandArgs),
    /// Bring the workspace onto its branch's head, rebasing its own
    /// changesets (exit status 2 on conflicts).
    #[command(long_about = work::UPDATE_HELP)]
    Update(work::IdentityArgs),
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
    /// it (merging first when both moved) as a fast-forward with a lease.
    /// Never force-pushes. Exits with 2 on conflicts and 1 when the push is
    /// refused.
    Sync {
        /// The git remote: a URL or a path.
        remote: String,
        /// The braid branch.
        #[arg(long, default_value = "main")]
        branch: String,
        /// The branch of the remote (default: the braid branch's name).
        #[arg(long, value_name = "NAME")]
        remote_branch: Option<String>,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("braid: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode> {
    match &cli.command {
        Command::Init { dir } => init(cli, dir.as_deref()),
        Command::HashObject { write, paths } => hash_object(cli, paths, *write),
        Command::CatObject { revision } => cat_object(cli, revision),
        Command::Refs => refs(cli),
        Command::Ref { action } => ref_action(cli, action),
        Command::Op {
            action: OpAction::Log { limit },
        } => op_log(cli, *limit),
        Command::Undo => undo(cli),
        Command::Log { revision, limit } => log(cli, revision, *limit),
        Command::Show { revision } => show(cli, revision),
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
                    dry_run,
                },
        } => sync::git_sync(cli, remote, branch, remote_branch.as_deref(), *dry_run),
        Command::Fsck => fsck(cli),
        Command::Workspace { action } => work::workspace(cli, action),
        Command::Status => work::status(cli),
        Command::Commit(args) => work::commit(cli, args),
        Command::Land(args) => work::land(cli, args),
        Command::Update(args) => work::update(cli, args),
        Command::Resolve { paths } => work::resolve(cli, paths),
        Command::Track { paths } => work::track(cli, paths),
        Command::Change { action } => work::change(cli, action),
    }
}

fn open_repo(cli: &Cli) -> Result<Repo> {
    let repo = match &cli.repo {
        Some(dir) => Repo::open(dir),
        None => Repo::discover(&std::env::current_dir()?),
    };
    Ok(repo?)
}

fn print_json(value: &Value) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

fn init(cli: &Cli, dir: Option<&Path>) -> Result<ExitCode> {
    let dir = match dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let repo = Repo::init(&dir)?;
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
            // Storing needs the whole payload; the store compresses it.
            Some(repo) => {
                let bytes = read_input(path)?;
                let id = repo.store().put(Kind::Blob, &bytes)?;
                (id, bytes.len() as u64)
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

fn read_input(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if path.as_os_str() == "-" {
        io::stdin()
            .lock()
            .read_to_end(&mut bytes)
            .context("cannot read standard input")?;
    } else {
        File::open(path)
            .with_context(|| format!("cannot open {}", path.display()))?
            .read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {}", path.display()))?;
    }
    Ok(bytes)
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

fn cat_object(cli: &Cli, revision: &str) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let id = repo.resolve(revision)?;
    let Some((kind, payload)) = repo.store().get(&id)? else {
        bail!("object {id} is not in the store");
    };
    if cli.json {
        print_json(&json!({
            "id": id.to_hex(),
            "kind": kind.name(),
            "size": payload.len(),
        }))?;
    } else {
        io::stdout().lock().write_all(&payload)?;
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

fn log(cli: &Cli, revision: &str, limit: usize) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let mut next = Some(resolve_changeset(&repo, revision)?);
    let mut entries = Vec::new();
    while let Some(id) = next {
        if entries.len() == limit {
            break;
        }
        let changeset = read_changeset(&repo, &id)?;
        next = changeset.parents.first().copied();
        entries.push((id, changeset));
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

fn show(cli: &Cli, revision: &str) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let id = resolve_changeset(&repo, revision)?;
    let changeset = read_changeset(&repo, &id)?;
    let changes = read_changes(&repo, &changeset)?;
    if cli.json {
        let mut value = changeset_json(&id, &changeset);
        value["changes"] = Value::Array(changes.iter().map(change_json).collect());
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
    for change in &changes {
        let op = match change.op {
            ChangeOp::Delete => "delete",
            ChangeOp::File { .. } => "file",
            ChangeOp::Executable { .. } => "exec",
            ChangeOp::Symlink { .. } => "link",
            ChangeOp::Gitlink { .. } => "gitlink",
        };
        writeln!(out, "{op:<8} {}", String::from_utf8_lossy(&change.path))?;
    }
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
    fn time_formatting() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_time(1_700_000_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(format_time(951_782_400_000), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_time(-1000), "1969-12-31 23:59:59 UTC");
    }
}
