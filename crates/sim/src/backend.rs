// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The system under test: the engine over an object store, and the lander, a
//! serialized section that owns the branch head.
//!
//! The lander does what the command line will do inside
//! [`Store::transact`]: read the head, call [`land`], and move the branch with
//! a compare-and-swap from the head it read to the changeset the outcome
//! names. Nothing moves on a conflict or when the change has nothing left to
//! add. A crash falls between computing the outcome and the compare-and-swap.

use std::path::{Path, PathBuf};

use braid_engine::{LandOutcome, land};
use braid_format::{MemoryObjects, ObjectId, Objects, Signature};
use braid_store::{Options, RefUpdate, Store, TransactError};

/// The branch the agents land on.
pub const MAIN: &str = "main";

/// The agent label of the lander's operations.
const LANDER: &str = "lander";

/// One landing request, as the lander receives it.
#[derive(Clone, Debug)]
pub struct LandRequest {
    /// The changeset to land.
    pub change: ObjectId,
    /// Who records the rebased changeset.
    pub committer: Signature,
    /// Whether the lander crashes after computing the outcome, before the
    /// compare-and-swap.
    pub crash: bool,
    /// What the operation is, for the op log.
    pub description: String,
}

/// What the lander did with a request.
#[derive(Clone, Debug)]
pub struct Landed {
    /// The head the lander read inside its serialized section.
    pub head: Option<ObjectId>,
    /// What [`land`] decided.
    pub outcome: LandOutcome,
    /// Where the compare-and-swap moved the branch, if it ran and moved it.
    pub moved: Option<ObjectId>,
}

/// One move of the branch, as the op log records it: the head before and
/// after.
pub type Move = (Option<ObjectId>, Option<ObjectId>);

/// The changeset an outcome moves the branch to.
pub fn target(outcome: &LandOutcome) -> Option<ObjectId> {
    match outcome {
        LandOutcome::FastForward(id) => Some(*id),
        LandOutcome::Rebased { changeset, .. } => Some(*changeset),
        LandOutcome::AlreadyApplied | LandOutcome::Conflicts(_) => None,
    }
}

/// Where objects and the branch head live.
pub trait Backend {
    /// The object store.
    type Objects: Objects;

    /// A short name for reports.
    fn name(&self) -> &'static str;

    /// Objects as the lander and the checks see them.
    fn objects(&self) -> &Self::Objects;

    /// Objects as an agent sees them: its own handle on the shared store.
    fn agent_objects(&self, agent: usize) -> &Self::Objects;

    /// Makes what an agent wrote durable, before it asks for a landing.
    fn agent_sync(&self, agent: usize) -> Result<(), String>;

    /// The head of the branch, as the checks read it.
    fn head(&self) -> Result<Option<ObjectId>, String>;

    /// The head of the branch, as an agent reads it.
    fn agent_head(&self, agent: usize) -> Result<Option<ObjectId>, String>;

    /// Points the branch at its first changeset.
    fn create_branch(&mut self, root: ObjectId) -> Result<(), String>;

    /// Runs the lander's serialized section for one request.
    fn land(&mut self, request: &LandRequest) -> Result<Landed, String>;

    /// Every move of the branch the op log recorded, oldest first.
    fn branch_moves(&self) -> Result<Vec<Move>, String>;

    /// Checks the store at the end of a run and opens it again, as a new
    /// process would. Returns whether anything was opened again: objects in
    /// memory stay as they are.
    fn reopen(&mut self) -> Result<bool, String>;
}

/// Objects in memory, and a simulated ref with its own op log.
#[derive(Debug, Default)]
pub struct MemoryBackend {
    objects: MemoryObjects,
    main: Option<ObjectId>,
    moves: Vec<Move>,
}

impl MemoryBackend {
    /// An empty store and no branch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the branch from `old` to `new`, if it is still at `old`.
    fn compare_and_swap(
        &mut self,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
    ) -> Result<(), String> {
        if self.main != old {
            return Err(format!(
                "compare-and-swap of {MAIN} failed: expected {old:?}, found {:?}",
                self.main
            ));
        }
        self.main = new;
        self.moves.push((old, new));
        Ok(())
    }
}

impl Backend for MemoryBackend {
    type Objects = MemoryObjects;

    fn name(&self) -> &'static str {
        "memory"
    }

    fn objects(&self) -> &MemoryObjects {
        &self.objects
    }

    fn agent_objects(&self, _agent: usize) -> &MemoryObjects {
        &self.objects
    }

    fn agent_sync(&self, _agent: usize) -> Result<(), String> {
        Ok(())
    }

    fn head(&self) -> Result<Option<ObjectId>, String> {
        Ok(self.main)
    }

    fn agent_head(&self, _agent: usize) -> Result<Option<ObjectId>, String> {
        Ok(self.main)
    }

    fn create_branch(&mut self, root: ObjectId) -> Result<(), String> {
        self.compare_and_swap(None, Some(root))
    }

    fn land(&mut self, request: &LandRequest) -> Result<Landed, String> {
        // The serialized section: nothing else touches the branch until it
        // returns.
        let head = self.main;
        let outcome = land(
            &self.objects,
            head,
            request.change,
            request.committer.clone(),
        )
        .map_err(|error| format!("land: {error}"))?;
        let mut moved = None;
        if !request.crash
            && let Some(new) = target(&outcome)
        {
            self.compare_and_swap(head, Some(new))?;
            moved = Some(new);
        }
        Ok(Landed {
            head,
            outcome,
            moved,
        })
    }

    fn branch_moves(&self) -> Result<Vec<Move>, String> {
        Ok(self.moves.clone())
    }

    fn reopen(&mut self) -> Result<bool, String> {
        Ok(false)
    }
}

