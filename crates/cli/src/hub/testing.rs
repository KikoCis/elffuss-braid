// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! What the hub client's tests share: repositories in temporary
//! directories, and a hub in memory that answers the [`Remote`] calls the
//! way `docs/hub.md` says, landing with [`land_stack`] as a hub does, and
//! the routes of verification ([`VerifyApi`]) the way section 25 says. No
//! HTTP is involved.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{Cursor, Read};

use braid_engine::{Error as EngineError, Reach, reachable};
use braid_format::{Kind, ObjectId, Objects};
use braid_hubproto::api::{
    LandMode, LandRequest, LandResponse, MissingRequest, Placed, Refs, RepoInfo,
};
use braid_hubproto::stream;
use braid_hubproto::verify::{
    self as verify, ReportPolicy, VerifyEvent, VerifyPolicy, VerifyReport, VerifyReset,
    VerifyStatus,
};
use braid_repo::Repo;
use braid_store::RefUpdate;
use braid_wc::{ChangeRequest, Identity, LandReport, Landed, StackMode, land_stack};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::link::Link;
use super::remote::{HubError, LACKS_BATCH, MAX_BODY, Remote};
use super::verify::VerifyApi;

/// The identity tests sign with.
pub(crate) fn ada() -> Identity {
    Identity::new("Ada", "ada@example.org").unwrap()
}

/// A changeset on `base` setting each `(path, content)`, made durable.
pub(crate) fn ada_change(repo: &Repo, base: Option<ObjectId>, sets: &[(&str, &str)]) -> ObjectId {
    let id = braid_wc::create_change(
        repo.store(),
        &ChangeRequest {
            base,
            sets: sets
                .iter()
                .map(|(path, content)| (path.as_bytes().to_vec(), content.as_bytes().to_vec()))
                .collect(),
            deletes: Vec::new(),
            expects: Vec::new(),
            message: b"change\n".to_vec(),
            author: ada(),
            agent: "tester".to_owned(),
        },
    )
    .unwrap();
    repo.store().sync().unwrap();
    id
}

/// Points `name` at `id`.
pub(crate) fn set_ref(repo: &Repo, name: &str, id: ObjectId) {
    repo.store()
        .transact("tester", &format!("set {name}"), |refs| {
            Ok::<_, Infallible>(vec![RefUpdate::new(name, refs.get(name), Some(id))])
        })
        .unwrap();
}

/// A repository in a temporary directory, and a link it may write.
pub(crate) struct Fixture {
    _dir: TempDir,
    pub(crate) repo: Repo,
    pub(crate) link: Link,
}

impl Fixture {
    /// A new, empty repository.
    pub(crate) fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        Fixture {
            _dir: dir,
            repo,
            link: Link {
                url: "http://127.0.0.1:9".to_owned(),
                repo: "ada/demo".parse().unwrap(),
                paths: None,
            },
        }
    }

    /// A new, empty repository linked to the repository of `hub`.
    pub(crate) fn linked(_hub: &FakeHub) -> Fixture {
        let fixture = Fixture::new();
        fixture.link.write(&fixture.repo).unwrap();
        fixture
    }
}

/// A hub in memory, holding one repository.
pub(crate) struct FakeHub {
    _dir: TempDir,
    /// The repository: its store and refs are the hub's.
    pub(crate) repo: Repo,
    /// Claims to store everything, so that uploads send nothing and a
    /// landing finds objects missing.
    pub(crate) forgetful: Cell<bool>,
    /// Answers every landing with `not_on_branch`.
    pub(crate) off_branch: Cell<bool>,
    /// The routes called, in order.
    pub(crate) calls: RefCell<Vec<&'static str>>,
    /// The verification of its repository's `main`.
    pub(crate) verify: RefCell<FakeVerify>,
}

fn internal(err: impl std::fmt::Display) -> HubError {
    HubError::api(500, "internal", err.to_string())
}

