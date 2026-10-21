// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Landing in a linked repository: the stack goes to the hub, which lands
//! it with the others, and the workspace then moves onto the head that
//! holds it, as after a local landing.
//!
//! 1. Fetch, so that the local branch is the hub's. A workspace that
//!    cannot land (uncommitted changes, a rebase in progress) says so
//!    first, as a local landing does, before anything goes to the hub.
//! 2. [`Workspace::stack`]: the workspace's changesets the branch lacks,
//!    after any changesets of the local branch below them that the hub's
//!    branch lacks, such as those landed here with `--local`.
//! 3. Upload what the hub lacks of them: the objects the stack reaches
//!    beyond its base and the hub's head, less what `lacks` says it holds.
//! 4. `POST land`, in `rebase` mode, naming the agent.
//! 5. Fetch again, and [`Workspace::adopt`] the new head, when the branch
//!    moved or already had the changes, and the head holds the stack.
//!
//! A failure at any step leaves the local branches where the hub has them
//! and the workspace as it was, and says what to run next: running the
//! same landing again finishes the job, since the hub answers
//! `already-applied` to a stack it has landed.

use anyhow::{Context, Result, bail};
use braid_engine::{Reach, read_changeset};
use braid_format::ObjectId;
use braid_hubproto::api::{LandMode, LandRequest, LandResponse};
use braid_repo::Repo;
use braid_store::Store;
use braid_wc::{Stack, Workspace};

use super::fetch::{Fetched, fetch, holds};
use super::link::Link;
use super::remote::{HubError, Remote};
use super::transfer::{upload, upload_missing};

/// How many times a landing is sent when the hub answers that objects are
/// missing: once, and again after each upload of what it named.
const ATTEMPTS: usize = 3;

/// A workspace's landing on the hub.
#[derive(Debug)]
pub(crate) struct HubLanding {
    /// The branch.
    pub(crate) branch: String,
    /// The hub's answer, or `None` when there was nothing to land.
    pub(crate) response: Option<LandResponse>,
    /// The hub's head of the branch when the landing started.
    pub(crate) head: Option<ObjectId>,
    /// The workspace's base afterwards.
    pub(crate) base: Option<ObjectId>,
    /// The paths of the working copy written or removed.
    pub(crate) updated_paths: Vec<Vec<u8>>,
}

