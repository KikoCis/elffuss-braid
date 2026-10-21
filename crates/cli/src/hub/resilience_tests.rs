// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The hub client when the hub, or the way to it, fails. A fault at any
//! step of a landing leaves the local repository and the workspace as they
//! were, or where the hub is, says what to run next, and running the same
//! landing again finishes the job. An answer that does not hold the stack
//! it claims to hold, an outcome the protocol does not have, an edit made
//! while a landing is in flight, an object stream with a hole, and a
//! damaged credentials file each fail without losing anything and without
//! showing a token.
//!
//! The hub is [`FakeHub`] behind [`Faulty`], a [`Remote`] that fails as it
//! is told: the `nth` call of a route, before the hub does anything or
//! after the call took effect (objects stored, a stack landed), as when an
//! answer is lost on its way back.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::Result;
use braid_engine::read_changeset;
use braid_format::{Kind, ObjectId, Objects};
use braid_hubproto::api::{LandRequest, LandResponse, MissingRequest, Refs, RepoInfo};
use braid_hubproto::{Token, stream};
use braid_repo::Repo;
use braid_wc::{CommitRequest, Workspace};

use super::creds::{Credentials, HubEnv, hub_url};
use super::fetch::fetch;
use super::land::{HubLanding, land_change, land_workspace};
use super::remote::{HubError, Remote};
use super::testing::{FakeHub, Fixture, ada, ada_change, set_ref};

/// A route of the hub, as [`Remote`] names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Route {
    Refs,
    Info,
    Kind,
    Lacks,
    Objects,
    Missing,
    Land,
}

/// When a fault strikes: before the hub does anything, or once the call
/// took effect, as when the answer is lost on its way back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum When {
    Before,
    After,
}

/// How a call fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    /// The connection broke.
    Transport,
    /// 500 `internal`.
    Internal,
    /// 502 from a proxy in front of the hub, with its HTML page.
    ProxyPage,
    /// 429 `rate_limited`, to try again in a second.
    RateLimited,
    /// 401 `unauthorized`: the token was revoked meanwhile.
    Unauthorized,
}

const FAILURES: [Failure; 5] = [
    Failure::Transport,
    Failure::Internal,
    Failure::ProxyPage,
    Failure::RateLimited,
    Failure::Unauthorized,
];

impl Failure {
    fn error(self) -> HubError {
        match self {
            Failure::Transport => HubError::Transport {
                hub: "http://127.0.0.1:9".to_owned(),
                detail: "io: Connection reset by peer (os error 54)".to_owned(),
            },
            Failure::Internal => HubError::api(500, "internal", "the hub failed"),
            Failure::ProxyPage => HubError::api(
                502,
                "http_502",
                "<html><head><title>502 Bad Gateway</title></head><body><center>502 Bad Gateway</center></body></html>",
            ),
            Failure::RateLimited => {
                let mut err = HubError::api(429, "rate_limited", "too many requests");
                if let HubError::Api { body, .. } = &mut err {
                    body.retry_after = Some(1);
                }
                err
            }
            Failure::Unauthorized => HubError::api(401, "unauthorized", "the token was revoked"),
        }
    }
}

/// One fault: the `nth` call of `route` fails with `failure`, `when`.
#[derive(Clone, Copy, Debug)]
struct Fault {
    route: Route,
    nth: usize,
    when: When,
    failure: Failure,
}

/// A [`FakeHub`] that fails as told.
struct Faulty<'h> {
    hub: &'h FakeHub,
    /// The fault to strike, if any.
    fault: Cell<Option<Fault>>,
    /// Whether it struck.
    fired: Cell<bool>,
    /// Calls of each route so far, faults included.
    calls: RefCell<BTreeMap<Route, usize>>,
    /// The largest upload body the hub takes; bigger ones are 413.
    max_body: Cell<Option<u64>>,
    /// Answers every landing with this, without landing anything.
    answer: RefCell<Option<LandResponse>>,
    /// Runs once the hub has landed a stack, before the answer goes back.
    after_land: RefCell<Option<Box<dyn Fn() + 'h>>>,
    /// Leaves the first blob out of every `missing` stream.
    drop_blob: Cell<bool>,
    /// The blob it left out last.
    dropped: Cell<Option<ObjectId>>,
}

