// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The commands answered with derived data: `log -- <path>`, `blame` and
//! `derive`.

use std::collections::HashMap;
use std::io::{self, Write};
use std::process::ExitCode;

use anyhow::{Result, bail};
use braid_format::{Changeset, ObjectId, validate_path};
use braid_repo::Repo;
use serde_json::{Value, json};

use crate::{
    Cli, agent_of, format_signature_time, open_repo, print_json, read_changeset, resolve_changeset,
};

/// The long help of `blame`.
pub(crate) const BLAME_HELP: &str =
    "Show, for each line of a file, the changeset that last changed it,
following first parents from the revision: a merge on the first-parent
chain owns the lines it brought in. Renames are not followed: the lines of
a file are new where the path appeared.

Each line prints the changeset's short id, its agent (or author when it
names no agent), the author's date, the line number and the line. With
--json, blame prints changeset, the full id of the revision, path, and
lines: one per line, with line (counted from 1), changeset, agent (or
null), author, time and content.

The path is given from the repository's root. Blame data is derived and
kept in the store, so blaming the file again, at this revision or a later
one, reuses it.";

/// The long help of `derive`.
pub(crate) const DERIVE_HELP: &str =
    "Compute the derived data of every changeset reachable from the revisions
(main when none is given, every ref with --all): its generation number and
first-parent skip pointer, which make ancestry queries and merge bases
cheap, and its changed-path filter, which lets log -- <path> and blame skip
changesets. Blame data, per file, is computed by blame itself.

Derived data is computed when a command first needs it, so this is only a
way to pay that cost ahead of time, after a large import for example.
With --json, derive prints changesets, how many are reachable, and
computed, how many records were missing and computed.";

/// A path given on the command line, as the repository names it: from the
/// root, without `./` in front or `/` at the end. `.` and the empty path are
/// the root.
pub(crate) fn repo_path(text: &str) -> Result<Vec<u8>> {
    let mut path = text.trim_end_matches('/');
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.trim_start_matches('/');
    }
    if path.is_empty() || path == "." {
        return Ok(Vec::new());
    }
    if let Err(err) = validate_path(path.as_bytes()) {
        bail!("{text:?} is not a path in the repository: {err}");
    }
    Ok(path.as_bytes().to_vec())
}

/// The changesets on the first-parent chain of `head` that touch one of
/// `paths`, newest first, at most `limit`.
pub(crate) fn path_log(
    repo: &Repo,
    head: ObjectId,
    paths: &[String],
    limit: usize,
) -> Result<Vec<ObjectId>> {
    let paths = paths
        .iter()
        .map(|path| repo_path(path))
        .collect::<Result<Vec<_>>>()?;
    let paths: Vec<&[u8]> = paths.iter().map(Vec::as_slice).collect();
    let mut history = repo.history();
    let found = history.path_log(head, &paths, limit)?;
    history.flush()?;
    Ok(found)
}

pub(crate) fn blame(cli: &Cli, revision: &str, path: &str) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let at = resolve_changeset(&repo, revision)?;
    let file = repo_path(path)?;
    let mut history = repo.history();
    let blame = history.blame(at, &file)?;
    history.flush()?;
    let Some(blame) = blame else {
        bail!("{path:?} is not a file at {}", at.short());
    };

    // Each changeset that owns lines, read once.
    let mut changesets: HashMap<ObjectId, Changeset> = HashMap::new();
    for origin in &blame.origins {
        if !changesets.contains_key(origin) {
            changesets.insert(*origin, read_changeset(&repo, origin)?);
        }
    }
    let who = |id: &ObjectId| {
        let changeset = &changesets[id];
        agent_of(changeset)
            .unwrap_or_else(|| String::from_utf8_lossy(&changeset.author.name).into_owned())
    };
    let text = |line: &[u8]| {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        String::from_utf8_lossy(line).into_owned()
    };

    if cli.json {
        let lines: Vec<Value> = blame
            .lines()
            .enumerate()
            .map(|(n, (line, origin))| {
                let changeset = &changesets[&origin];
                json!({
                    "line": n + 1,
                    "changeset": origin.to_hex(),
                    "agent": agent_of(changeset),
                    "author": String::from_utf8_lossy(&changeset.author.name),
                    "time": format_signature_time(&changeset.author),
                    "content": text(line),
                })
            })
            .collect();
        print_json(&json!({
            "changeset": at.to_hex(),
            "path": String::from_utf8_lossy(&file),
            "lines": lines,
        }))?;
        return Ok(ExitCode::SUCCESS);
    }

    let width = blame
        .origins
        .iter()
        .map(|origin| who(origin).chars().count())
        .max()
        .unwrap_or(0);
    let digits = blame.origins.len().to_string().len();
    let mut out = io::stdout().lock();
    for (n, (line, origin)) in blame.lines().enumerate() {
        writeln!(
            out,
            "{}  {:<width$}  {}  {:>digits$}  {}",
            origin.short(),
            who(&origin),
            format_signature_time(&changesets[&origin].author),
            n + 1,
            text(line),
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn derive(cli: &Cli, all: bool, revisions: &[String]) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let mut heads = Vec::new();
    for revision in revisions {
        heads.push(resolve_changeset(&repo, revision)?);
    }
    if all {
        heads.extend(repo.ref_changesets()?);
    } else if revisions.is_empty() {
        heads.push(resolve_changeset(&repo, "main")?);
    }
    let mut history = repo.history();
    let report = history.derive(&heads)?;
    if cli.json {
        print_json(&json!({
            "changesets": report.changesets,
            "computed": report.computed,
        }))?;
    } else {
        println!(
            "{} changesets; {} derived records computed",
            report.changesets, report.computed
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_taken_from_the_root() {
        for (text, path) in [
            ("src/lib.rs", "src/lib.rs"),
            ("src/", "src"),
            ("./src//", "src"),
            ("././a", "a"),
            (".", ""),
            ("", ""),
            ("./", ""),
        ] {
            assert_eq!(repo_path(text).unwrap(), path.as_bytes(), "{text:?}");
        }
        for bad in ["/abs", "a//b", "a/../b", ".."] {
            assert!(repo_path(bad).is_err(), "{bad:?}");
        }
    }
}
