// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Putting every branch and tag of a repository on the hub, for `braid hub
//! publish`: everything their changesets reach goes up, less what the hub
//! has, then each ref is landed fast-forward-only: its head alone, which
//! creates a ref the hub does not have, or, for a branch the hub has
//! further down its first-parent chain, the changesets above the hub's
//! head. A tag the hub has elsewhere is left alone: tags never move.
//!
//! The protocol lands changesets only. A ref that names anything else,
//! such as an annotated tag imported from git (a blob holding the tag's
//! bytes), cannot be created on the hub: it is skipped and reported.

use anyhow::Result;
use braid_engine::{Reach, read_changeset};
use braid_format::{Kind, ObjectId, Objects};
use braid_hubproto::api::{LandMode, LandRequest};
use braid_repo::Repo;

use super::fetch::{is_carried, is_tag};
use super::land::{OffBranch, land};
use super::remote::Remote;
use super::transfer::{Sent, upload};

/// Why a ref that names no changeset is not published.
pub(crate) const NOT_A_CHANGESET: &str = "it names an object that is not a changeset, such as an annotated tag, which the hub protocol cannot create";

/// Why a git ref that is neither a branch nor a tag is not published.
pub(crate) const NOT_A_BRANCH: &str = "a git ref that is neither a branch nor a tag";

/// A ref put on the hub, and how its landing went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublishedRef {
    /// The ref.
    pub(crate) name: String,
    /// Its head here.
    pub(crate) id: ObjectId,
    /// The landing's outcome: `fast-forward` (created or moved forward),
    /// `already-applied`, or `refused` when the hub's ref is not on this
    /// one's first-parent chain (for a tag: anywhere else).
    pub(crate) outcome: String,
    /// The hub's head of the ref before: `None` when the landing made it.
    pub(crate) head_before: Option<ObjectId>,
    /// The hub's head of the ref afterwards.
    pub(crate) head: Option<ObjectId>,
}