impl FakeHub {
    pub(crate) fn new() -> FakeHub {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        FakeHub {
            _dir: dir,
            repo,
            forgetful: Cell::new(false),
            off_branch: Cell::new(false),
            calls: RefCell::new(Vec::new()),
            verify: RefCell::new(FakeVerify::new()),
        }
    }

    /// How many times `route` was called.
    pub(crate) fn called(&self, route: &str) -> usize {
        self.calls.borrow().iter().filter(|c| **c == route).count()
    }

    fn call(&self, route: &'static str) {
        self.calls.borrow_mut().push(route);
    }

    /// The objects landing `change` needs that the hub does not store.
    fn needs(&self, change: ObjectId) -> Result<Vec<ObjectId>, HubError> {
        let store = self.repo.store();
        let reached = match reachable(
            store,
            &Reach {
                want: &[change],
                history: Some(0),
                ..Reach::default()
            },
        ) {
            Ok(reached) => reached,
            Err(EngineError::Missing { id, .. }) => return Ok(vec![id]),
            Err(err) => return Err(HubError::api(400, "invalid", err.to_string())),
        };
        let mut needed = Vec::new();
        for object in &reached {
            if !store.contains(&object.id).map_err(internal)? {
                needed.push(object.id);
            }
        }
        let changeset = braid_engine::read_changeset(store, change).map_err(internal)?;
        for parent in changeset.parents.iter().take(1) {
            if !store.contains(parent).map_err(internal)? {
                needed.push(*parent);
            }
        }
        Ok(needed)
    }
}

/// A landing as the hub reports it.
fn response(report: &LandReport) -> LandResponse {
    let (outcome, change, conflicts) = match &report.landed {
        Landed::Nothing => ("nothing", None, Vec::new()),
        Landed::FastForward(_) => ("fast-forward", None, Vec::new()),
        Landed::Rebased(_) => ("rebased", None, Vec::new()),
        Landed::AlreadyApplied => ("already-applied", None, Vec::new()),
        Landed::Conflicts { change, conflicts } => (
            "conflicts",
            Some(*change),
            conflicts
                .iter()
                .map(braid_hubproto::json::conflict)
                .collect(),
        ),
        Landed::Refused => ("refused", None, Vec::new()),
    };
    LandResponse {
        reference: report.branch.clone(),
        outcome: outcome.to_owned(),
        head_before: report.head_before,
        head: report.head(),
        landed: report
            .placed
            .iter()
            .map(|placed| Placed {
                from: placed.from,
                to: placed.to,
            })
            .collect(),
        merged_paths: report
            .merged_paths
            .iter()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect(),
        conflicts,
        change,
        git_commit: None,
        operation: report.operation,
        verify: None,
    }
}

impl Remote for FakeHub {
    fn refs(&self) -> Result<Refs, HubError> {
        self.call("refs");
        let refs = self.repo.store().refs().map_err(internal)?;
        Ok(Refs {
            seq: refs.seq(),
            refs: refs.as_map().clone(),
        })
    }

    fn info(&self) -> Result<RepoInfo, HubError> {
        self.call("info");
        Ok(RepoInfo {
            owner: "ada".to_owned(),
            name: "demo".to_owned(),
            private: true,
            description: String::new(),
            default_branch: "main".to_owned(),
            created: "2026-01-01T00:00:00Z".to_owned(),
            mirror: None,
            permission: "admin".to_owned(),
            ..RepoInfo::default()
        })
    }

    fn kind(&self, id: &ObjectId) -> Result<Option<Kind>, HubError> {
        self.call("kind");
        Ok(self
            .repo
            .store()
            .stat(id)
            .map_err(internal)?
            .map(|(kind, _)| kind))
    }

    fn lacks(&self, ids: &[ObjectId]) -> Result<Vec<ObjectId>, HubError> {
        self.call("lacks");
        if ids.len() > LACKS_BATCH {
            return Err(HubError::api(400, "invalid", "too many ids"));
        }
        if self.forgetful.get() {
            return Ok(Vec::new());
        }
        let mut lacking = Vec::new();
        for id in ids {
            if !self.repo.store().contains(id).map_err(internal)? {
                lacking.push(*id);
            }
        }
        Ok(lacking)
    }

