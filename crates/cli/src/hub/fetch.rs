// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Bringing a hub's branches and tags in: `braid hub fetch`, `braid hub
//! clone`, and the fetch that landing and updating start with in a linked
//! repository.
//!
//! The objects come as `/missing` streams: what the hub's heads need, less
//! what the local heads reach. Then one op log operation moves the refs:
//! each local branch moves to the hub's head when it does not exist, or
//! when its head is on the first-parent chain of the hub's; a branch with
//! changesets the hub does not have stays where it is, and is reported. A
//! tag is created, and never moved. `remote/hub/<ref>` records where each
//! of the hub's refs was seen, so that a branch kept back still has a name
//! for the hub's head.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use braid_engine::{LandOutcome, REBASED_FROM, read_changeset};
use braid_format::changeset::Changes;
use braid_format::tree::{EntryKind, Node};
use braid_format::{Kind, ObjectId, Objects, Signature};
use braid_hubproto::api::MissingRequest;
use braid_repo::{Repo, RepoHistory};
use braid_store::{RefUpdate, Refs, Store, TransactError};

use super::link::Link;
use super::remote::{HubError, Remote};
use super::transfer::{Received, store_stream};

/// The prefix of the refs that record where the hub's refs were seen. A
/// `git sync` label always ends with a hash, so it is never `hub`.
pub(crate) const REMOTE_PREFIX: &str = "remote/hub/";

/// Where the hub's ref `name` was seen.
pub(crate) fn remote_ref(name: &str) -> String {
    format!("{REMOTE_PREFIX}{name}")
}

/// Whether a ref is a tag.
pub(crate) fn is_tag(name: &str) -> bool {
    name.starts_with("tags/")
}

/// Whether a hub ref is brought in: a branch or a tag.
pub(crate) fn is_carried(name: &str) -> bool {
    braid_repo::is_branch(name) || is_tag(name)
}

/// What became of a local ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// It did not exist: it was created at the hub's head.
    Created,
    /// It moved forward to the hub's head.
    Moved,
    /// It was at the hub's head already.
    Same,
    /// It has changesets the hub lacks, on top of the hub's head: kept.
    Ahead,
    /// It and the hub each have changesets the other lacks: kept.
    Diverged,
    /// The hub's ref could not be brought in, for the reason given.
    Skipped(&'static str),
}

impl Outcome {
    /// How JSON output names it.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Outcome::Created => "created",
            Outcome::Moved => "moved",
            Outcome::Same => "same",
            Outcome::Ahead => "ahead",
            Outcome::Diverged => "diverged",
            Outcome::Skipped(_) => "skipped",
        }
    }
}

/// One of the hub's refs, and what became of the local ref of that name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefFetched {
    /// The ref.
    pub(crate) name: String,
    /// The hub's head.
    pub(crate) hub: ObjectId,
    /// The local ref before.
    pub(crate) before: Option<ObjectId>,
    /// The local ref after.
    pub(crate) after: Option<ObjectId>,
    /// What happened.
    pub(crate) outcome: Outcome,
}

/// What a fetch did.
#[derive(Clone, Debug)]
pub(crate) struct Fetched {
    /// The hub's op log sequence number when its refs were read.
    pub(crate) seq: u64,
    /// Every ref of the hub, as read.
    pub(crate) hub_refs: BTreeMap<String, ObjectId>,
    /// Objects stored.
    pub(crate) objects: u64,
    /// The hub's branches and tags, by name.
    pub(crate) refs: Vec<RefFetched>,
    /// The local op log operation that moved refs, if any did.
    pub(crate) operation: Option<u64>,
}

impl Fetched {
    /// The hub's head of `branch`, if it has the branch.
    pub(crate) fn hub_head(&self, branch: &str) -> Option<ObjectId> {
        self.hub_refs.get(branch).copied()
    }

    /// The heads of the hub's branches.
    pub(crate) fn hub_branch_heads(&self) -> Vec<ObjectId> {
        self.hub_refs
            .iter()
            .filter(|(name, _)| braid_repo::is_branch(name))
            .map(|(_, id)| *id)
            .collect()
    }
}

/// Whether the store holds `id` as a changeset.
fn holds_changeset(store: &Store, id: &ObjectId) -> Result<bool> {
    Ok(matches!(store.stat(id)?, Some((Kind::Changeset, _))))
}

