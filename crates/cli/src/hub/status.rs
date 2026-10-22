// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! How each branch of a linked repository compares with the hub's, for
//! `braid hub status`, and how the hub verifies it: its green and its red.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use braid_format::{Kind, ObjectId, Objects};
use braid_hubproto::verify::{green_ref, red_ref};
use braid_repo::Repo;

use super::fetch::remote_ref;

/// How a branch compares with the hub's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// Both at the same changeset.
    Same,
    /// The hub's has changesets this one lacks, on top of it; or only the
    /// hub has the branch. `braid hub fetch` brings them in.
    Behind,
    /// This one has changesets the hub lacks, on top of the hub's; or only
    /// this repository has the branch.
    Ahead,
    /// Each has changesets the other lacks.
    Diverged,
}

impl State {
    /// How output names it.
    pub(crate) fn code(self) -> &'static str {
        match self {
            State::Same => "same",
            State::Behind => "behind",
            State::Ahead => "ahead",
            State::Diverged => "diverged",
        }
    }
}

/// One branch, here and on the hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BranchState {
    /// The branch.
    pub(crate) name: String,
    /// Its head here.
    pub(crate) local: Option<ObjectId>,
    /// Its head on the hub.
    pub(crate) hub: Option<ObjectId>,
    /// How they compare.
    pub(crate) state: State,
    /// The hub's green of the branch, when it verifies the branch: the
    /// newest changeset with a pass it accepts.
    pub(crate) green: Option<ObjectId>,
    /// The hub's red of the branch, while it holds the branch red.
    pub(crate) red: Option<ObjectId>,
}

/// Compares every branch of `repo` and of the hub, whose refs are `hub`.
///
/// When the hub's head is not in the store yet, the hub moved since the
/// last fetch, and a hub's branch only ever moves forward: a branch at, or
/// below, where the hub's was last seen (`remote/hub/<branch>`) is behind,
/// and any other is diverged.
pub(crate) fn compare(repo: &Repo, hub: &BTreeMap<String, ObjectId>) -> Result<Vec<BranchState>> {
    let store = repo.store();
    let local = store.refs()?;
    let names: BTreeSet<&str> = local
        .iter()
        .map(|(name, _)| name)
        .chain(hub.keys().map(String::as_str))
        .filter(|name| braid_repo::is_branch(name))
        .collect();
    let holds = |id: &ObjectId| -> Result<bool> {
        Ok(matches!(store.stat(id)?, Some((Kind::Changeset, _))))
    };
    let mut history = repo.history();
    let mut on_chain = |head: ObjectId, base: ObjectId| -> Result<bool> {
        Ok(history.first_parent_chain_contains(head, base, usize::MAX)?)
    };
    let mut states = Vec::new();
    for name in names {
        let (here, there) = (local.get(name), hub.get(name).copied());
        let state = match (here, there) {
            (Some(here), Some(there)) if here == there => State::Same,
            (Some(here), Some(there)) if holds(&here)? && holds(&there)? => {
                if on_chain(there, here)? {
                    State::Behind
                } else if on_chain(here, there)? {
                    State::Ahead
                } else {
                    State::Diverged
                }
            }
            (Some(here), Some(there)) if holds(&here)? && !holds(&there)? => {
                match local.get(&remote_ref(name)) {
                    Some(seen) if holds(&seen)? && (seen == here || on_chain(seen, here)?) => {
                        State::Behind
                    }
                    _ => State::Diverged,
                }
            }
            (Some(_), Some(_)) => State::Diverged,
            (None, _) => State::Behind,
            (Some(_), None) => State::Ahead,
        };
        states.push(BranchState {
            name: name.to_owned(),
            local: here,
            hub: there,
            state,
            green: hub.get(&green_ref(name)).copied(),
            red: hub.get(&red_ref(name)).copied(),
        });
    }
    history.flush()?;
    Ok(states)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::testing::{Fixture, ada_change, set_ref};

    #[test]
    fn each_branch_against_the_hub() {
        let f = Fixture::new();
        let root = ada_change(&f.repo, None, &[("a.txt", "a\n")]);
        let two = ada_change(&f.repo, Some(root), &[("b.txt", "b\n")]);
        let other = ada_change(&f.repo, Some(root), &[("c.txt", "c\n")]);
        for name in ["same", "behind", "ahead", "diverged", "local-only"] {
            set_ref(&f.repo, name, root);
        }
        set_ref(&f.repo, "ahead", two);
        set_ref(&f.repo, "diverged", other);
        set_ref(&f.repo, "tags/v1", root);
        // The hub moved past what this repository holds, from where it was
        // last seen.
        let unknown = ObjectId::hash(Kind::Changeset, b"not fetched");
        set_ref(&f.repo, "moved", root);
        set_ref(&f.repo, "remote/hub/moved", root);
        set_ref(&f.repo, "lost", two);
        set_ref(&f.repo, "remote/hub/lost", other);

        let hub: BTreeMap<String, ObjectId> = [
            ("same", root),
            ("behind", two),
            ("ahead", root),
            ("diverged", two),
            ("hub-only", two),
            ("moved", unknown),
            ("lost", unknown),
            ("tags/v1", two),
            // The hub verifies `behind`: green at root, red at two.
            ("braid/green/behind", root),
            ("braid/red/behind", two),
        ]
        .into_iter()
        .map(|(name, id)| (name.to_owned(), id))
        .collect();
        let states = compare(&f.repo, &hub).unwrap();
        let found: Vec<(&str, State)> = states.iter().map(|s| (s.name.as_str(), s.state)).collect();
        assert_eq!(
            found,
            [
                ("ahead", State::Ahead),
                ("behind", State::Behind),
                ("diverged", State::Diverged),
                ("hub-only", State::Behind),
                ("local-only", State::Ahead),
                ("lost", State::Diverged),
                ("moved", State::Behind),
                ("same", State::Same),
            ]
        );
        let verified: Vec<(&str, Option<ObjectId>, Option<ObjectId>)> = states
            .iter()
            .filter(|s| s.green.is_some() || s.red.is_some())
            .map(|s| (s.name.as_str(), s.green, s.red))
            .collect();
        assert_eq!(verified, [("behind", Some(root), Some(two))]);
    }
}