    fn send_objects(&self, body: &mut dyn Read, len: u64) -> Result<u64, HubError> {
        self.call("objects");
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes)?;
        if bytes.len() as u64 != len || len > MAX_BODY {
            return Err(HubError::api(400, "invalid", "the body is not its length"));
        }
        let store = self.repo.store();
        let mut stored = 0;
        for record in stream::Reader::new(&bytes[..], MAX_BODY).map_err(internal)? {
            let record = record.map_err(|err| HubError::api(400, "invalid", err.to_string()))?;
            store.put(record.kind, &record.payload).map_err(internal)?;
            stored += 1;
        }
        store.sync().map_err(internal)?;
        Ok(stored)
    }

    fn missing(&self, request: &MissingRequest) -> Result<Box<dyn Read + '_>, HubError> {
        self.call("missing");
        let store = self.repo.store();
        let paths: Option<Vec<Vec<u8>>> = request
            .paths
            .as_ref()
            .map(|paths| paths.iter().map(|p| p.as_bytes().to_vec()).collect());
        let reached = reachable(
            store,
            &Reach {
                want: &request.want,
                have: &request.have,
                history: request.history,
                paths: paths.as_deref(),
                max_objects: None,
            },
        )
        .map_err(|err| HubError::api(400, "invalid", err.to_string()))?;
        let mut writer = stream::Writer::new(Vec::new())?;
        for object in &reached {
            let (kind, payload) = store
                .get(&object.id)
                .map_err(internal)?
                .ok_or_else(|| internal(format!("{} is gone", object.id)))?;
            writer.object(kind, &object.id, &payload)?;
        }
        Ok(Box::new(Cursor::new(writer.finish()?)))
    }

    fn land(&self, request: &LandRequest) -> Result<LandResponse, HubError> {
        self.call("land");
        if self.off_branch.get() {
            return Err(HubError::api(
                409,
                "not_on_branch",
                "the stack is not built on the ref",
            ));
        }
        let mut needed = Vec::new();
        for change in &request.changes {
            needed.extend(self.needs(*change)?);
        }
        if !needed.is_empty() {
            let mut err = HubError::api(409, "missing", "objects are missing");
            if let HubError::Api { body, .. } = &mut err {
                body.ids = needed;
            }
            return Err(err);
        }
        let mode = match request.mode {
            LandMode::Rebase => StackMode::Rebase,
            LandMode::FfOnly => StackMode::FastForwardOnly,
        };
        let committer = Identity::new("BraidHub", "hub@example.org").unwrap();
        let agent = request.agent.as_deref().unwrap_or("hub");
        match land_stack(
            &self.repo,
            &request.reference,
            &request.changes,
            mode,
            agent,
            &committer,
        ) {
            Ok(report) => Ok(response(&report)),
            Err(braid_wc::Error::NotOnBranch { .. }) => Err(HubError::api(
                409,
                "not_on_branch",
                "the stack is not built on the ref",
            )),
            Err(err @ (braid_wc::Error::EmptyStack | braid_wc::Error::NotAStack { .. })) => {
                Err(HubError::api(400, "invalid", err.to_string()))
            }
            Err(err) => Err(internal(err)),
        }
    }
}

/// How the fake hub's routes of verification answer while a test says so.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Refusal {
    /// An error answer: its status and its code.
    Api(u16, &'static str),
    /// The hub cannot be reached.
    Unreachable,
}

impl Refusal {
    fn error(self) -> HubError {
        match self {
            Refusal::Api(status, code) => HubError::api(status, code, "as the test says"),
            Refusal::Unreachable => HubError::Transport {
                hub: "http://127.0.0.1:9".to_owned(),
                detail: "connection refused".to_owned(),
            },
        }
    }
}