/// Whether `head` holds every changeset of `stack`, a stack as a landing
/// sends it, oldest first: its last changeset, or one rebased from it, is
/// on `head`'s first-parent chain, as after a fast-forward or a rebase; or
/// `head` has what each of them changes already, which is what a landing
/// that finds them there answers. `false` when the store does not hold
/// `head`. Nothing is written to the store.
pub(crate) fn holds(store: &Store, head: ObjectId, stack: &[ObjectId]) -> Result<bool> {
    if !holds_changeset(store, &head)? {
        return Ok(false);
    }
    let Some(&last) = stack.last() else {
        return Ok(true);
    };
    if landed_on(store, head, last)? {
        return Ok(true);
    }
    let scratch = Scratch {
        store,
        written: RefCell::new(HashMap::new()),
    };
    let checker = Signature {
        name: b"braid".to_vec(),
        email: b"braid@example.org".to_vec(),
        seconds: 0,
        offset_minutes: 0,
    };
    for change in stack {
        if !holds_changeset(store, change)? {
            return Ok(false);
        }
        match braid_engine::land(&scratch, Some(head), *change, checker.clone()) {
            Ok(LandOutcome::AlreadyApplied) => {}
            Ok(_) => return Ok(false),
            Err(err) if err.is_store_failure() => return Err(err.into()),
            Err(_) => return Ok(false),
        }
    }
    Ok(true)
}

/// Whether `change`, or a changeset rebased from it, is on the first-parent
/// chain of `head`, above `change`'s own first parent.
fn landed_on(store: &Store, head: ObjectId, change: ObjectId) -> Result<bool> {
    let base = read_changeset(store, change)?.parents.first().copied();
    let origin = change.to_hex().into_bytes();
    let mut current = Some(head);
    while let Some(id) = current {
        if id == change {
            return Ok(true);
        }
        if Some(id) == base {
            return Ok(false);
        }
        let changeset = read_changeset(store, id)?;
        if changeset
            .extras
            .iter()
            .any(|(key, value)| key.as_slice() == REBASED_FROM && *value == origin)
        {
            return Ok(true);
        }
        current = changeset.parents.first().copied();
    }
    Ok(false)
}

/// The store, with what is written kept in memory instead: for checks that
/// run the engine, and must leave the store as it was.
struct Scratch<'s> {
    store: &'s Store,
    written: RefCell<HashMap<ObjectId, (Kind, Vec<u8>)>>,
}

impl Objects for Scratch<'_> {
    type Error = <Store as Objects>::Error;

    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Self::Error> {
        if let Some((kind, payload)) = self.written.borrow().get(id) {
            return Ok(Some((*kind, payload.clone())));
        }
        self.store.get(id)
    }

    fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Self::Error> {
        let id = ObjectId::hash(kind, payload);
        self.written
            .borrow_mut()
            .entry(id)
            .or_insert_with(|| (kind, payload.to_vec()));
        Ok(id)
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, Self::Error> {
        Ok(self.written.borrow().contains_key(id) || self.store.contains(id)?)
    }

    fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, Self::Error> {
        if let Some((kind, payload)) = self.written.borrow().get(id) {
            return Ok(Some((*kind, payload.len() as u64)));
        }
        self.store.stat(id)
    }
}

/// What the store holds with everything it reaches: the heads of local
/// branches and of `remote/hub/` refs, and `extra`.
pub(crate) fn haves(store: &Store, refs: &Refs, extra: &[ObjectId]) -> Result<Vec<ObjectId>> {
    let mut found = BTreeSet::new();
    for (name, id) in refs.iter() {
        if (braid_repo::is_branch(name) || name.starts_with(REMOTE_PREFIX))
            && holds_changeset(store, &id)?
        {
            found.insert(id);
        }
    }
    found.extend(extra);
    Ok(found.into_iter().collect())
}

/// Brings in what holding `want` takes, through `/missing` streams whose
/// lists hold no more ids than the hub takes in one: `want` goes in parts,
/// and `have`, which only saves sending what is here already, is cut short.
fn bring(
    store: &Store,
    remote: &impl Remote,
    link: &Link,
    want: Vec<ObjectId>,
    mut have: Vec<ObjectId>,
) -> Result<u64> {
    if want.is_empty() {
        return Ok(0);
    }
    let most = if want.len() > 1 || have.len() > 1 {
        remote.max_ids().max(1)
    } else {
        1
    };
    have.truncate(most);
    let mut objects = 0;
    for part in want.chunks(most) {
        let request = MissingRequest {
            want: part.to_vec(),
            have: have.clone(),
            history: None,
            paths: link.paths.clone(),
            limit: None,
            after: None,
        };
        let body = remote.missing(&request)?;
        let received = store_stream(store, body)?;
        for id in &request.want {
            if !holds_changeset(store, id)? {
                bail!("the hub's object stream did not hold changeset {id}, which was asked for");
            }
        }
        whole(store, &received, link.prefixes().as_deref())?;
        objects += received.objects;
    }
    Ok(objects)
}