impl<'h> Faulty<'h> {
    fn new(hub: &'h FakeHub) -> Faulty<'h> {
        Faulty {
            hub,
            fault: Cell::new(None),
            fired: Cell::new(false),
            calls: RefCell::new(BTreeMap::new()),
            max_body: Cell::new(None),
            answer: RefCell::new(None),
            after_land: RefCell::new(None),
            drop_blob: Cell::new(false),
            dropped: Cell::new(None),
        }
    }

    /// Counts a call of `route`, and returns which call it is.
    fn count(&self, route: Route) -> usize {
        let mut calls = self.calls.borrow_mut();
        let n = calls.entry(route).or_insert(0);
        *n += 1;
        *n
    }

    fn called(&self, route: Route) -> usize {
        self.calls.borrow().get(&route).copied().unwrap_or(0)
    }

    /// The fault for the `nth` call of `route` at `when`, if one is set.
    fn strike(&self, route: Route, nth: usize, when: When) -> Result<(), HubError> {
        match self.fault.get() {
            Some(fault) if fault.route == route && fault.nth == nth && fault.when == when => {
                self.fired.set(true);
                Err(fault.failure.error())
            }
            _ => Ok(()),
        }
    }

    /// Runs `call` against the hub as the `nth` call of `route`, with the
    /// fault around it.
    fn around<T>(
        &self,
        route: Route,
        call: impl FnOnce() -> Result<T, HubError>,
    ) -> Result<T, HubError> {
        let nth = self.count(route);
        self.strike(route, nth, When::Before)?;
        let answer = call()?;
        self.strike(route, nth, When::After)?;
        Ok(answer)
    }
}

/// `bytes`, an object stream, without its first blob; and that blob.
fn without_first_blob(bytes: &[u8]) -> (Vec<u8>, Option<ObjectId>) {
    let mut writer = stream::Writer::new(Vec::new()).unwrap();
    let mut dropped = None;
    for record in stream::Reader::new(bytes, 1 << 30).unwrap() {
        let record = record.unwrap();
        if record.kind == Kind::Blob && dropped.is_none() {
            dropped = Some(record.id);
            continue;
        }
        writer
            .object(record.kind, &record.id, &record.payload)
            .unwrap();
    }
    (writer.finish().unwrap(), dropped)
}

impl Remote for Faulty<'_> {
    fn refs(&self) -> Result<Refs, HubError> {
        self.around(Route::Refs, || self.hub.refs())
    }

    fn info(&self) -> Result<RepoInfo, HubError> {
        self.around(Route::Info, || self.hub.info())
    }

    fn kind(&self, id: &ObjectId) -> Result<Option<Kind>, HubError> {
        self.around(Route::Kind, || self.hub.kind(id))
    }

    fn lacks(&self, ids: &[ObjectId]) -> Result<Vec<ObjectId>, HubError> {
        self.around(Route::Lacks, || self.hub.lacks(ids))
    }

    fn send_objects(&self, body: &mut dyn Read, len: u64) -> Result<u64, HubError> {
        if let Some(max) = self.max_body.get()
            && len > max
        {
            return Err(HubError::api(
                413,
                "too_large",
                format!("a body of {len} bytes is more than {max}"),
            ));
        }
        self.around(Route::Objects, || self.hub.send_objects(body, len))
    }