/// Lands the workspace's changesets on `onto` (by default its branch) on
/// the hub, and moves the workspace onto the head that holds them.
pub(crate) fn land_workspace(
    repo: &Repo,
    workspace: &Workspace<'_>,
    remote: &impl Remote,
    link: &Link,
    onto: Option<&str>,
    agent: &str,
) -> Result<HubLanding> {
    workspace.stack(onto)?;
    let fetched = fetch(repo, remote, link, agent, None).context(
        "cannot fetch from the hub, so nothing was landed: run `braid land` again once it answers, or `braid land --local` to land in this repository instead",
    )?;
    let stack = workspace.stack(onto)?;
    let head = fetched.hub_head(&stack.branch);
    let changes = hub_stack(repo, &stack, head, &fetched)?;
    let Some(&last) = changes.last() else {
        // A workspace at the hub's head, as after a landing whose answer
        // was lost, takes it as where its own changesets start, as a
        // landing would have made it.
        if let Some(head) = head
            && stack.base == Some(head)
        {
            workspace.adopt(stack.base, head)?;
        }
        return Ok(HubLanding {
            branch: stack.branch,
            response: None,
            head,
            base: stack.base,
            updated_paths: Vec::new(),
        });
    };
    let most = remote.max_ids();
    if changes.len() > most {
        bail!(
            "this landing is a stack of {} changesets, more than the {most} the hub takes in one landing, so nothing was sent: `braid land --local` lands them here, and `braid hub publish` then sends them to the hub in parts, as fast-forwards",
            changes.len()
        );
    }
    // Changesets below the workspace's own that the hub's branch had once,
    // and no longer has, were taken off it on purpose: they go back only
    // when asked.
    if changes.len() > stack.changes.len()
        && let Some(seen) = once_on_hub(repo, &stack.branch, changes[0])?
    {
        let now = match head {
            Some(head) => format!("is at {} now", head.short()),
            None => "is gone now".to_owned(),
        };
        bail!(
            "the hub's {} no longer has {} of the changesets below this workspace's own: it was at {} when this repository saw it, and {now}, so someone took them off, and landing would put them back; nothing was sent. To put them back on purpose, `braid land --local` lands here and `braid hub publish` sends the branch as it is here; to leave them out, ask whoever took them off",
            stack.branch,
            crate::work::count(changes.len() - stack.changes.len(), "changeset"),
            seen.short()
        );
    }
    let store = repo.store();
    let base = read_changeset(store, changes[0])?.parents.first().copied();
    let have: Vec<ObjectId> = base.into_iter().chain(head).collect();
    send_stack(store, remote, link, last, &have, changes.len()).context(
        "cannot upload the changesets to the hub, so nothing was landed: run `braid land` again, which sends only what the hub still lacks",
    )?;
    let request = LandRequest {
        reference: stack.branch.clone(),
        changes: changes.clone(),
        mode: LandMode::Rebase,
        agent: Some(agent.to_owned()),
    };
    let response = land(store, remote, &request, OffBranch::Stack)
        .map_err(|err| not_landed(err, &format!("its {}", stack.branch), "braid land"))?;
    let (base, updated_paths) = match response.outcome.as_str() {
        "fast-forward" | "rebased" | "already-applied" => {
            let new_head = response.head.with_context(|| {
                format!(
                    "the hub answered {} without a head: nothing moved here; run `braid land` again",
                    response.outcome
                )
            })?;
            fetch(repo, remote, link, agent, Some((&stack.branch, &changes))).map_err(|err| {
                let next = if refused_credentials(&err) {
                    "once signed in again, run `braid land`"
                } else {
                    "run `braid land` again"
                };
                err.context(format!(
                    "{} landed on the hub's {} as {}, but fetching it failed: {next} to bring it here and move the workspace onto it",
                    last.short(),
                    stack.branch,
                    new_head.short()
                ))
            })?;
            // The hub's answer must hold what was sent before the workspace
            // takes it: a stale or confused answer would lose the stack.
            if !holds(store, new_head, &changes)? {
                bail!(
                    "the hub answered {} with {} as the head of its {}, which does not hold {}: nothing moved here; run `braid land` again, and if the hub answers the same, its operator should look at it",
                    response.outcome,
                    new_head.short(),
                    stack.branch,
                    last.short()
                );
            }
            let updated = workspace.adopt(stack.base, new_head).map_err(|err| {
                anyhow::Error::new(err).context(format!(
                    "{} landed on the hub's {} as {}, but the workspace could not be moved onto it; once that is fixed, run `braid land` again",
                    last.short(),
                    stack.branch,
                    new_head.short()
                ))
            })?;
            (Some(new_head), updated)
        }
        "conflicts" => (stack.base, Vec::new()),
        other => bail!(
            "the hub answered `{other}` to the landing on its {}: {} did not land, and nothing moved here",
            stack.branch,
            last.short()
        ),
    };
    Ok(HubLanding {
        branch: stack.branch,
        response: Some(response),
        head,
        base,
        updated_paths,
    })
}

/// Lands one changeset, made without a working copy, on `branch` of the
/// hub, then fetches. Every error names the changeset, and whether it
/// landed; when the answer was lost, it says to run `again`, the command
/// that made the changeset, once more.
pub(crate) fn land_change(
    repo: &Repo,
    remote: &impl Remote,
    link: &Link,
    branch: &str,
    change: ObjectId,
    agent: &str,
    again: &str,
) -> Result<LandResponse> {
    let store = repo.store();
    let base = read_changeset(store, change)?.parents.first().copied();
    let have: Vec<ObjectId> = base.into_iter().collect();
    send_stack(store, remote, link, change, &have, 1).with_context(|| {
        format!("cannot upload changeset {change} to the hub, so it did not land on {branch}")
    })?;
    let request = LandRequest {
        reference: branch.to_owned(),
        changes: vec![change],
        mode: LandMode::Rebase,
        agent: Some(agent.to_owned()),
    };
    let response = land(store, remote, &request, OffBranch::Change { base })
        .map_err(|err| not_landed(err, &format!("its {branch} (changeset {change})"), again))?;
    let landed = match response.outcome.as_str() {
        "fast-forward" | "rebased" | "already-applied" => true,
        "conflicts" => false,
        other => bail!(
            "the hub answered `{other}` to the landing of changeset {change} on its {branch}: it did not land"
        ),
    };
    fetch(repo, remote, link, agent, None).with_context(|| {
        if landed {
            format!(
                "changeset {change} landed on the hub's {branch} as {}, but fetching it failed: `braid hub fetch` brings it in",
                crate::work::short(response.head)
            )
        } else {
            format!(
                "changeset {change} did not land on the hub's {branch}, and fetching what is there failed: `braid hub fetch` brings it in"
            )
        }
    })?;
    Ok(response)
}