/// The verification of the fake hub's repository, whose default branch is
/// `main` (section 25): what a test sets, and what the calls of
/// [`VerifyApi`] leave. It keeps the refs in its status rather than in the
/// store, and names a culprit as the hub does when one changeset separates
/// green from a fail.
pub(crate) struct FakeVerify {
    /// Whether `GET /v1` lists the feature.
    pub(crate) served: bool,
    /// The policy, as it was last put, kept whole.
    pub(crate) policy: Value,
    /// How `main` stands.
    pub(crate) status: VerifyStatus,
    /// Fields a newer hub adds to the status it answers.
    pub(crate) extra: Option<Value>,
    /// The log, oldest first.
    pub(crate) events: Vec<VerifyEvent>,
    /// The git commits the hub knows, and their changesets.
    pub(crate) commits: HashMap<String, ObjectId>,
    /// Each request that carried an `Idempotency-Key`: the key, and the
    /// body.
    pub(crate) keyed: Vec<(String, Value)>,
    /// The verdicts recorded, oldest first.
    pub(crate) verdicts: Vec<(ObjectId, String)>,
    /// How many of the next answers of reports and resets are lost on
    /// their way back, once they took effect.
    pub(crate) lose: usize,
    /// What every call answers instead, while it is set.
    pub(crate) refuse: Option<Refusal>,
    /// The answers given, by key: the same key gets the same answer.
    answers: HashMap<String, Value>,
}

impl FakeVerify {
    fn new() -> FakeVerify {
        FakeVerify {
            served: false,
            policy: serde_json::to_value(VerifyPolicy::default()).unwrap(),
            status: VerifyStatus::default(),
            extra: None,
            events: Vec::new(),
            commits: HashMap::new(),
            keyed: Vec::new(),
            verdicts: Vec::new(),
            lose: 0,
            refuse: None,
            answers: HashMap::new(),
        }
    }

    /// Verification on, as an admin turns it on: the hub lists the
    /// feature, and takes the reports of ci-bot on the suite `ci`.
    pub(crate) fn turn_on(&mut self) {
        self.served = true;
        self.policy = serde_json::to_value(VerifyPolicy {
            mode: verify::MODE_REPORT.to_owned(),
            green_from: vec![verify::SOURCE_REPORT.to_owned()],
            flaky_moves_green: false,
            report: Some(ReportPolicy {
                suite: "ci".to_owned(),
                reporters: vec!["ci-bot".to_owned()],
            }),
        })
        .unwrap();
        self.status = VerifyStatus {
            mode: verify::MODE_REPORT.to_owned(),
            branch: Some("main".to_owned()),
            state: verify::STATE_UNKNOWN.to_owned(),
            ..VerifyStatus::default()
        };
    }

    fn refused(&self) -> Result<(), HubError> {
        self.refuse.map_or(Ok(()), |refusal| Err(refusal.error()))
    }

    /// Records an event of the log.
    fn event(&mut self, code: &str, changeset: Option<ObjectId>, actor: Option<&str>) {
        let id = self.events.len() as u64 + 1;
        self.events.push(VerifyEvent {
            id,
            at: format!("2026-10-22T12:{:02}:00Z", id % 60),
            code: code.to_owned(),
            changeset,
            other: None,
            actor: actor.map(str::to_owned),
        });
    }

    /// Keeps `answer` for `key`, and gives it, unless it is to be lost.
    fn answer(&mut self, key: &str, answer: Value) -> Result<Value, HubError> {
        let answer = self.answers.entry(key.to_owned()).or_insert(answer).clone();
        if self.lose > 0 {
            self.lose -= 1;
            return Err(HubError::Transport {
                hub: "http://127.0.0.1:9".to_owned(),
                detail: "the connection closed before the answer".to_owned(),
            });
        }
        Ok(answer)
    }

    /// The state the refs make, with `head` the head of `main`.
    fn settle(&mut self, head: Option<ObjectId>) {
        self.status.head = head;
        self.status.state = if self.status.red.is_some() {
            verify::STATE_RED
        } else if self.status.green.is_some() && self.status.green == head {
            verify::STATE_GREEN
        } else {
            verify::STATE_PENDING
        }
        .to_owned();
    }
}

