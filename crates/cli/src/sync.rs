// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid git sync`: the report of `Repo::git_sync`, as text or JSON.

use std::process::ExitCode;

use anyhow::Result;
use braid_format::{EntryKind, ObjectId};
use braid_repo::{Conflict, ConflictKind, HunkSide, SyncOptions, SyncOutcome, SyncReport};
use serde_json::{Value, json};

use crate::{Cli, open_repo, print_json};

/// Exit status when the sync found conflicts.
const CONFLICTS: u8 = 2;

/// What the text report says to do about conflicts: the two sides meet in
/// git, and the guide for agents gives the steps.
const MERGE_IN_GIT: &str = "Merge them in git and push the merge, then sync again: https://braid.elffuss.com/agents.md shows how, under \"Keeping the git remote up to date\".";

pub(crate) fn git_sync(
    cli: &Cli,
    remote: &str,
    branch: &str,
    remote_branch: Option<&str>,
    dry_run: bool,
) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let options = SyncOptions {
        remote_branch: remote_branch.map(str::to_owned),
        dry_run,
        ..SyncOptions::default()
    };
    let report = repo.git_sync(remote, branch, &cli.agent, &options)?;
    if cli.json {
        print_json(&report_json(&report))?;
    } else {
        print_text(&report);
    }
    Ok(match report.outcome {
        SyncOutcome::Conflicts(_) => ExitCode::from(CONFLICTS),
        ref outcome if outcome.is_success() => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

fn hex(id: Option<ObjectId>) -> Value {
    id.map_or(Value::Null, |id| Value::String(id.to_hex()))
}

fn report_json(report: &SyncReport) -> Value {
    let mut value = json!({
        "outcome": report.outcome.code(),
        "dry_run": report.dry_run,
        "label": report.label,
        "branch": report.branch,
        "remote_branch": report.remote_branch,
        "remote_ref": report.remote_ref,
        "before": hex(report.before),
        "after": hex(report.after),
        "fetched": report.fetched.map(|(commit, changeset)| json!({
            "commit": commit.to_hex(),
            "changeset": changeset.to_hex(),
        })),
        "pushed": report.pushed.map(|commit| commit.to_hex()),
        "imported": report.imported,
        "operations": report.operations.iter().map(|op| op.seq).collect::<Vec<_>>(),
    });
    match &report.outcome {
        SyncOutcome::Merged {
            merge,
            merged_paths,
        } => {
            value["merge"] = Value::String(merge.to_hex());
            value["merged_paths"] = merged_paths
                .iter()
                .map(|path| Value::String(String::from_utf8_lossy(path).into_owned()))
                .collect();
        }
        SyncOutcome::Conflicts(conflicts) => {
            value["conflicts"] = conflicts.iter().map(conflict_json).collect();
        }
        SyncOutcome::RemoteMoved { found } => {
            value["found"] = found.map_or(Value::Null, |oid| Value::String(oid.to_hex()));
        }
        SyncOutcome::Rejected { reason } => {
            value["reason"] = Value::String(reason.clone());
        }
        _ => {}
    }
    value
}

fn entry_json(entry: &Option<EntryKind>) -> Value {
    let Some(entry) = entry else {
        return Value::Null;
    };
    match entry {
        EntryKind::File { blob, size } => {
            json!({ "kind": "file", "blob": blob.to_hex(), "size": size })
        }
        EntryKind::Executable { blob, size } => {
            json!({ "kind": "executable", "blob": blob.to_hex(), "size": size })
        }
        EntryKind::Symlink { blob, size } => {
            json!({ "kind": "symlink", "blob": blob.to_hex(), "size": size })
        }
        EntryKind::Tree { tree } => json!({ "kind": "tree", "tree": tree.to_hex() }),
        EntryKind::Gitlink { commit } => json!({
            "kind": "gitlink",
            "commit": commit.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>(),
        }),
    }
}

fn side_json(side: &HunkSide) -> Value {
    json!({
        "start": side.lines.start,
        "end": side.lines.end,
        "text": String::from_utf8_lossy(&side.text),
    })
}

fn conflict_json(conflict: &Conflict) -> Value {
    let mut value = json!({
        "path": String::from_utf8_lossy(&conflict.path),
        "kind": conflict.kind.code(),
        "base": entry_json(&conflict.base),
        "ours": entry_json(&conflict.ours),
        "theirs": entry_json(&conflict.theirs),
    });
    match &conflict.kind {
        ConflictKind::Content { hunks } => {
            value["hunks"] = hunks
                .iter()
                .map(|hunk| {
                    json!({
                        "base": side_json(&hunk.base),
                        "ours": side_json(&hunk.ours),
                        "theirs": side_json(&hunk.theirs),
                    })
                })
                .collect();
        }
        ConflictKind::DirectoryRemoved { directory } => {
            value["directory"] = Value::String(String::from_utf8_lossy(directory).into_owned());
        }
        ConflictKind::Case { existing } => {
            value["existing"] = Value::String(String::from_utf8_lossy(existing).into_owned());
        }
        _ => {}
    }
    value
}

fn short(id: Option<ObjectId>) -> String {
    id.map_or_else(|| "(none)".to_owned(), |id| id.short())
}

fn print_text(report: &SyncReport) {
    let (branch, remote) = (&report.branch, &report.remote_ref);
    let would = |done: &str, planned: &str| {
        if report.dry_run {
            planned.to_owned()
        } else {
            done.to_owned()
        }
    };
    let pushed = report
        .pushed
        .map_or_else(String::new, |commit| commit.to_hex()[..12].to_owned());
    match &report.outcome {
        SyncOutcome::UpToDate => println!(
            "{branch} is up to date with {remote} ({}).",
            short(report.after)
        ),
        SyncOutcome::Created => println!(
            "{} {branch} at {} from {remote}; {} commits imported.",
            would("Created", "Would create"),
            short(report.after),
            report.imported
        ),
        SyncOutcome::FastForwarded => println!(
            "{} {branch} to {}; {} commits imported.",
            would("Fast-forwarded", "Would fast-forward"),
            short(report.after),
            report.imported
        ),
        SyncOutcome::Pushed => println!(
            "{} {branch} ({}) to {} as {pushed}.",
            would("Pushed", "Would push"),
            short(report.after),
            report.remote_branch
        ),
        SyncOutcome::Merged {
            merge,
            merged_paths,
        } => {
            println!(
                "{} {remote} into {branch} as {}, and {} it as {pushed}.",
                would("Merged", "Would merge"),
                merge.short(),
                would("pushed", "push")
            );
            for path in merged_paths {
                println!("merged   {}", String::from_utf8_lossy(path));
            }
        }
        SyncOutcome::Conflicts(conflicts) => {
            println!("Conflicts between {branch} and {remote}; nothing moved, nothing was pushed.");
            for conflict in conflicts {
                println!(
                    "{:<13} {}",
                    conflict.kind.code(),
                    String::from_utf8_lossy(&conflict.path)
                );
            }
            println!("{MERGE_IN_GIT}");
        }
        SyncOutcome::RemoteMoved { found } => {
            eprintln!(
                "braid: error: {} moved after the fetch (it is at {} now); nothing was pushed. Sync again.",
                report.remote_branch,
                found.map_or_else(|| "nothing".to_owned(), |oid| oid.to_hex())
            );
            if report.after != report.before {
                eprintln!("{branch} is at the merge {}.", short(report.after));
            }
        }
        SyncOutcome::Rejected { reason } => {
            eprintln!("braid: error: the remote refused the push: {reason}");
            if report.after != report.before {
                eprintln!("{branch} is at the merge {}.", short(report.after));
            }
        }
        other => println!("{}", other.code()),
    }
}