/// What a landing whose request failed says, on top of why: when the
/// answer was lost, that the hub may have landed the stack, and that the
/// same command again lands it or finds it landed (`already-applied`);
/// otherwise that nothing landed. The errors of a local landing, which a
/// refusal such as `not_on_branch` becomes, stay as they are.
fn not_landed(err: anyhow::Error, on: &str, again: &str) -> anyhow::Error {
    if err.downcast_ref::<braid_wc::Error>().is_some() {
        return err;
    }
    match err.downcast_ref::<HubError>() {
        Some(hub) if hub.may_have_happened() => err.context(format!(
            "cannot tell whether the hub landed this on {on}, since its answer was lost: run `{again}` again, which lands it or finds it landed already"
        )),
        Some(hub) if hub.unauthorized() || hub.rate_limited() => err.context(
            format!("nothing was landed on {on}: run `{again}` again once that is settled"),
        ),
        _ => err.context(format!("nothing was landed on {on}")),
    }
}

/// Whether `err` comes from a hub that refused the credentials.
fn refused_credentials(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<HubError>()
            .is_some_and(HubError::unauthorized)
    })
}

/// The changesets to land on the hub: the workspace's own, after those of
/// the local branch below them that the hub's branch does not have yet,
/// such as changesets landed here with `--local`, so that the hub can
/// take them in order. When the hub has no such branch, the walk down
/// stops at the first changeset on any of its branches.
pub(crate) fn hub_stack(
    repo: &Repo,
    stack: &Stack,
    head: Option<ObjectId>,
    fetched: &Fetched,
) -> Result<Vec<ObjectId>> {
    let Some(base) = stack.base else {
        return Ok(Vec::new());
    };
    let store = repo.store();
    let heads = match head {
        Some(head) => vec![head],
        None => fetched.hub_branch_heads(),
    };
    let mut below = match stack.changes.first() {
        Some(first) => read_changeset(store, *first)?.parents.first().copied(),
        None => Some(base),
    };
    let mut history = repo.history();
    let mut extra = Vec::new();
    while let Some(id) = below {
        let mut on_hub = false;
        for head in &heads {
            if history.first_parent_chain_contains(*head, id, usize::MAX)? {
                on_hub = true;
                break;
            }
        }
        if on_hub {
            break;
        }
        extra.push(id);
        below = read_changeset(store, id)?.parents.first().copied();
    }
    history.flush()?;
    extra.reverse();
    extra.extend(&stack.changes);
    Ok(extra)
}

/// How far back the op log is read for where the hub's branches were seen.
const SEEN_OPERATIONS: usize = 100_000;

/// Where the hub's `branch` was seen, by a fetch that then saw it elsewhere
/// (`remote/hub/<branch>` before it moved or went, as the op log keeps it),
/// when that head had `oldest` on its first-parent chain: the hub had the
/// changeset there once.
pub(crate) fn once_on_hub(repo: &Repo, branch: &str, oldest: ObjectId) -> Result<Option<ObjectId>> {
    let seen_ref = super::fetch::remote_ref(branch);
    let mut heads = Vec::new();
    let mut listed = std::collections::HashSet::new();
    for operation in repo.store().log(SEEN_OPERATIONS)? {
        for update in &operation.updates {
            if update.name == seen_ref
                && let Some(old) = update.old
                && listed.insert(old)
            {
                heads.push(old);
            }
        }
    }
    let mut history = repo.history();
    let mut found = None;
    for head in heads {
        if head == oldest || history.first_parent_chain_contains(head, oldest, usize::MAX)? {
            found = Some(head);
            break;
        }
    }
    history.flush()?;
    Ok(found)
}

/// Uploads what the hub lacks of a stack of `len` changesets ending at
/// `last`, beyond what `have` reaches. A sparse clone holds only part of
/// each tree, so it walks its paths instead, and the hub's `lacks` sorts
/// out what it has.
pub(crate) fn send_stack(
    store: &Store,
    remote: &impl Remote,
    link: &Link,
    last: ObjectId,
    have: &[ObjectId],
    len: usize,
) -> Result<()> {
    let prefixes = link.prefixes();
    let want = [last];
    let reach = Reach {
        want: &want,
        have: if prefixes.is_some() { &[] } else { have },
        history: Some(u32::try_from(len.saturating_sub(1)).unwrap_or(u32::MAX)),
        paths: prefixes.as_deref(),
        max_objects: None,
    };
    upload(store, remote, &reach)?;
    Ok(())
}