/// Checks that what a stream brought is whole, before any ref moves to it:
/// each changeset's change list and git origin are stored, and so is
/// everything each tree node it brought names, along the paths of a sparse
/// repository. A node the store held already came whole before.
pub(crate) fn whole(store: &Store, received: &Received, paths: Option<&[Vec<u8>]>) -> Result<()> {
    let brought: HashSet<ObjectId> = received.trees.iter().copied().collect();
    let need = |id: ObjectId, what: &str, by: ObjectId| -> Result<()> {
        if store.contains(&id)? {
            Ok(())
        } else {
            bail!(
                "the hub's object stream did not hold {what} {id}, which {by} needs: nothing moved here; run the command again"
            )
        }
    };
    let mut walked = HashSet::new();
    for &id in &received.changesets {
        let changeset = read_changeset(store, id)?;
        if let Changes::External { list, .. } = &changeset.changes {
            need(*list, "the change list", id)?;
        }
        if let Some(origin) = changeset.git_origin {
            need(origin, "the git origin", id)?;
        }
        let mut pending = vec![(changeset.root, Vec::new())];
        while let Some((node, dir)) = pending.pop() {
            if node == braid_engine::EMPTY_TREE
                || !brought.contains(&node)
                || !walked.insert((node, dir.clone()))
            {
                continue;
            }
            let Some((_, bytes)) = store.get(&node)? else {
                bail!("tree {node} went missing from the store");
            };
            match Node::decode(&bytes).map_err(|err| anyhow!("tree {node}: {err}"))? {
                Node::Flat(entries) | Node::Leaf(entries) => {
                    for entry in entries {
                        let path = if dir.is_empty() {
                            entry.name.clone()
                        } else {
                            [&dir[..], b"/", &entry.name].concat()
                        };
                        if !wanted(paths, &path) {
                            continue;
                        }
                        match entry.kind {
                            EntryKind::File { blob, .. }
                            | EntryKind::Executable { blob, .. }
                            | EntryKind::Symlink { blob, .. } => need(blob, "blob", node)?,
                            EntryKind::Tree { tree } => {
                                need(tree, "tree", node)?;
                                pending.push((tree, path));
                            }
                            EntryKind::Gitlink { .. } => {}
                        }
                    }
                }
                Node::Internal { children, .. } => {
                    for child in children {
                        need(child.node, "tree", node)?;
                        pending.push((child.node, dir.clone()));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Whether a sparse repository holds what is at `path`: it is on the way to
/// one of `paths`, or under one. Everything, without `paths`.
fn wanted(paths: Option<&[Vec<u8>]>, path: &[u8]) -> bool {
    let Some(prefixes) = paths else {
        return true;
    };
    prefixes.iter().any(|prefix| {
        let on_the_way = prefix.starts_with(path)
            && (prefix.len() == path.len() || path.is_empty() || prefix[path.len()] == b'/');
        let under = path.starts_with(prefix)
            && (path.len() == prefix.len() || prefix.is_empty() || path[prefix.len()] == b'/');
        on_the_way || under
    })
}

/// Whether the hub refused to send what was asked: an id it does not hold
/// (404), or holds as another kind (400).
fn refused(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<HubError>(),
        Some(HubError::Api {
            status: 400 | 404,
            ..
        })
    )
}

/// A branch, and the stack a landing just put on it on the hub.
pub(crate) type Landed<'a> = (&'a str, &'a [ObjectId]);

/// Fetches from the hub into `repo`, recording the operation as `agent`.
///
/// `landed` names a branch and the stack a landing just put on it: when
/// the local branch's head is one of those changesets, it moves to the
/// hub's head even if that is not a fast-forward, once that head holds its
/// changes, rebased.
pub(crate) fn fetch(
    repo: &Repo,
    remote: &impl Remote,
    link: &Link,
    agent: &str,
    landed: Option<Landed<'_>>,
) -> Result<Fetched> {
    let hub = remote.refs()?;
    let store = repo.store();
    let local = store.refs()?;
    let carried: Vec<(String, ObjectId)> = hub
        .refs
        .iter()
        .filter(|(name, _)| is_carried(name))
        .map(|(name, id)| (name.clone(), *id))
        .collect();

    // What a local ref points at is whole, and so is the stack a landing
    // just sent, which this repository made. Anything else the hub's refs
    // name is asked for, even when the store holds it: a stream that broke
    // half way may have left it there without all it needs. The hub sends
    // nothing that the `have` changesets reach.
    let sent: &[ObjectId] = landed.map_or(&[], |(_, stack)| stack);
    let pointed: HashSet<ObjectId> = local
        .iter()
        .map(|(_, id)| id)
        .chain(sent.iter().copied())
        .collect();

    // Branches first: their heads are changesets.
    let mut branch_heads = BTreeSet::new();
    for (name, id) in &carried {
        if !is_tag(name) && !pointed.contains(id) {
            branch_heads.insert(*id);
        }
    }
    let branch_heads: Vec<ObjectId> = branch_heads.into_iter().collect();
    let mut objects = bring(
        store,
        remote,
        link,
        branch_heads.clone(),
        haves(store, &local, sent)?,
    )?;

    // Then tags, which may name anything: most name a changeset that came
    // with the branches; the hub is asked about the others. A tag whose
    // target the hub gives no valid kind or stream for is skipped, saying
    // why: one odd tag never stops a fetch.
    let mut skipped: HashMap<String, &'static str> = HashMap::new();
    let mut tag_targets: BTreeMap<ObjectId, Vec<String>> = BTreeMap::new();
    for (name, id) in carried.iter().filter(|(name, _)| is_tag(name)) {
        let kind = match store.stat(id)? {
            Some((kind, _)) => Some(kind),
            None => match remote.kind(id) {
                Ok(kind) => kind,
                Err(HubError::Protocol(_)) => {
                    skipped.insert(
                        name.clone(),
                        "the hub gives no valid kind for the object it names",
                    );
                    continue;
                }
                Err(err) => return Err(err.into()),
            },
        };
        match kind {
            Some(Kind::Changeset) => {
                if !pointed.contains(id) {
                    tag_targets.entry(*id).or_default().push(name.clone());
                }
            }
            Some(_) => {
                skipped.insert(
                    name.clone(),
                    "it names an object that is not a changeset, such as an annotated tag, which the hub protocol cannot carry",
                );
            }
            None => {
                skipped.insert(name.clone(), "the hub does not hold what it names");
            }
        }
    }
    let have = haves(store, &local, &[&branch_heads[..], sent].concat())?;
    let targets: Vec<ObjectId> = tag_targets.keys().copied().collect();
    match bring(store, remote, link, targets.clone(), have.clone()) {
        Ok(brought) => objects += brought,
        // One at a time, then, leaving out what the hub will not send.
        Err(err) if refused(&err) => {
            for target in targets {
                match bring(store, remote, link, vec![target], have.clone()) {
                    Ok(brought) => objects += brought,
                    Err(err) if refused(&err) => {
                        for name in &tag_targets[&target] {
                            skipped.insert(
                                name.clone(),
                                "the hub does not send the changeset it names",
                            );
                        }
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        Err(err) => return Err(err),
    }

    let mut history = repo.history();
    let mut decided = Vec::new();
    let description = format!("hub fetch {} from {}", link.repo, link.url);
    let result = store.transact_with(agent, |current| -> Result<_, Option<anyhow::Error>> {
        let mut updates = Vec::new();
        for (name, hub_head) in &carried {
            let before = current.get(name);
            let outcome = match skipped.get(name) {
                Some(reason) => Outcome::Skipped(reason),
                None => {
                    let seen = remote_ref(name);
                    if current.get(&seen) != Some(*hub_head) {
                        updates.push(RefUpdate::new(
                            seen.clone(),
                            current.get(&seen),
                            Some(*hub_head),
                        ));
                    }
                    decide(&mut history, store, name, before, *hub_head, landed).map_err(Some)?
                }
            };
            let after = match outcome {
                Outcome::Created | Outcome::Moved => {
                    updates.push(RefUpdate::new(name.clone(), before, Some(*hub_head)));
                    Some(*hub_head)
                }
                _ => before,
            };
            decided.push(RefFetched {
                name: name.clone(),
                hub: *hub_head,
                before,
                after,
                outcome,
            });
        }
        // A ref the hub no longer has, or that cannot be carried, has no
        // place where it was seen.
        for (name, id) in current.iter() {
            if let Some(rest) = name.strip_prefix(REMOTE_PREFIX)
                && (!carried.iter().any(|(carried, _)| carried == rest)
                    || skipped.contains_key(rest))
            {
                updates.push(RefUpdate::new(name, Some(id), None));
            }
        }
        if updates.is_empty() {
            return Err(None);
        }
        Ok((updates, description))
    });
    let operation = match result {
        Ok(op) => Some(op.seq),
        Err(TransactError::Aborted(None)) => None,
        Err(TransactError::Aborted(Some(err))) => return Err(err),
        Err(TransactError::Store(err)) => return Err(err.into()),
    };
    history.flush()?;
    Ok(Fetched {
        seq: hub.seq,
        hub_refs: hub.refs,
        objects,
        refs: decided,
        operation,
    })
}

/// What becomes of the local ref `name`, at `before`, given the hub's head.
fn decide(
    history: &mut RepoHistory<'_>,
    store: &Store,
    name: &str,
    before: Option<ObjectId>,
    hub: ObjectId,
    landed: Option<Landed<'_>>,
) -> Result<Outcome> {
    let Some(local) = before else {
        return Ok(Outcome::Created);
    };
    if local == hub {
        return Ok(Outcome::Same);
    }
    // A tag never moves, and a ref that names no changeset is no branch to
    // move forward.
    if is_tag(name) || !holds_changeset(store, &local)? {
        return Ok(Outcome::Diverged);
    }
    let on_chain = |history: &mut RepoHistory<'_>, head: ObjectId, base: ObjectId| {
        history
            .first_parent_chain_contains(head, base, usize::MAX)
            .map_err(|err| anyhow!(err))
    };
    // The branch follows a landing that rebased its head only when the
    // hub's head holds what the branch has of the stack: never back.
    let rebased_here = match landed {
        Some((branch, stack)) if branch == name => match stack.iter().position(|id| *id == local) {
            Some(at) => holds(store, hub, &stack[..=at])?,
            None => false,
        },
        _ => false,
    };
    if rebased_here || on_chain(history, hub, local)? {
        Ok(Outcome::Moved)
    } else if on_chain(history, local, hub)? {
        Ok(Outcome::Ahead)
    } else {
        Ok(Outcome::Diverged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::testing::{FakeHub, Fixture, ada_change, set_ref};

    fn outcome(fetched: &Fetched, name: &str) -> Outcome {
        fetched
            .refs
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("no {name} in {:?}", fetched.refs))
            .outcome
    }

    #[test]
    fn branches_move_forward_only() {
        let hub = FakeHub::new();
        let local = Fixture::linked(&hub);
        let root = ada_change(&hub.repo, None, &[("a.txt", "a\n")]);
        let two = ada_change(&hub.repo, Some(root), &[("b.txt", "b\n")]);
        set_ref(&hub.repo, "main", two);
        set_ref(&hub.repo, "behind", two);
        set_ref(&hub.repo, "ahead", root);
        set_ref(&hub.repo, "diverged", two);
        set_ref(&hub.repo, "tags/v1", root);
        set_ref(&hub.repo, "braid/internal", root);

        // The first fetch creates everything it carries.
        let first = fetch(&local.repo, &hub, &local.link, "tester", None).unwrap();
        assert!(first.objects > 0);
        for name in ["main", "behind", "ahead", "diverged", "tags/v1"] {
            assert_eq!(outcome(&first, name), Outcome::Created, "{name}");
        }
        let refs = local.repo.store().refs().unwrap();
        assert_eq!(refs.get("main"), Some(two));
        assert_eq!(refs.get("tags/v1"), Some(root));
        assert_eq!(refs.get("remote/hub/main"), Some(two));
        assert_eq!(refs.get("braid/internal"), None, "internal refs stay");
        assert!(first.refs.iter().all(|r| r.name != "braid/internal"));

        // Local work and hub work.
        let three = ada_change(&hub.repo, Some(two), &[("c.txt", "c\n")]);
        set_ref(&hub.repo, "behind", three);
        set_ref(&hub.repo, "diverged", three);
        let mine = ada_change(&local.repo, Some(root), &[("mine.txt", "m\n")]);
        set_ref(&local.repo, "ahead", mine);
        let other = ada_change(&local.repo, Some(two), &[("other.txt", "o\n")]);
        set_ref(&local.repo, "diverged", other);
        set_ref(&local.repo, "tags/v1", two);

        let second = fetch(&local.repo, &hub, &local.link, "tester", None).unwrap();
        assert_eq!(outcome(&second, "main"), Outcome::Same);
        assert_eq!(outcome(&second, "behind"), Outcome::Moved);
        assert_eq!(outcome(&second, "ahead"), Outcome::Ahead);
        assert_eq!(outcome(&second, "diverged"), Outcome::Diverged);
        assert_eq!(
            outcome(&second, "tags/v1"),
            Outcome::Diverged,
            "tags never move"
        );
        let refs = local.repo.store().refs().unwrap();
        assert_eq!(refs.get("behind"), Some(three));
        assert_eq!(refs.get("ahead"), Some(mine));
        assert_eq!(refs.get("diverged"), Some(other));
        // The hub's head has a name even where the branch was kept.
        assert_eq!(refs.get("remote/hub/diverged"), Some(three));
        assert!(local.repo.store().contains(&three).unwrap());

        // Nothing new: no operation.
        let third = fetch(&local.repo, &hub, &local.link, "tester", None).unwrap();
        assert_eq!(third.objects, 0);
        assert_eq!(third.operation, None);

        // A landing that rebased a local branch's head lets that branch, and
        // no other, follow: the hub's head holds the changeset, rebased.
        set_ref(&local.repo, "behind", other);
        crate::hub::transfer::upload(
            local.repo.store(),
            &hub,
            &braid_engine::Reach {
                want: &[other],
                ..braid_engine::Reach::default()
            },
        )
        .unwrap();
        let signature = Signature {
            name: b"BraidHub".to_vec(),
            email: b"hub@example.org".to_vec(),
            seconds: 1_700_000_000,
            offset_minutes: 0,
        };
        let Ok(LandOutcome::Rebased { changeset, .. }) =
            braid_engine::land(hub.repo.store(), Some(three), other, signature)
        else {
            panic!("other rebases onto three");
        };
        hub.repo.store().sync().unwrap();
        set_ref(&hub.repo, "diverged", changeset);
        let landed: &[ObjectId] = &[other];
        let fourth = fetch(
            &local.repo,
            &hub,
            &local.link,
            "tester",
            Some(("diverged", landed)),
        )
        .unwrap();
        assert_eq!(outcome(&fourth, "diverged"), Outcome::Moved);
        assert_eq!(outcome(&fourth, "behind"), Outcome::Diverged);
    }

    #[test]
    fn tags_that_name_no_changeset_are_skipped() {
        let hub = FakeHub::new();
        let local = Fixture::linked(&hub);
        let root = ada_change(&hub.repo, None, &[("a.txt", "a\n")]);
        let side = ada_change(&hub.repo, Some(root), &[("side.txt", "s\n")]);
        set_ref(&hub.repo, "main", root);
        // A tag on a changeset no branch has, and one on a blob.
        set_ref(&hub.repo, "tags/side", side);
        let blob = hub
            .repo
            .store()
            .put(Kind::Blob, b"object 1234\ntype commit\n")
            .unwrap();
        hub.repo.store().sync().unwrap();
        set_ref(&hub.repo, "tags/annotated", blob);
        let fetched = fetch(&local.repo, &hub, &local.link, "tester", None).unwrap();
        assert_eq!(outcome(&fetched, "tags/side"), Outcome::Created);
        assert!(matches!(
            outcome(&fetched, "tags/annotated"),
            Outcome::Skipped(_)
        ));
        let refs = local.repo.store().refs().unwrap();
        assert_eq!(refs.get("tags/side"), Some(side));
        assert_eq!(refs.get("tags/annotated"), None);
        assert_eq!(refs.get("remote/hub/tags/annotated"), None);
    }

    #[test]
    fn a_sparse_link_brings_only_its_paths() {
        let hub = FakeHub::new();
        let root = ada_change(
            &hub.repo,
            None,
            &[("src/net/tcp.rs", "tcp\n"), ("docs/big.md", "docs\n")],
        );
        set_ref(&hub.repo, "main", root);
        let mut local = Fixture::linked(&hub);
        local.link.paths = Some(vec!["src/net".to_owned()]);
        fetch(&local.repo, &hub, &local.link, "tester", None).unwrap();
        let store = local.repo.store();
        assert!(store.contains(&root).unwrap());
        let tcp = ObjectId::hash(Kind::Blob, b"tcp\n");
        let docs = ObjectId::hash(Kind::Blob, b"docs\n");
        assert!(store.contains(&tcp).unwrap());
        assert!(!store.contains(&docs).unwrap());
    }
}
