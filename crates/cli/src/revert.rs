// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `revert`: undo a changeset that landed with a new changeset, and land
//! that.

use std::process::ExitCode;

use anyhow::{Result, bail};
use braid_format::ObjectId;
use braid_repo::Repo;
use braid_wc::{Identity, RevertRequest};
use clap::Args;
use serde_json::json;

use crate::hub::{self, link::Link};
use crate::work::{IdentityArgs, Report, branches, identity, print_conflicts};
use crate::{Cli, open_repo, print_json, resolve_changeset};

/// The long help of `revert`.
pub(crate) const REVERT_HELP: &str =
    "Undo a changeset that landed on a branch: make a new changeset on it
whose changes put every file back as its first parent had it, modes,
symlinks and submodules included, and land that on the branch. The message
is `Revert \"<subject>\"`, and the changeset has a `reverts` field naming
the one it undoes.

Landing works as for any change: when the branch has moved on since, the
revert is rebased onto its head path by path, so later changes to other
lines and files stay. When a later change touched the same lines, the
landing stops with conflicts, the branch does not move and the exit status
is 2: undo the change by hand in a workspace instead.

A changeset without parents cannot be reverted, nor a merge unless
--mainline 1 says to undo what it brought in against its first parent;
--mainline is refused for a changeset that is not a merge. --branch must
name a branch, not a ref in one of braid's own namespaces.
--no-land only makes the changeset and prints its id. In a repository
linked to a hub, the revert lands on the hub, then fetches; --local, or
BRAID_HUB=off, lands here.

With --json it prints revert, the new changeset's id; reverts, the one it
undoes; branch; and land, the report of the landing as `braid land --help`
describes it, or null with --no-land.";

#[derive(Debug, Args)]
pub(crate) struct RevertArgs {
    /// The changeset to undo: a ref, an id, or a short id (6 hex digits or
    /// more).
    revision: String,
    /// The branch to land the revert on (default: main, master, or the
    /// repository's only branch).
    #[arg(long, value_name = "BRANCH")]
    branch: Option<String>,
    /// Only make the revert and print its id; do not land it.
    #[arg(long)]
    no_land: bool,
    /// Revert a merge against this parent: only 1, the first, is supported.
    #[arg(long, value_name = "PARENT", value_parser = clap::value_parser!(u8).range(1..=1))]
    mainline: Option<u8>,
    #[command(flatten)]
    identity: IdentityArgs,
    /// Land in this repository even if it is linked to a hub.
    #[arg(long)]
    local: bool,
}

/// The branch a command acts on: the one given, or else the repository's
/// main branch: `main`, `master`, or its only branch. A name in one of
/// braid's own namespaces, such as a green ref, is refused.
pub(crate) fn branch_or_main(repo: &Repo, given: Option<&str>) -> Result<String> {
    let branch = match given {
        Some(branch) => branch.to_owned(),
        None => crate::agents::main_branch(&branches(&repo.store().refs()?))?,
    };
    if !braid_repo::is_branch(&branch) {
        bail!(
            "{branch:?} is in one of braid's own namespaces (braid/, git/, remote/, tags/), not a branch"
        );
    }
    Ok(branch)
}

/// Refuses to revert `culprit` on `branch` unless it is on the branch's
/// first-parent chain, as landing requires: anything else would be stored
/// first and refused after.
pub(crate) fn check_on_branch(repo: &Repo, branch: &str, culprit: ObjectId) -> Result<()> {
    let Some(head) = repo.store().refs()?.get(branch) else {
        bail!("there is no branch {branch:?} to land the revert on");
    };
    if !braid_engine::first_parent_chain_contains(repo.store(), head, culprit, usize::MAX)? {
        bail!(
            "changeset {} is not on {branch}'s first-parent chain: only what landed on {branch} can be reverted there",
            culprit.short()
        );
    }
    Ok(())
}

/// Lands `change`, a revert made with [`braid_wc::create_revert`], on
/// `branch`: on the hub of `link`, then fetching, or in this repository.
pub(crate) fn land_revert(
    repo: &Repo,
    link: Option<&Link>,
    branch: &str,
    change: ObjectId,
    agent: &str,
    committer: &Identity,
) -> Result<Report> {
    Ok(match link {
        Some(link) => {
            let hub = hub::connect(link)?;
            let remote = hub.repo(link.repo.clone());
            let response =
                hub::land::land_change(repo, &remote, link, branch, change, agent, "braid revert")?;
            Report::hub(&response)
        }
        None => Report::landing(&braid_wc::land_change(
            repo.store(),
            branch,
            change,
            agent,
            committer,
        )?),
    })
}

pub(crate) fn revert(cli: &Cli, args: &RevertArgs) -> Result<ExitCode> {
    let repo = open_repo(cli)?;
    let culprit = resolve_changeset(&repo, &args.revision)?;
    let branch = branch_or_main(&repo, args.branch.as_deref())?;
    let author = identity(&args.identity, &repo)?;
    let link = if args.no_land {
        None
    } else {
        hub::linked(&repo, args.local)?
    };
    if !args.no_land && link.is_none() {
        check_on_branch(&repo, &branch, culprit)?;
    }
    let change = braid_wc::create_revert(
        repo.store(),
        &RevertRequest {
            culprit,
            mainline: args.mainline.is_some(),
            author: author.clone(),
            agent: cli.agent.clone(),
        },
    )?;
    let report = if args.no_land {
        None
    } else {
        Some(land_revert(
            &repo,
            link.as_ref(),
            &branch,
            change,
            &cli.agent,
            &author,
        )?)
    };
    let code = report.as_ref().map_or(ExitCode::SUCCESS, Report::exit_code);
    if cli.json {
        print_json(&json!({
            "revert": change.to_hex(),
            "reverts": culprit.to_hex(),
            "branch": branch,
            "land": report.as_ref().map(Report::json),
        }))?;
        return Ok(code);
    }
    match &report {
        None => println!(
            "Made {}, the revert of {}; it has not landed",
            change.short(),
            culprit.short()
        ),
        Some(report) if report.conflicted() => {
            println!(
                "The revert of {} conflicts with what landed on {branch} since; {branch} did not move:",
                culprit.short()
            );
            print_conflicts(&report.conflicts);
            println!("Undo the change by hand in a workspace instead.");
        }
        Some(report) if report.outcome == "already-applied" => println!(
            "{branch} has what the revert of {} would change already: nothing landed",
            culprit.short()
        ),
        Some(report) => {
            println!("Reverted {} with {}", culprit.short(), change.short());
            report.print_landing(cli);
        }
    }
    Ok(code)
}