/// What landed, for the error a refusal with `not_on_branch` becomes: the
/// one the same landing gives in a local repository.
#[derive(Clone, Copy, Debug)]
pub(crate) enum OffBranch {
    /// A workspace's stack, as `braid land` lands it.
    Stack,
    /// One changeset made without a working copy, on `base`, as `braid
    /// change create --land` lands it.
    Change {
        /// The changeset's first parent.
        base: Option<ObjectId>,
    },
}

/// Sends a landing, uploading what the hub says is missing and sending it
/// again; `not_on_branch` is the error a local landing gives.
pub(crate) fn land(
    store: &Store,
    remote: &impl Remote,
    request: &LandRequest,
    off_branch: OffBranch,
) -> Result<LandResponse> {
    let mut attempt = 1;
    loop {
        match remote.land(request) {
            Ok(response) => return Ok(response),
            Err(err) if err.code() == Some("missing") && attempt < ATTEMPTS => {
                let ids = err.missing_ids().to_vec();
                if ids.is_empty() {
                    return Err(err.into());
                }
                upload_missing(store, remote, &ids)?;
                attempt += 1;
            }
            Err(err) if err.code() == Some("not_on_branch") => {
                let branch = request.reference.clone();
                return Err(match off_branch {
                    OffBranch::Stack => braid_wc::Error::NotOnBranch { branch },
                    OffBranch::Change { base } => {
                        // The local error names the head, which the
                        // refusal gives; a hub that leaves it out is asked.
                        let given = match &err {
                            HubError::Api { body, .. } => {
                                body.head.as_deref().and_then(|head| head.parse().ok())
                            }
                            _ => None,
                        };
                        let head = given.or_else(|| {
                            remote
                                .refs()
                                .ok()
                                .and_then(|refs| refs.refs.get(&branch).copied())
                        });
                        braid_wc::Error::from(braid_engine::Error::BaseNotOnBranch { base, head })
                    }
                }
                .into());
            }
            Err(err) => return Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::fetch::fetch;
    use crate::hub::testing::{FakeHub, Fixture, ada, ada_change, set_ref};
    use braid_wc::CommitRequest;
    use std::fs;

    /// A linked repository holding the hub's `main`, with a workspace on it.
    struct Setup {
        hub: FakeHub,
        local: Fixture,
    }

    impl Setup {
        fn new() -> Setup {
            let hub = FakeHub::new();
            let root = ada_change(
                &hub.repo,
                None,
                &[("a.txt", "one\ntwo\nthree\n"), ("b.txt", "b\n")],
            );
            set_ref(&hub.repo, "main", root);
            let local = Fixture::linked(&hub);
            fetch(&local.repo, &hub, &local.link, "tester", None).unwrap();
            Setup { hub, local }
        }

        fn workspace(&self, name: &str) -> Workspace<'_> {
            let repo = &self.local.repo;
            let head = repo.store().refs().unwrap().get("main");
            let dir = Workspace::default_dir(repo, name);
            Workspace::create(repo, name, &dir, head, "main").unwrap().0
        }

        fn hub_main(&self) -> Option<ObjectId> {
            self.hub.repo.store().refs().unwrap().get("main")
        }

        fn local_main(&self) -> Option<ObjectId> {
            self.local.repo.store().refs().unwrap().get("main")
        }

        fn land(&self, ws: &Workspace<'_>) -> Result<HubLanding> {
            land_workspace(
                &self.local.repo,
                ws,
                &self.hub,
                &self.local.link,
                None,
                "agent-1",
            )
        }
    }

    fn edit_and_commit(ws: &Workspace<'_>, path: &str, content: &str) -> ObjectId {
        fs::write(ws.dir().join(path), content).unwrap();
        ws.commit(&CommitRequest {
            message: Some(format!("edit {path}").into_bytes()),
            paths: Vec::new(),
            author: None,
            committer: ada(),
            agent: "agent-1".to_owned(),
            force: false,
        })
        .unwrap()
        .changeset
        .unwrap()
    }

    #[test]
    fn a_fast_forward_moves_the_hub_the_branch_and_the_workspace() {
        let s = Setup::new();
        let ws = s.workspace("w1");
        let mine = edit_and_commit(&ws, "c.txt", "c\n");
        let landing = s.land(&ws).unwrap();
        let response = landing.response.as_ref().unwrap();
        assert_eq!(response.outcome, "fast-forward");
        assert_eq!(response.head, Some(mine));
        assert_eq!(s.hub_main(), Some(mine));
        assert_eq!(s.local_main(), Some(mine));
        assert_eq!(landing.base, Some(mine));
        assert_eq!(ws.state().unwrap().base, Some(mine));
        assert_eq!(ws.state().unwrap().upstream, Some(mine));
        // The agent label reached the hub's op log.
        assert_eq!(s.hub.repo.store().log(1).unwrap()[0].agent, "agent-1");
        // Landing again finds nothing to land, and asks the hub nothing.
        let lands = s.hub.called("land");
        let again = s.land(&ws).unwrap();
        assert!(again.response.is_none());
        assert_eq!(again.head, Some(mine));
        assert_eq!(s.hub.called("land"), lands);
    }

    #[test]
    fn a_rebase_on_the_hub_brings_the_others_work_into_the_workspace() {
        let s = Setup::new();
        let ws = s.workspace("w1");
        let mine = edit_and_commit(&ws, "c.txt", "c\n");
        // Someone else lands first, on the hub.
        let before = s.hub_main().unwrap();
        let theirs = ada_change(&s.hub.repo, Some(before), &[("d.txt", "theirs\n")]);
        set_ref(&s.hub.repo, "main", theirs);

        let landing = s.land(&ws).unwrap();
        let response = landing.response.as_ref().unwrap();
        assert_eq!(response.outcome, "rebased");
        assert_eq!(response.head_before, Some(theirs));
        let head = response.head.unwrap();
        assert_ne!(head, mine);
        assert_eq!(response.landed[0].from, mine);
        assert_eq!(response.landed[0].to, Some(head));
        assert_eq!(s.hub_main(), Some(head));
        assert_eq!(s.local_main(), Some(head));
        assert_eq!(ws.state().unwrap().base, Some(head));
        assert_eq!(landing.updated_paths, [b"d.txt".to_vec()]);
        assert_eq!(
            fs::read_to_string(ws.dir().join("d.txt")).unwrap(),
            "theirs\n"
        );
        assert_eq!(fs::read_to_string(ws.dir().join("c.txt")).unwrap(), "c\n");
    }

    #[test]
    fn conflicts_leave_everything_where_it_was() {
        let s = Setup::new();
        let ws = s.workspace("w1");
        let mine = edit_and_commit(&ws, "a.txt", "one\nmine\nthree\n");
        let before = s.hub_main().unwrap();
        let theirs = ada_change(
            &s.hub.repo,
            Some(before),
            &[("a.txt", "one\ntheirs\nthree\n")],
        );
        set_ref(&s.hub.repo, "main", theirs);

        let landing = s.land(&ws).unwrap();
        let response = landing.response.as_ref().unwrap();
        assert_eq!(response.outcome, "conflicts");
        assert_eq!(response.change, Some(mine));
        assert_eq!(response.conflicts.len(), 1);
        assert_eq!(response.conflicts[0]["path"], "a.txt");
        assert_eq!(response.conflicts[0]["kind"], "CONTENT");
        assert_eq!(s.hub_main(), Some(theirs));
        assert_eq!(landing.base, Some(mine));
        assert!(landing.updated_paths.is_empty());
        assert_eq!(ws.state().unwrap().base, Some(mine));
        // The fetch before the landing brought the hub's head in.
        assert_eq!(s.local_main(), Some(theirs));
    }

    #[test]
    fn what_the_hub_finds_missing_is_sent_and_the_landing_sent_again() {
        let s = Setup::new();
        let ws = s.workspace("w1");
        let mine = edit_and_commit(&ws, "c.txt", "c\n");
        s.hub.forgetful.set(true);
        let landing = s.land(&ws).unwrap();
        assert_eq!(landing.response.unwrap().outcome, "fast-forward");
        assert_eq!(s.hub.called("land"), 2);
        assert_eq!(s.hub_main(), Some(mine));
    }

    #[test]
    fn a_workspace_that_cannot_land_says_so_before_asking_the_hub() {
        let s = Setup::new();
        let ws = s.workspace("w1");
        fs::write(ws.dir().join("c.txt"), "not committed\n").unwrap();
        let calls = s.hub.calls.borrow().len();
        let err = s.land(&ws).unwrap_err().to_string();
        assert!(err.contains("uncommitted changes"), "{err}");
        assert_eq!(s.hub.calls.borrow().len(), calls);
    }

    #[test]
    fn not_on_branch_is_the_local_error() {
        let s = Setup::new();
        let ws = s.workspace("w1");
        edit_and_commit(&ws, "c.txt", "c\n");
        s.hub.off_branch.set(true);
        let err = s.land(&ws).unwrap_err();
        let local = braid_wc::Error::NotOnBranch {
            branch: "main".to_owned(),
        };
        assert_eq!(err.to_string(), local.to_string());
        assert!(matches!(
            err.downcast_ref::<braid_wc::Error>(),
            Some(braid_wc::Error::NotOnBranch { .. })
        ));
    }

    #[test]
    fn changesets_landed_here_with_local_go_up_first() {
        let s = Setup::new();
        // One workspace lands locally, as --local does.
        let w1 = s.workspace("w1");
        let local_only = edit_and_commit(&w1, "local.txt", "l\n");
        let report = w1.land(None, "agent-1", &ada()).unwrap();
        assert_eq!(report.head(), Some(local_only));
        // Another workspace, on top of it, lands on the hub.
        let w2 = s.workspace("w2");
        let mine = edit_and_commit(&w2, "c.txt", "c\n");
        let landing = s.land(&w2).unwrap();
        let response = landing.response.unwrap();
        assert_eq!(response.outcome, "fast-forward");
        let froms: Vec<ObjectId> = response.landed.iter().map(|p| p.from).collect();
        assert_eq!(froms, [local_only, mine]);
        assert_eq!(s.hub_main(), Some(mine));
        assert_eq!(s.local_main(), Some(mine));
    }

    #[test]
    fn a_change_off_the_branch_gives_the_local_error() {
        let s = Setup::new();
        let base = s.local_main().unwrap();
        let change = ada_change(&s.local.repo, Some(base), &[("new.txt", "n\n")]);
        s.hub.off_branch.set(true);
        let err = land_change(
            &s.local.repo,
            &s.hub,
            &s.local.link,
            "main",
            change,
            "agent-1",
            "braid change create",
        )
        .unwrap_err();
        let local = braid_engine::Error::BaseNotOnBranch {
            base: Some(base),
            head: s.hub_main(),
        };
        assert_eq!(err.to_string(), local.to_string());
    }

    #[test]
    fn a_change_lands_as_a_stack_of_one() {
        let s = Setup::new();
        let base = s.local_main().unwrap();
        let change = ada_change(&s.local.repo, Some(base), &[("new.txt", "n\n")]);
        let response = land_change(
            &s.local.repo,
            &s.hub,
            &s.local.link,
            "main",
            change,
            "agent-1",
            "braid change create",
        )
        .unwrap();
        assert_eq!(response.outcome, "fast-forward");
        assert_eq!(s.hub_main(), Some(change));
        assert_eq!(s.local_main(), Some(change));
    }

    #[test]
    fn a_revert_lands_on_the_hub_rebased_over_later_work() {
        let s = Setup::new();
        let base = s.local_main().unwrap();
        let culprit = ada_change(&s.local.repo, Some(base), &[("b.txt", "bad\n")]);
        let later = ada_change(&s.local.repo, Some(culprit), &[("c.txt", "c\n")]);
        for change in [culprit, later] {
            let response = land_change(
                &s.local.repo,
                &s.hub,
                &s.local.link,
                "main",
                change,
                "agent-1",
                "braid change create",
            )
            .unwrap();
            assert_eq!(response.outcome, "fast-forward");
        }
        let revert = braid_wc::create_revert(
            s.local.repo.store(),
            &braid_wc::RevertRequest {
                culprit,
                allow_merge: false,
                author: ada(),
                agent: "verifier".to_owned(),
            },
        )
        .unwrap();
        let response = land_change(
            &s.local.repo,
            &s.hub,
            &s.local.link,
            "main",
            revert,
            "verifier",
            "braid revert",
        )
        .unwrap();
        assert_eq!(response.outcome, "rebased");
        let head = s.hub_main().unwrap();
        assert_eq!(s.local_main(), Some(head));
        let store = s.local.repo.store();
        let root = braid_engine::read_changeset(store, head).unwrap().root;
        let file = |path: &str| match braid_engine::entry_at(store, root, path.as_bytes()) {
            Ok(Some(braid_format::EntryKind::File { blob, .. })) => {
                braid_engine::read_blob(store, blob).unwrap()
            }
            other => panic!("{path}: {other:?}"),
        };
        assert_eq!(file("b.txt"), b"b\n");
        assert_eq!(file("c.txt"), b"c\n");
    }
}