/// Why the lander's transaction records nothing.
#[derive(Debug)]
enum Abort {
    /// The outcome does not move the branch.
    Unmoved,
    /// The lander crashed before the compare-and-swap.
    Crash,
    /// The engine failed.
    Engine(braid_engine::Error),
}

/// The real local store in a directory: one handle for the lander and one for
/// each agent, as separate processes would have.
#[derive(Debug)]
pub struct StoreBackend {
    dir: PathBuf,
    options: Options,
    lander: Store,
    agents: Vec<Store>,
}

impl StoreBackend {
    /// Creates a store in `dir` with a handle for each of `agents` agents.
    pub fn create(dir: &Path, agents: usize) -> Result<Self, String> {
        let options = Options {
            // Small, so that runs seal segments and write checkpoints.
            segment_size: 256 << 10,
            checkpoint_interval: 8,
            ..Options::default()
        };
        let lander = Store::init(dir, options.clone()).map_err(|error| error.to_string())?;
        let mut backend = StoreBackend {
            dir: dir.to_path_buf(),
            options,
            lander,
            agents: Vec::new(),
        };
        backend.open_agents(agents)?;
        Ok(backend)
    }

    fn open(&self) -> Result<Store, String> {
        Store::open(&self.dir, self.options.clone()).map_err(|error| error.to_string())
    }

    fn open_agents(&mut self, count: usize) -> Result<(), String> {
        self.agents.clear();
        for _ in 0..count {
            let store = self.open()?;
            self.agents.push(store);
        }
        Ok(())
    }
}

impl Backend for StoreBackend {
    type Objects = Store;

    fn name(&self) -> &'static str {
        "store"
    }

    fn objects(&self) -> &Store {
        &self.lander
    }

    fn agent_objects(&self, agent: usize) -> &Store {
        &self.agents[agent]
    }

    fn agent_sync(&self, agent: usize) -> Result<(), String> {
        self.agents[agent].sync().map_err(|error| error.to_string())
    }

    fn head(&self) -> Result<Option<ObjectId>, String> {
        let refs = self.lander.refs().map_err(|error| error.to_string())?;
        Ok(refs.get(MAIN))
    }

    fn agent_head(&self, agent: usize) -> Result<Option<ObjectId>, String> {
        let refs = self.agents[agent]
            .refs()
            .map_err(|error| error.to_string())?;
        Ok(refs.get(MAIN))
    }

    fn create_branch(&mut self, root: ObjectId) -> Result<(), String> {
        self.lander
            .transact(LANDER, "create main", |refs| {
                Ok::<_, String>(vec![RefUpdate::new(MAIN, refs.get(MAIN), Some(root))])
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn land(&mut self, request: &LandRequest) -> Result<Landed, String> {
        let mut seen = None;
        let lander = &self.lander;
        let result = lander.transact(LANDER, &request.description, |refs| {
            let head = refs.get(MAIN);
            let outcome = land(lander, head, request.change, request.committer.clone())
                .map_err(Abort::Engine)?;
            let new = target(&outcome);
            seen = Some((head, outcome));
            if request.crash {
                return Err(Abort::Crash);
            }
            match new {
                Some(new) => Ok(vec![RefUpdate::new(MAIN, head, Some(new))]),
                None => Err(Abort::Unmoved),
            }
        });
        let landed = |seen: Option<(Option<ObjectId>, LandOutcome)>, moved| {
            let (head, outcome) = seen.ok_or("the transaction did not run its closure")?;
            Ok(Landed {
                head,
                outcome,
                moved,
            })
        };
        match result {
            Ok(op) => {
                let (head, outcome) = seen.ok_or("the transaction did not run its closure")?;
                let moved = target(&outcome);
                let expected = [RefUpdate::new(MAIN, head, moved)];
                if op.updates != expected {
                    return Err(format!(
                        "operation {} records {:?}, not {expected:?}",
                        op.seq, op.updates
                    ));
                }
                Ok(Landed {
                    head,
                    outcome,
                    moved,
                })
            }
            Err(TransactError::Aborted(Abort::Unmoved)) => landed(seen, None),
            Err(TransactError::Aborted(Abort::Crash)) => {
                // The process died: its handle goes, and a new one opens the
                // store again.
                self.lander = self.open()?;
                landed(seen, None)
            }
            Err(TransactError::Aborted(Abort::Engine(error))) => Err(format!("land: {error}")),
            Err(TransactError::Store(error)) => Err(format!("transaction: {error}")),
        }
    }

    fn branch_moves(&self) -> Result<Vec<Move>, String> {
        let mut ops = self
            .lander
            .log(usize::MAX)
            .map_err(|error| error.to_string())?;
        ops.reverse();
        let mut moves = Vec::new();
        for op in ops {
            for update in op.updates {
                if update.name == MAIN {
                    moves.push((update.old, update.new));
                }
            }
        }
        Ok(moves)
    }

    fn reopen(&mut self) -> Result<bool, String> {
        let report = self.lander.fsck().map_err(|error| error.to_string())?;
        if !report.is_clean() {
            return Err(format!("fsck found problems: {:?}", report.problems));
        }
        let agents = self.agents.len();
        self.agents.clear();
        self.lander = self.open()?;
        self.open_agents(agents)?;
        Ok(true)
    }
}