impl FakeHub {
    /// The head of `main`.
    fn main_head(&self) -> Option<ObjectId> {
        self.repo.store().refs().ok()?.get("main")
    }

    /// Whether `id` is `head`, or on its first-parent chain.
    fn on_chain(&self, head: ObjectId, id: ObjectId) -> bool {
        braid_engine::first_parent_chain_contains(self.repo.store(), head, id, usize::MAX)
            .unwrap_or(false)
    }

    /// The first parent of `id`.
    fn parent(&self, id: ObjectId) -> Option<ObjectId> {
        braid_engine::read_changeset(self.repo.store(), id)
            .ok()?
            .parents
            .first()
            .copied()
    }
}

impl VerifyApi for FakeHub {
    fn features(&self) -> Result<Vec<String>, HubError> {
        self.call("features");
        let verify = self.verify.borrow();
        verify.refused()?;
        Ok(if verify.served {
            vec![verify::FEATURE.to_owned()]
        } else {
            Vec::new()
        })
    }

    fn status(&self) -> Result<Value, HubError> {
        self.call("verify");
        let verify = self.verify.borrow();
        verify.refused()?;
        let mut answer = serde_json::to_value(&verify.status).map_err(internal)?;
        if let (Some(Value::Object(extra)), Value::Object(fields)) = (&verify.extra, &mut answer) {
            fields.extend(extra.clone());
        }
        Ok(answer)
    }

    fn report(&self, report: &VerifyReport, key: &str) -> Result<Value, HubError> {
        self.call("verify/report");
        let head = self.main_head();
        let mut verify = self.verify.borrow_mut();
        verify.refused()?;
        let body = serde_json::to_value(report).map_err(internal)?;
        verify.keyed.push((key.to_owned(), body));
        if let Some(answer) = verify.answers.get(key).cloned() {
            return verify.answer(key, answer);
        }
        let policy: VerifyPolicy =
            serde_json::from_value(verify.policy.clone()).map_err(internal)?;
        let Some(reporting) = policy.report.filter(|_| policy.mode != verify::MODE_OFF) else {
            return Err(HubError::api(403, "forbidden", "verification is off"));
        };
        if report.suite != reporting.suite {
            return Err(HubError::api(400, "invalid", "not the policy's suite"));
        }
        let changeset = match (&report.changeset, &report.git_commit) {
            (Some(id), None) => *id,
            (None, Some(sha)) => *verify
                .commits
                .get(sha)
                .ok_or_else(|| HubError::api(404, "not_found", "no such commit"))?,
            _ => return Err(HubError::api(400, "invalid", "give one of the two")),
        };
        if !head.is_some_and(|head| self.on_chain(head, changeset)) {
            let mut err = HubError::api(409, "not_on_branch", "not on main");
            if let HubError::Api { body, .. } = &mut err {
                body.head = head.map(|head| head.to_hex());
            }
            return Err(err);
        }
        let outcome = report.outcome.as_str();
        let effect = if verify
            .status
            .green
            .is_some_and(|green| self.on_chain(green, changeset))
        {
            verify::EFFECT_STALE
        } else if verify.verdicts.contains(&(changeset, outcome.to_owned())) {
            verify::EFFECT_DUPLICATE
        } else {
            verify.verdicts.push((changeset, outcome.to_owned()));
            match outcome {
                verify::PASS => {
                    verify.status.green = Some(changeset);
                    verify.event(verify::EVENT_GREEN, Some(changeset), None);
                    let cleared = verify
                        .status
                        .red
                        .is_some_and(|red| self.on_chain(changeset, red));
                    if cleared {
                        let status = &mut verify.status;
                        (status.red, status.culprit, status.decided_by) = (None, None, None);
                        (status.code, status.since, status.window) = (None, None, None);
                        verify.event(verify::EVENT_FIXED, None, None);
                        verify::EFFECT_RED_CLEARED
                    } else {
                        verify::EFFECT_GREEN_MOVED
                    }
                }
                verify::FAIL if verify.status.red.is_none() => {
                    let single = verify.status.green.is_some()
                        && self.parent(changeset) == verify.status.green;
                    let status = &mut verify.status;
                    status.red = Some(changeset);
                    status.since = Some("2026-10-22T12:03:00Z".to_owned());
                    if single {
                        status.culprit = Some(changeset);
                        status.decided_by = Some(verify::DECIDED_SINGLE.to_owned());
                    } else {
                        status.code = Some(
                            if status.green.is_some() {
                                verify::CODE_CULPRIT_UNKNOWN
                            } else {
                                verify::CODE_NO_GREEN
                            }
                            .to_owned(),
                        );
                    }
                    verify.event(verify::EVENT_RED, Some(changeset), None);
                    verify::EFFECT_RED_SET
                }
                verify::FLAKY => {
                    verify.status.code = Some(verify::CODE_FLAKY.to_owned());
                    verify.event(verify::EVENT_FLAKY, Some(changeset), None);
                    verify::EFFECT_RECORDED
                }
                _ => verify::EFFECT_RECORDED,
            }
        };
        verify.settle(head);
        let answer = json!({"effect": effect, "status": verify.status});
        verify.answer(key, answer)
    }