    fn missing(&self, request: &MissingRequest) -> Result<Box<dyn Read + '_>, HubError> {
        let mut bytes = self.around(Route::Missing, || {
            let mut bytes = Vec::new();
            self.hub.missing(request)?.read_to_end(&mut bytes)?;
            Ok(bytes)
        })?;
        if self.drop_blob.get() {
            let (cut, dropped) = without_first_blob(&bytes);
            bytes = cut;
            self.dropped.set(dropped);
        }
        Ok(Box::new(Cursor::new(bytes)))
    }

    fn land(&self, request: &LandRequest) -> Result<LandResponse, HubError> {
        self.around(Route::Land, || {
            if let Some(answer) = self.answer.borrow().clone() {
                return Ok(answer);
            }
            let response = self.hub.land(request)?;
            if let Some(hook) = &*self.after_land.borrow() {
                hook();
            }
            Ok(response)
        })
    }

    fn max_body(&self) -> u64 {
        self.max_body.get().unwrap_or_else(|| self.hub.max_body())
    }
}

/// A linked repository holding the hub's `main`, as [`land`](super::land)'s
/// own tests set it up.
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

    /// Someone else lands on the hub's main: a new file, so that what lands
    /// next is rebased.
    fn move_hub(&self) -> ObjectId {
        let theirs = ada_change(&self.hub.repo, self.hub_main(), &[("d.txt", "theirs\n")]);
        set_ref(&self.hub.repo, "main", theirs);
        theirs
    }

    fn land(&self, remote: &impl Remote, ws: &Workspace<'_>) -> Result<HubLanding> {
        land_workspace(
            &self.local.repo,
            ws,
            remote,
            &self.local.link,
            None,
            "agent-1",
        )
    }
}