/// What publishing did.
#[derive(Clone, Debug, Default)]
pub(crate) struct Published {
    /// What was uploaded.
    pub(crate) sent: Sent,
    /// The refs landed, branches first.
    pub(crate) refs: Vec<PublishedRef>,
    /// The refs left out, with why.
    pub(crate) skipped: Vec<(String, &'static str)>,
}

/// Uploads every branch and tag of `repo` to the hub and lands each one.
pub(crate) fn publish_refs(repo: &Repo, remote: &impl Remote, agent: &str) -> Result<Published> {
    let store = repo.store();
    let mut published = Published::default();
    let mut branches = Vec::new();
    let mut tags = Vec::new();
    for (name, id) in store.refs()?.iter() {
        if name.starts_with("braid/") || name.starts_with("remote/") {
            continue;
        }
        if !is_carried(name) {
            published.skipped.push((name.to_owned(), NOT_A_BRANCH));
        } else if !matches!(store.stat(&id)?, Some((Kind::Changeset, _))) {
            published.skipped.push((name.to_owned(), NOT_A_CHANGESET));
        } else if is_tag(name) {
            tags.push((name.to_owned(), id));
        } else {
            branches.push((name.to_owned(), id));
        }
    }
    let mut heads: Vec<ObjectId> = branches.iter().chain(&tags).map(|(_, id)| *id).collect();
    heads.sort();
    heads.dedup();
    published.sent = upload(
        store,
        remote,
        &Reach {
            want: &heads,
            ..Reach::default()
        },
    )?;
    let hub_refs = remote.refs()?.refs;
    let mut history = repo.history();
    for (name, id) in branches.into_iter().chain(tags) {
        let on_hub = hub_refs.get(&name).copied();
        let changes = match on_hub {
            None => vec![id],
            Some(head)
                if head != id
                    && !is_tag(&name)
                    && history.first_parent_chain_contains(id, head, usize::MAX)? =>
            {
                above(store, id, head)?
            }
            // There already, or somewhere it cannot be moved forward from.
            Some(head) => {
                published.refs.push(PublishedRef {
                    name,
                    id,
                    outcome: if head == id {
                        "already-applied"
                    } else {
                        "refused"
                    }
                    .to_owned(),
                    head_before: Some(head),
                    head: Some(head),
                });
                continue;
            }
        };
        // In stacks no longer than the hub takes, each a fast-forward of
        // the one before. A fast-forward-only landing is refused, never off
        // the branch.
        let request = |part: &[ObjectId]| LandRequest {
            reference: name.clone(),
            changes: part.to_vec(),
            mode: LandMode::FfOnly,
            agent: Some(agent.to_owned()),
        };
        let mut parts = changes.chunks(remote.max_ids().max(1));
        let first = parts.next().unwrap_or_default();
        let mut response = land(store, remote, &request(first), OffBranch::Stack)?;
        let head_before = response.head_before;
        for part in parts {
            if response.outcome != "fast-forward" {
                break;
            }
            response = land(store, remote, &request(part), OffBranch::Stack)?;
        }
        published.refs.push(PublishedRef {
            name,
            id,
            outcome: response.outcome,
            head_before,
            head: response.head,
        });
    }
    history.flush()?;
    Ok(published)
}

/// The changesets from `base` (excluded) up to `head` along first parents,
/// oldest first; `base` must be on the chain.
fn above(objects: &impl Objects, head: ObjectId, base: ObjectId) -> Result<Vec<ObjectId>> {
    let mut stack = Vec::new();
    let mut current = head;
    while current != base {
        stack.push(current);
        current = read_changeset(objects, current)?
            .parents
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("{base} is not below {head}"))?;
    }
    stack.reverse();
    Ok(stack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::testing::{FakeHub, Fixture, ada_change, set_ref};
    use braid_engine::reachable;

    #[test]
    fn every_branch_and_tag_goes_up() {
        let local = Fixture::new();
        let root = ada_change(&local.repo, None, &[("a.txt", "a\n")]);
        let two = ada_change(&local.repo, Some(root), &[("dir/b.txt", "b\n")]);
        let side = ada_change(&local.repo, Some(root), &[("side.txt", "s\n")]);
        set_ref(&local.repo, "main", two);
        set_ref(&local.repo, "feature", side);
        set_ref(&local.repo, "tags/v1", root);
        let blob = local.repo.store().put(Kind::Blob, b"tag bytes").unwrap();
        local.repo.store().sync().unwrap();
        set_ref(&local.repo, "tags/annotated", blob);
        set_ref(&local.repo, "git/refs/notes/commits", root);
        set_ref(&local.repo, "remote/origin-0123456789ab/main", root);

        let hub = FakeHub::new();
        let published = publish_refs(&local.repo, &hub, "agent-1").unwrap();
        let names: Vec<&str> = published.refs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["feature", "main", "tags/v1"]);
        assert!(published.refs.iter().all(|r| r.outcome == "fast-forward"));
        let skipped: Vec<&str> = published.skipped.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(skipped, ["git/refs/notes/commits", "tags/annotated"]);

        let refs = hub.repo.store().refs().unwrap();
        assert_eq!(refs.get("main"), Some(two));
        assert_eq!(refs.get("feature"), Some(side));
        assert_eq!(refs.get("tags/v1"), Some(root));
        assert_eq!(refs.get("tags/annotated"), None);
        // The hub holds the whole history, and was sent each object once.
        let all = reachable(
            local.repo.store(),
            &Reach {
                want: &[two, side],
                ..Reach::default()
            },
        )
        .unwrap();
        for object in &all {
            assert!(hub.repo.store().contains(&object.id).unwrap());
        }
        assert_eq!(published.sent.objects, all.len() as u64);

        // Publishing again sends nothing and lands nothing new.
        let lands = hub.called("land");
        let again = publish_refs(&local.repo, &hub, "agent-1").unwrap();
        assert_eq!(again.sent.objects, 0);
        assert!(again.refs.iter().all(|r| r.outcome == "already-applied"));
        assert_eq!(hub.called("land"), lands);

        // A branch that moved on by several changesets goes up as a
        // fast-forward of all of them; one that went elsewhere, and a tag
        // moved here, are refused.
        let three = ada_change(&local.repo, Some(two), &[("c.txt", "c\n")]);
        let four = ada_change(&local.repo, Some(three), &[("d.txt", "d\n")]);
        set_ref(&local.repo, "main", four);
        let elsewhere = ada_change(&local.repo, Some(root), &[("e.txt", "e\n")]);
        set_ref(&local.repo, "feature", elsewhere);
        set_ref(&local.repo, "tags/v1", two);
        let moved = publish_refs(&local.repo, &hub, "agent-1").unwrap();
        let outcome = |name: &str| {
            moved
                .refs
                .iter()
                .find(|r| r.name == name)
                .map(|r| r.outcome.clone())
                .unwrap()
        };
        assert_eq!(outcome("main"), "fast-forward");
        assert_eq!(outcome("feature"), "refused");
        assert_eq!(outcome("tags/v1"), "refused");
        let refs = hub.repo.store().refs().unwrap();
        assert_eq!(refs.get("main"), Some(four));
        assert_eq!(refs.get("feature"), Some(side));
        assert_eq!(refs.get("tags/v1"), Some(root));
    }
}