    fn policy(&self) -> Result<Value, HubError> {
        self.call("verify/policy");
        let verify = self.verify.borrow();
        verify.refused()?;
        Ok(verify.policy.clone())
    }

    fn put_policy(&self, policy: &Value) -> Result<Value, HubError> {
        self.call("verify/policy put");
        let mut verify = self.verify.borrow_mut();
        verify.refused()?;
        let mode = policy["mode"].as_str().unwrap_or_default().to_owned();
        if !verify::MODES.contains(&mode.as_str()) {
            return Err(HubError::api(400, "invalid", "no such mode"));
        }
        verify.policy = policy.clone();
        if mode == verify::MODE_OFF {
            verify.status = VerifyStatus::default();
        } else {
            verify.status.mode = mode;
            verify.status.branch = Some("main".to_owned());
        }
        verify.event(verify::EVENT_POLICY, None, Some("ada"));
        Ok(policy.clone())
    }

    fn log(&self, after: Option<u64>, limit: Option<u32>) -> Result<Value, HubError> {
        self.call("verify/log");
        let verify = self.verify.borrow();
        verify.refused()?;
        let limit = limit.unwrap_or(30) as usize;
        let older: Vec<&VerifyEvent> = verify
            .events
            .iter()
            .rev()
            .filter(|event| after.is_none_or(|after| event.id < after))
            .collect();
        let page: Vec<&VerifyEvent> = older.iter().take(limit).copied().collect();
        let next = (older.len() > limit).then(|| page.last().map(|event| event.id));
        Ok(json!({"events": page, "next": next.flatten()}))
    }

    fn reset(&self, reset: &VerifyReset, key: &str) -> Result<Value, HubError> {
        self.call("verify/reset");
        let head = self.main_head();
        let mut verify = self.verify.borrow_mut();
        verify.refused()?;
        let body = serde_json::to_value(reset).map_err(internal)?;
        verify.keyed.push((key.to_owned(), body));
        if let Some(answer) = verify.answers.get(key).cloned() {
            return verify.answer(key, answer);
        }
        if let Some(expect) = &reset.expect
            && (expect.green != verify.status.green || expect.red != verify.status.red)
        {
            return Err(HubError::api(409, "moved", "the refs moved"));
        }
        if let Some(green) = reset.green {
            if !head.is_some_and(|head| self.on_chain(head, green)) {
                return Err(HubError::api(400, "invalid", "not on main"));
            }
            verify.status.green = Some(green);
        }
        let status = &mut verify.status;
        (status.red, status.culprit, status.decided_by) = (None, None, None);
        (status.code, status.since, status.window) = (None, None, None);
        verify.event(verify::EVENT_RESET, reset.green, Some("ada"));
        verify.settle(head);
        let answer = serde_json::to_value(&verify.status).map_err(internal)?;
        verify.answer(key, answer)
    }
}