/// Writes `files` in the workspace and commits them with `message`.
fn commit(ws: &Workspace<'_>, files: &[(&str, &str)], message: &str) -> ObjectId {
    for (path, content) in files {
        fs::write(ws.dir().join(path), content).unwrap();
    }
    ws.commit(&CommitRequest {
        message: Some(message.as_bytes().to_vec()),
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

/// Every file of a directory, by path, with its bytes.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, out);
            } else {
                let name = path.strip_prefix(root).unwrap().to_path_buf();
                out.insert(name, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn refs(repo: &Repo) -> BTreeMap<String, ObjectId> {
    repo.store().refs().unwrap().as_map().clone()
}

/// What a landing that fails must leave as it was: every local ref, the
/// workspace's base and its files.
struct Snapshot {
    refs: BTreeMap<String, ObjectId>,
    base: Option<ObjectId>,
    files: BTreeMap<PathBuf, Vec<u8>>,
}

impl Snapshot {
    fn of(repo: &Repo, ws: &Workspace<'_>) -> Snapshot {
        Snapshot {
            refs: refs(repo),
            base: ws.state().unwrap().base,
            files: files(ws.dir()),
        }
    }

    /// Asserts that every local ref is as it was, or names a changeset the
    /// hub holds, and that the workspace did not move.
    fn left_alone(&self, s: &Setup, ws: &Workspace<'_>, what: &str) {
        for (name, id) in refs(&s.local.repo) {
            if self.refs.get(&name) != Some(&id) {
                assert!(
                    matches!(
                        s.hub.repo.store().stat(&id).unwrap(),
                        Some((Kind::Changeset, _))
                    ),
                    "{what}: {name} moved to {id}, which the hub does not hold"
                );
            }
        }
        assert_eq!(
            ws.state().unwrap().base,
            self.base,
            "{what}: the base moved"
        );
        assert_eq!(files(ws.dir()), self.files, "{what}: the files changed");
    }
}

/// How many changesets along the first parents of `head` have `message`.
fn on_chain(repo: &Repo, head: Option<ObjectId>, message: &str) -> usize {
    let store = repo.store();
    let mut found = 0;
    let mut current = head;
    while let Some(id) = current {
        let changeset = read_changeset(store, id).unwrap();
        if changeset.message == message.as_bytes() {
            found += 1;
        }
        current = changeset.parents.first().copied();
    }
    found
}

/// Four files of different content, each a body of its own under a limit
/// of 1,024 bytes, so that an upload of them takes more than three bodies.
fn change() -> Vec<(String, String)> {
    (1..=4)
        .map(|n| {
            let line = format!("file {n}: the quick brown fox jumps over the lazy dog\n");
            (format!("c{n}.txt"), line.repeat(16))
        })
        .collect()
}

/// [`commit`], with owned names and contents.
fn commit_owned(ws: &Workspace<'_>, files: &[(String, String)], message: &str) -> ObjectId {
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    commit(ws, &files, message)
}

/// One landing with `fault` set, on a hub that took nothing meanwhile
/// (`rebase` false) or that someone else landed on first; then the same
/// landing again without it, then once more.
fn one_fault(rebase: bool, fault: Fault) {
    let what = format!(
        "{} with {fault:?}",
        if rebase { "a rebase" } else { "a fast-forward" }
    );
    let s = Setup::new();
    let ws = s.workspace("w1");
    let files = change();
    let mine = commit_owned(&ws, &files, "the change");
    if rebase {
        s.move_hub();
    }
    let faulty = Faulty::new(&s.hub);
    faulty.max_body.set(Some(1024));
    faulty.fault.set(Some(fault));
    let before = Snapshot::of(&s.local.repo, &ws);

    let err = match s.land(&faulty, &ws) {
        Ok(landing) => panic!("{what}: landed anyway: {landing:?}"),
        Err(err) => format!("{err:#}"),
    };
    assert!(faulty.fired.get(), "{what}: the fault never struck");
    assert!(
        err.contains("braid land") || err.contains("braid update"),
        "{what}: the error names no command to run next: {err}"
    );
    before.left_alone(&s, &ws, &what);
    let hub_landed = matches!(
        (fault.route, fault.nth, fault.when),
        (Route::Land, _, When::After) | (Route::Refs, 2, _) | (Route::Missing, 2, _)
    );
    let hub_before = s.hub_main();
    assert_eq!(
        on_chain(&s.hub.repo, hub_before, "the change"),
        usize::from(hub_landed),
        "{what}: the hub's main"
    );

    // The same landing again finishes the job.
    faulty.fault.set(None);
    let again = s
        .land(&faulty, &ws)
        .unwrap_or_else(|err| panic!("{what}: landing again failed: {err:#}"));
    let outcome = again.response.as_ref().map(|r| r.outcome.as_str());
    let expected = match (hub_landed, rebase) {
        (false, false) => Some("fast-forward"),
        (false, true) => Some("rebased"),
        (true, false) => None,
        (true, true) => Some("already-applied"),
    };
    assert_eq!(outcome, expected, "{what}: landing again: {again:?}");
    let head = s.hub_main();
    assert_eq!(
        on_chain(&s.hub.repo, head, "the change"),
        1,
        "{what}: the change is on the hub's main once"
    );
    assert_eq!(s.local_main(), head, "{what}: the local main");
    assert_eq!(ws.state().unwrap().base, head, "{what}: the workspace");
    if !rebase {
        assert_eq!(head, Some(mine), "{what}");
    }
    for (path, content) in &files {
        assert_eq!(
            &fs::read_to_string(ws.dir().join(path)).unwrap(),
            content,
            "{what}"
        );
    }

    // And once more: nothing to land, and nothing is asked of the hub.
    let lands = faulty.called(Route::Land);
    let third = s.land(&faulty, &ws).unwrap();
    assert!(third.response.is_none(), "{what}: {third:?}");
    assert_eq!(faulty.called(Route::Land), lands, "{what}");
}

#[test]
fn a_fault_at_any_step_of_a_landing_leaves_nothing_half_way() {
    let rows = [
        (Route::Refs, 1, When::Before),
        (Route::Missing, 1, When::Before),
        (Route::Lacks, 1, When::Before),
        (Route::Objects, 1, When::Before),
        (Route::Objects, 3, When::After),
        (Route::Land, 1, When::Before),
        (Route::Land, 1, When::After),
        (Route::Refs, 2, When::Before),
        (Route::Missing, 2, When::Before),
    ];
    for rebase in [false, true] {
        for (route, nth, when) in rows {
            // A fast-forward needs nothing from the hub: no `missing` call.
            if !rebase && route == Route::Missing {
                continue;
            }
            for failure in FAILURES {
                one_fault(
                    rebase,
                    Fault {
                        route,
                        nth,
                        when,
                        failure,
                    },
                );
            }
        }
    }
}

/// A landing whose answer was lost after the hub fast-forwarded, landed
/// again, leaves the workspace ready for the next one: what it lands next
/// is its new changeset alone.
#[test]
fn after_a_lost_answer_the_next_landing_takes_only_what_is_new() {
    for nth_refs in [None, Some(2)] {
        let s = Setup::new();
        let ws = s.workspace("w1");
        let first = commit(&ws, &[("c.txt", "c\n")], "first");
        let faulty = Faulty::new(&s.hub);
        faulty.fault.set(Some(match nth_refs {
            None => Fault {
                route: Route::Land,
                nth: 1,
                when: When::After,
                failure: Failure::Transport,
            },
            Some(nth) => Fault {
                route: Route::Refs,
                nth,
                when: When::Before,
                failure: Failure::ProxyPage,
            },
        }));
        assert!(s.land(&faulty, &ws).is_err());
        faulty.fault.set(None);
        assert!(s.land(&faulty, &ws).unwrap().response.is_none());
        assert_eq!(ws.state().unwrap().base, Some(first));

        let second = commit(&ws, &[("e.txt", "e\n")], "second");
        s.move_hub();
        let landing = s.land(&faulty, &ws).unwrap();
        let response = landing.response.unwrap();
        assert_eq!(response.outcome, "rebased", "{response:?}");
        let froms: Vec<ObjectId> = response.landed.iter().map(|p| p.from).collect();
        assert_eq!(froms, [second], "only the new changeset goes up");
        assert_eq!(on_chain(&s.hub.repo, s.hub_main(), "first"), 1);
        assert_eq!(on_chain(&s.hub.repo, s.hub_main(), "second"), 1);
    }
}

/// An answer that says the stack landed, with a head that does not hold
/// it, moves nothing: not the workspace, its files, nor the local branch.
#[test]
fn an_answer_whose_head_lacks_the_stack_moves_nothing() {
    for outcome in ["already-applied", "fast-forward", "rebased"] {
        let s = Setup::new();
        let ws = s.workspace("w1");
        let mine = commit(&ws, &[("c.txt", "c\n")], "edit c.txt");
        let hub_main = s.hub_main().unwrap();
        // Someone else's changeset, on no branch of the hub.
        let unrelated = ada_change(&s.hub.repo, None, &[("x.txt", "unrelated\n")]);
        let head = if outcome == "rebased" {
            unrelated
        } else {
            hub_main
        };
        let faulty = Faulty::new(&s.hub);
        *faulty.answer.borrow_mut() = Some(LandResponse {
            reference: "main".to_owned(),
            outcome: outcome.to_owned(),
            head_before: Some(hub_main),
            head: Some(head),
            landed: Vec::new(),
            merged_paths: Vec::new(),
            conflicts: Vec::new(),
            change: None,
            git_commit: None,
            operation: Some(7),
        });
        let before = Snapshot::of(&s.local.repo, &ws);
        let err = format!("{:#}", s.land(&faulty, &ws).unwrap_err());
        assert!(err.contains("does not hold"), "{outcome}: {err}");
        assert!(err.contains(&head.short()), "{outcome}: {err}");
        assert_eq!(ws.state().unwrap().base, Some(mine), "{outcome}");
        assert_eq!(
            fs::read_to_string(ws.dir().join("c.txt")).unwrap(),
            "c\n",
            "{outcome}"
        );
        assert_eq!(refs(&s.local.repo), before.refs, "{outcome}");
        before.left_alone(&s, &ws, outcome);

        // The hub's real answer lands it, once.
        *faulty.answer.borrow_mut() = None;
        let landing = s.land(&faulty, &ws).unwrap();
        assert_eq!(landing.response.unwrap().outcome, "fast-forward");
        assert_eq!(s.hub_main(), Some(mine));
        assert_eq!(on_chain(&s.hub.repo, s.hub_main(), "edit c.txt"), 1);
        assert_eq!(ws.state().unwrap().base, Some(mine));
    }
}

/// The same for a stack that carries a changeset landed here with
/// `--local`: the local branch, which is that changeset, does not follow a
/// head that lacks it.
#[test]
fn a_head_that_lacks_a_local_changeset_does_not_take_the_branch_back() {
    let s = Setup::new();
    let w1 = s.workspace("w1");
    let local_only = commit(&w1, &[("local.txt", "l\n")], "landed here");
    w1.land(None, "agent-1", &ada()).unwrap();
    assert_eq!(s.local_main(), Some(local_only));
    let w2 = s.workspace("w2");
    let mine = commit(&w2, &[("c.txt", "c\n")], "edit c.txt");
    let hub_main = s.hub_main().unwrap();
    let faulty = Faulty::new(&s.hub);
    *faulty.answer.borrow_mut() = Some(LandResponse {
        reference: "main".to_owned(),
        outcome: "rebased".to_owned(),
        head_before: Some(hub_main),
        head: Some(hub_main),
        landed: Vec::new(),
        merged_paths: Vec::new(),
        conflicts: Vec::new(),
        change: None,
        git_commit: None,
        operation: Some(7),
    });
    let err = format!("{:#}", s.land(&faulty, &w2).unwrap_err());
    assert!(err.contains("does not hold"), "{err}");
    assert_eq!(s.local_main(), Some(local_only), "the branch stayed");
    assert_eq!(w2.state().unwrap().base, Some(mine));

    *faulty.answer.borrow_mut() = None;
    let landing = s.land(&faulty, &w2).unwrap();
    let response = landing.response.unwrap();
    assert_eq!(response.outcome, "fast-forward");
    let froms: Vec<ObjectId> = response.landed.iter().map(|p| p.from).collect();
    assert_eq!(froms, [local_only, mine]);
    assert_eq!(s.local_main(), Some(mine));
}

/// An outcome a landing in rebase mode never has (`refused`), or one the
/// protocol does not know, is an error that names it: nothing moved.
#[test]
fn a_refused_or_unknown_outcome_is_an_error() {
    for outcome in ["refused", "queued"] {
        let s = Setup::new();
        let ws = s.workspace("w1");
        let mine = commit(&ws, &[("c.txt", "c\n")], "edit c.txt");
        let hub_main = s.hub_main().unwrap();
        let faulty = Faulty::new(&s.hub);
        *faulty.answer.borrow_mut() = Some(LandResponse {
            reference: "main".to_owned(),
            outcome: outcome.to_owned(),
            head_before: Some(hub_main),
            head: Some(hub_main),
            landed: Vec::new(),
            merged_paths: Vec::new(),
            conflicts: Vec::new(),
            change: None,
            git_commit: None,
            operation: None,
        });
        let before = Snapshot::of(&s.local.repo, &ws);
        let err = format!("{:#}", s.land(&faulty, &ws).unwrap_err());
        assert!(
            err.contains(outcome) && err.contains("did not land"),
            "{outcome}: {err}"
        );
        assert_eq!(ws.state().unwrap().base, Some(mine));
        assert_eq!(refs(&s.local.repo), before.refs, "{outcome}");
        before.left_alone(&s, &ws, outcome);

        // `change create --land` too, naming the changeset.
        let change = ada_change(&s.local.repo, Some(hub_main), &[("n.txt", "n\n")]);
        let err = format!(
            "{:#}",
            land_change(
                &s.local.repo,
                &faulty,
                &s.local.link,
                "main",
                change,
                "agent-1",
                "braid change create"
            )
            .unwrap_err()
        );
        assert!(
            err.contains(outcome) && err.contains(&change.to_hex()) && err.contains("did not land"),
            "{outcome}: {err}"
        );
        assert_eq!(s.local_main(), Some(hub_main));
    }
}

/// A file edited in the workspace while a fast-forward is in flight: the
/// landing went through and says so, the edit stays, and the next landing
/// takes only the changeset that records it.
#[test]
fn an_edit_during_a_fast_forward_stays_and_lands_next() {
    let s = Setup::new();
    let ws = s.workspace("w1");
    let mine = commit(&ws, &[("c.txt", "c\n")], "edit c.txt");
    let faulty = Faulty::new(&s.hub);
    let dir = ws.dir().to_path_buf();
    *faulty.after_land.borrow_mut() = Some(Box::new(move || {
        fs::write(dir.join("e.txt"), "edited meanwhile\n").unwrap();
    }));
    let landing = s.land(&faulty, &ws).unwrap();
    assert_eq!(landing.response.as_ref().unwrap().outcome, "fast-forward");
    assert_eq!(landing.base, Some(mine));
    assert_eq!(s.hub_main(), Some(mine));
    assert_eq!(s.local_main(), Some(mine));
    assert_eq!(
        fs::read_to_string(ws.dir().join("e.txt")).unwrap(),
        "edited meanwhile\n"
    );
    *faulty.after_land.borrow_mut() = None;

    let edit = commit(&ws, &[], "record the edit");
    let landing = s.land(&faulty, &ws).unwrap();
    let response = landing.response.unwrap();
    assert_eq!(response.outcome, "fast-forward");
    let froms: Vec<ObjectId> = response.landed.iter().map(|p| p.from).collect();
    assert_eq!(froms, [edit]);
    assert_eq!(s.hub_main(), Some(edit));
}

/// The same while a rebase is in flight: the workspace cannot move onto the
/// rebased head without losing the edit, so it stays, and the error says
/// that the change landed, where, and what to do; committing and landing
/// again finishes it, with every change on the hub once.
#[test]
fn an_edit_during_a_rebase_stays_and_the_error_says_what_landed() {
    let s = Setup::new();
    let ws = s.workspace("w1");
    let mine = commit(&ws, &[("c.txt", "c\n")], "edit c.txt");
    s.move_hub();
    let faulty = Faulty::new(&s.hub);
    let dir = ws.dir().to_path_buf();
    *faulty.after_land.borrow_mut() = Some(Box::new(move || {
        fs::write(dir.join("e.txt"), "edited meanwhile\n").unwrap();
    }));
    let err = format!("{:#}", s.land(&faulty, &ws).unwrap_err());
    let head = s.hub_main().unwrap();
    assert!(
        err.contains(&head.short()) && err.contains("landed") && err.contains("braid land"),
        "{err}"
    );
    assert_eq!(ws.state().unwrap().base, Some(mine));
    assert_eq!(
        fs::read_to_string(ws.dir().join("e.txt")).unwrap(),
        "edited meanwhile\n"
    );
    assert!(!ws.dir().join("d.txt").exists());
    *faulty.after_land.borrow_mut() = None;

    commit(&ws, &[], "record the edit");
    let landing = s.land(&faulty, &ws).unwrap();
    assert_eq!(landing.response.unwrap().outcome, "rebased");
    let head = s.hub_main();
    assert_eq!(on_chain(&s.hub.repo, head, "edit c.txt"), 1);
    assert_eq!(on_chain(&s.hub.repo, head, "record the edit"), 1);
    assert_eq!(ws.state().unwrap().base, head);
    assert_eq!(s.local_main(), head);
    assert_eq!(
        fs::read_to_string(ws.dir().join("d.txt")).unwrap(),
        "theirs\n"
    );
}

/// An object stream that leaves out a blob a tree names moves no ref; the
/// next fetch, with the whole stream, brings everything.
#[test]
fn a_stream_with_a_hole_moves_no_ref_and_the_next_fetch_heals_it() {
    let s = Setup::new();
    let theirs = s.move_hub();
    let faulty = Faulty::new(&s.hub);
    faulty.drop_blob.set(true);
    let before = refs(&s.local.repo);
    let err = format!(
        "{:#}",
        fetch(&s.local.repo, &faulty, &s.local.link, "tester", None).unwrap_err()
    );
    let dropped = faulty.dropped.get().expect("a blob was left out");
    assert!(err.contains(&dropped.to_hex()), "{err}");
    assert_eq!(refs(&s.local.repo), before);

    // The first fetch of a landing too: nothing moves, the workspace stays.
    let ws = s.workspace("w1");
    commit(&ws, &[("c.txt", "c\n")], "edit c.txt");
    let snapshot = Snapshot::of(&s.local.repo, &ws);
    let err = format!("{:#}", s.land(&faulty, &ws).unwrap_err());
    assert!(err.contains("--local"), "{err}");
    assert_eq!(refs(&s.local.repo), snapshot.refs);
    snapshot.left_alone(&s, &ws, "landing");

    faulty.drop_blob.set(false);
    fetch(&s.local.repo, &faulty, &s.local.link, "tester", None).unwrap();
    assert_eq!(s.local_main(), Some(theirs));
    assert!(s.local.repo.store().contains(&dropped).unwrap());
    assert!(s.local.repo.store().fsck().unwrap().is_clean());
}

/// A credentials file damaged in a way that puts the token where serde
/// quotes what it found: the error names the file and what to do, and
/// shows no part of the token.
#[test]
fn a_damaged_credentials_file_never_shows_the_token() {
    let token = Token::from_random([7; 5], [9; 32]).expose();
    let secret = &token["brd_".len() + 8 + 1..];
    assert_eq!(secret.len(), 52);
    let url = "https://hub.example.org";
    let shows_none = |err: &str, what: &str| {
        assert!(!err.contains(&token), "{what}: {err}");
        for start in 0..=secret.len() - 16 {
            let run = &secret[start..start + 16];
            assert!(
                !err.contains(run),
                "{what}: part of the secret shows: {err}"
            );
        }
    };
    let variants = [
        serde_json::json!({ "hubs": { url: token } }).to_string(),
        serde_json::json!({ "hubs": token }).to_string(),
        serde_json::json!({ "autopublish": token }).to_string(),
        serde_json::json!({ "default": token }).to_string(),
        serde_json::json!({ "default": url, "hubs": { url: { "token": token, "login": 7 } } })
            .to_string(),
        format!("{{\"hubs\": {{\"{url}\": {{\"token\": \"{token}\""),
        format!("{{\"hubs\": {{\"{url}\": {{\"token\": \"{token}\"}}}}, {token}"),
    ];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.json");
    for text in &variants {
        fs::write(&path, text).unwrap();
        let err = format!("{:#}", Credentials::load(&path).unwrap_err());
        assert!(
            err.contains(&path.display().to_string()) && err.contains("fix it, or remove it"),
            "{text}: {err}"
        );
        shows_none(&err, text);
    }

    // A token where the hub's URL goes, in the file or in BRAID_HUB_URL,
    // is refused without being repeated.
    let creds = Credentials {
        default: Some(token.clone()),
        ..Credentials::default()
    };
    let err = format!(
        "{:#}",
        hub_url(&HubEnv::default(), &creds, None).unwrap_err()
    );
    assert!(err.contains("not a hub URL"), "{err}");
    shows_none(&err, "default");
    let env = HubEnv {
        url: Some(format!("  {token}  ")),
        ..HubEnv::default()
    };
    let err = format!(
        "{:#}",
        hub_url(&env, &Credentials::default(), None).unwrap_err()
    );
    assert!(err.contains("not a hub URL"), "{err}");
    shows_none(&err, "BRAID_HUB_URL");
    let err = format!(
        "{:#}",
        hub_url(&HubEnv::default(), &Credentials::default(), Some(&token)).unwrap_err()
    );
    shows_none(&err, "--hub");
}
