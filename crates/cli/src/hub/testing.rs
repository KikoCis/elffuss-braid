// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! What the hub client's tests share: repositories in temporary
//! directories, and a hub in memory that answers the [`Remote`] calls the
//! way `docs/hub.md` says, landing with [`land_stack`] as a hub does. No
//! HTTP is involved.

use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::io::{Cursor, Read};

use braid_engine::{Error as EngineError, Reach, reachable};
use braid_format::{Kind, ObjectId, Objects};
use braid_hubproto::api::{
    LandMode, LandRequest, LandResponse, MissingRequest, Placed, Refs, RepoInfo,
};
use braid_hubproto::stream;
use braid_repo::Repo;
use braid_store::RefUpdate;
use braid_wc::{ChangeRequest, Identity, LandReport, Landed, StackMode, land_stack};
use tempfile::TempDir;

use super::link::Link;
use super::remote::{HubError, LACKS_BATCH, MAX_BODY, Remote};

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
