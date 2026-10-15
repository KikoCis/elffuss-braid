// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The scheduler: agents snapshot the branch, edit, commit and ask the lander
//! to land their changes, in an order the seed decides, with faults
//! injected; every invariant is checked after every step.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::rc::Rc;

use braid_engine::{
    Conflict, ConflictHunk, ConflictKind, LandOutcome, MergeResult, REBASED_FROM, admit, commit,
    diff, is_ancestor, land, merge3, read_changes, read_changeset, read_tree,
};
use braid_format::{
    Change, ChangeOp, Changes, Changeset, EntryKind, Kind, ObjectId, Objects, Signature,
};

use crate::backend::{Backend, LandRequest, MAIN};
use crate::model::{
    self, Edits, Files, Hunk, MergeCase, ModelConflict, Oracle, Rebase, TextMerge, Value, show,
};
use crate::ops::{self, FLAT_LIMIT, Generator, Op, WIDE};
use crate::rng::Rng;
use crate::trace::{Trace, short};

/// How many times an agent redoes a change that keeps conflicting before it
/// gives up on it.
const MAX_ATTEMPTS: u32 = 6;

/// How a run goes. Probabilities are per request or per change.
#[derive(Clone, Debug)]
pub struct Config {
    /// Agents landing changes at the same time.
    pub agents: usize,
    /// Changes each agent sees through before it stops.
    pub tasks: u32,
    /// Probability that a run starts with a directory of about a thousand
    /// files, on either side of the largest flat directory.
    pub wide: f64,
    /// Share of the steps the lander takes while requests wait.
    pub lander_share: f64,
    /// Probability that the lander's answer is lost on its way back.
    pub lost_ack: f64,
    /// Probability that a request reaches the lander twice.
    pub duplicate: f64,
    /// Probability that an agent sends a changeset again after it landed.
    pub resubmit: f64,
    /// Probability that the lander crashes after computing an outcome and
    /// before the compare-and-swap.
    pub crash: f64,
    /// Probability that the lander takes a request out of order.
    pub reorder: f64,
    /// Probability that a new changeset is landed together with another one
    /// built on the same base, in both orders, away from the branch.
    pub commute: f64,
    /// Probability that an agent copies a file from the newest head into its
    /// change.
    pub copy: f64,
    /// Probability that an agent sends a change that changes nothing, rather
    /// than dropping it.
    pub empty: f64,
    /// The shortest time an agent waits for an answer before it sends its
    /// request again; the longest is six times as long.
    pub timeout: u64,
    /// Steps after which an unfinished run fails.
    pub max_steps: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            agents: 8,
            tasks: 10,
            wide: 0.2,
            lander_share: 0.3,
            lost_ack: 0.08,
            duplicate: 0.06,
            resubmit: 0.06,
            crash: 0.05,
            reorder: 0.1,
            commute: 0.35,
            copy: 0.08,
            empty: 0.2,
            timeout: 25,
            max_steps: 200_000,
        }
    }
}

/// Counts of what happened, for one run or summed over many.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Runs.
    pub seeds: u64,
    /// Runs that started with the wide directory.
    pub wide_seeds: u64,
    /// Scheduler steps.
    pub steps: u64,
    /// Changesets agents committed.
    pub commits: u64,
    /// Requests the lander handled, retries and duplicates included.
    pub requests: u64,
    /// Landings that fast-forwarded the branch.
    pub fast_forwards: u64,
    /// Landings that rebased the change onto the head.
    pub rebased: u64,
    /// Paths both sides changed that a landing merged.
    pub merged_paths: u64,
    /// Landings whose change list was too long to be inline.
    pub external_lists: u64,
    /// Requests answered with "already applied".
    pub already_applied: u64,
    /// Requests answered with conflicts.
    pub conflicts: u64,
    /// Conflicts reported, by kind.
    pub conflict_kinds: BTreeMap<String, u64>,
    /// Answers lost on their way back.
    pub lost_acks: u64,
    /// Requests delivered twice.
    pub duplicates: u64,
    /// Changesets sent again after they landed.
    pub resubmits: u64,
    /// Lander crashes between the outcome and the compare-and-swap.
    pub crashes: u64,
    /// Requests sent again after waiting too long for an answer.
    pub retries: u64,
    /// Changes redone on a newer head after a conflict.
    pub redos: u64,
    /// Changes given up after conflicting too often.
    pub abandoned: u64,
    /// Pairs of changes landed in both orders.
    pub commutations: u64,
    /// Of those, pairs that conflicted in both orders.
    pub commuted_conflicting: u64,
    /// Landings that moved the wide directory across the flat limit.
    pub wide_crossings: u64,
}

impl Report {
    /// Landings that moved the branch.
    pub fn landings(&self) -> u64 {
        self.fast_forwards + self.rebased
    }

    /// Faults injected: lost answers, duplicates, resubmissions and crashes.
    pub fn faults(&self) -> u64 {
        self.lost_acks + self.duplicates + self.resubmits + self.crashes
    }

    /// Adds another report's counts to this one.
    pub fn add(&mut self, other: &Report) {
        self.seeds += other.seeds;
        self.wide_seeds += other.wide_seeds;
        self.steps += other.steps;
        self.commits += other.commits;
        self.requests += other.requests;
        self.fast_forwards += other.fast_forwards;
        self.rebased += other.rebased;
        self.merged_paths += other.merged_paths;
        self.external_lists += other.external_lists;
        self.already_applied += other.already_applied;
        self.conflicts += other.conflicts;
        for (kind, count) in &other.conflict_kinds {
            *self.conflict_kinds.entry(kind.clone()).or_default() += count;
        }
        self.lost_acks += other.lost_acks;
        self.duplicates += other.duplicates;
        self.resubmits += other.resubmits;
        self.crashes += other.crashes;
        self.retries += other.retries;
        self.redos += other.redos;
        self.abandoned += other.abandoned;
        self.commutations += other.commutations;
        self.commuted_conflicting += other.commuted_conflicting;
        self.wide_crossings += other.wide_crossings;
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: Vec<String> = self
            .conflict_kinds
            .iter()
            .map(|(kind, count)| format!("{kind} {count}"))
            .collect();
        writeln!(
            f,
            "seeds {} ({} with the wide directory), steps {}, commits {}, requests {}",
            self.seeds, self.wide_seeds, self.steps, self.commits, self.requests
        )?;
        writeln!(
            f,
            "landings {}: {} fast-forward, {} rebased ({} paths merged, {} external change lists)",
            self.landings(),
            self.fast_forwards,
            self.rebased,
            self.merged_paths,
            self.external_lists
        )?;
        writeln!(
            f,
            "already applied {}, conflicts {} ({})",
            self.already_applied,
            self.conflicts,
            kinds.join(", ")
        )?;
        writeln!(
            f,
            "faults {}: {} lost answers, {} duplicate requests, {} resubmissions after landing, \
             {} lander crashes; {} retries after a timeout",
            self.faults(),
            self.lost_acks,
            self.duplicates,
            self.resubmits,
            self.crashes,
            self.retries
        )?;
        write!(
            f,
            "agents redid {} changes after a conflict and gave up {}; {} pairs landed in both \
             orders ({} conflicting both ways); {} crossings of the flat directory limit",
            self.redos,
            self.abandoned,
            self.commutations,
            self.commuted_conflicting,
            self.wide_crossings
        )
    }
}

/// A run that broke an invariant or disagreed with the model.
#[derive(Clone, Debug)]
pub struct Failure {
    /// The seed that reproduces it.
    pub seed: u64,
    /// The backend it ran on.
    pub backend: &'static str,
    /// The step it failed at.
    pub step: u64,
    /// Which check failed: `I1` to `I7`, or another check of the run.
    pub invariant: &'static str,
    /// What went wrong.
    pub message: String,
    /// The events about the changesets involved.
    pub trace: Vec<String>,
    /// Every event of the run.
    pub full_trace: Vec<String>,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "seed {} on the {} backend failed at step {}: {} ({})",
            self.seed,
            self.backend,
            self.step,
            self.invariant,
            meaning(self.invariant)
        )?;
        writeln!(f, "{}", self.message)?;
        writeln!(
            f,
            "replay: BRAID_SIM_SEED={} cargo test -p elffuss-braid-sim --test simulation -- --nocapture",
            self.seed
        )?;
        writeln!(
            f,
            "events about the changesets involved ({} of {}):",
            self.trace.len(),
            self.full_trace.len()
        )?;
        for line in &self.trace {
            writeln!(f, "  {line}")?;
        }
        Ok(())
    }
}

/// What a check name stands for.
fn meaning(invariant: &str) -> &'static str {
    match invariant {
        "I1" => "the head only moves forward, each new head on the old one",
        "I2" => "main's first-parent history is the sequence of landings",
        "I3" => "no lost update: the head's tree is the model's state",
        "I4" => "a changeset lands at most once",
        "I5" => "a conflict leaves the head unchanged",
        "I6" => "every changeset on main is admissible",
        "I7" => "disjoint changes land the same in either order",
        "model" => "the lander's answer differs from the reference model",
        "merge" => "merge3 differs from the model's line merge",
        "commit" => "a committed changeset differs from what the agent wrote",
        "engine" => "the engine or the store failed",
        "liveness" => "the run did not finish",
        _ => "a check of the simulation",
    }
}

/// A successful run.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// What happened.
    pub report: Report,
    /// Every event, as lines.
    pub trace: Vec<String>,
    /// Main's heads, oldest first: the first changeset, then one per landing.
    pub heads: Vec<ObjectId>,
}

/// Runs one seed on `backend`.
pub fn run<B: Backend>(seed: u64, config: &Config, backend: B) -> Result<Outcome, Failure> {
    let mut sim = Sim::new(seed, config.clone(), backend);
    match sim.run() {
        Ok(()) => {
            sim.report.seeds = 1;
            sim.report.steps = sim.steps;
            Ok(Outcome {
                report: sim.report,
                trace: sim.trace.lines(),
                heads: sim.history.entries.iter().map(|entry| entry.head).collect(),
            })
        }
        Err(fail) => Err(sim.failure(fail)),
    }
}

/// A failed check: which, why, and the changesets it is about.
#[derive(Debug)]
struct Fail {
    invariant: &'static str,
    message: String,
    ids: Vec<ObjectId>,
}

fn fail(invariant: &'static str, message: impl Into<String>) -> Fail {
    Fail {
        invariant,
        message: message.into(),
        ids: Vec::new(),
    }
}

impl Fail {
    fn about(mut self, ids: &[ObjectId]) -> Self {
        self.ids.extend_from_slice(ids);
        self
    }
}

/// Turns an error of the engine or a store into a failed check.
fn engine<E: fmt::Display>(what: &'static str) -> impl FnOnce(E) -> Fail {
    move |error| fail("engine", format!("{what}: {error}"))
}

type Checked<T = ()> = Result<T, Fail>;

/// One head of main as the model sees it.
struct MainEntry {
    head: ObjectId,
    /// The request that landed it; `None` for the first changeset.
    request: Option<ObjectId>,
    state: Rc<Files>,
    /// The oracle's root tree of `state`.
    root: ObjectId,
    /// Files in the wide directory.
    wide: usize,
}

/// Main's history as the model sees it.
#[derive(Default)]
struct History {
    entries: Vec<MainEntry>,
    index: BTreeMap<ObjectId, usize>,
    /// Entries up to this one were checked by walking first parents.
    verified: usize,
}

impl History {
    fn push(&mut self, entry: MainEntry) {
        self.index.insert(entry.head, self.entries.len());
        self.entries.push(entry);
    }

    fn last(&self) -> &MainEntry {
        self.entries
            .last()
            .expect("main always has a first changeset")
    }

    fn state(&self, head: ObjectId) -> Checked<Rc<Files>> {
        match self.index.get(&head) {
            Some(at) => Ok(Rc::clone(&self.entries[*at].state)),
            None => {
                Err(fail("I2", format!("{} is not a head main had", short(head))).about(&[head]))
            }
        }
    }

    fn contains(&self, id: &ObjectId) -> bool {
        self.index.contains_key(id)
    }
}

/// What an agent is doing.
enum Phase {
    /// Between changes.
    Idle,
    /// Editing on a snapshot; committing and sending come next.
    Editing(Edit),
    /// Waiting for the lander's answer about a changeset.
    Waiting { change: ObjectId, deadline: u64 },
    /// Done.
    Finished,
}

/// An edit in progress.
struct Edit {
    base: ObjectId,
    base_state: Rc<Files>,
    ops: Vec<Op>,
    state: Files,
    /// Conflicts this change has met so far.
    attempt: u32,
}

/// The lander's answer.
#[derive(Clone, Copy, Debug)]
enum Answer {
    Landed,
    AlreadyApplied,
    Conflicts,
}

struct Reply {
    change: ObjectId,
    answer: Answer,
}

struct Agent {
    name: String,
    phase: Phase,
    wake_at: u64,
    done: u32,
    mailbox: VecDeque<Reply>,
}

/// How a request came to be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delivery {
    First,
    Retry,
    Duplicate,
    Resubmit,
}

impl Delivery {
    fn name(self) -> &'static str {
        match self {
            Delivery::First => "first time",
            Delivery::Retry => "retry after a timeout",
            Delivery::Duplicate => "duplicate delivery",
            Delivery::Resubmit => "again after it landed",
        }
    }
}

struct Message {
    change: ObjectId,
    agent: usize,
    delivery: Delivery,
}

/// A changeset an agent committed and asked to land.
struct Request {
    agent: usize,
    base: ObjectId,
    base_state: Rc<Files>,
    state: Rc<Files>,
    ops: Vec<Op>,
    edits: Edits,
    attempt: u32,
    /// The head of main it became, once it landed.
    landed: Option<ObjectId>,
}

/// What the model says landing a request gives.
enum Expect {
    /// The request landed already, as this head.
    Landed(ObjectId),
    /// The base is the head: the branch moves to the change itself.
    FastForward(Rc<Files>),
    /// The head already has everything the change does.
    Unchanged,
    /// The change is rebased onto the head.
    Rebased {
        state: Files,
        resolved: Edits,
        merged: Vec<Vec<u8>>,
    },
    /// The change conflicts with the head.
    Conflicts {
        conflicts: Vec<ModelConflict>,
        base: Rc<Files>,
        ours: Rc<Files>,
        theirs: Rc<Files>,
    },
}

impl Expect {
    fn describe(&self) -> String {
        match self {
            Expect::Landed(at) => format!("already applied (it landed as {})", short(*at)),
            Expect::FastForward(_) => "a fast-forward".to_owned(),
            Expect::Unchanged => "already applied (the head has everything it does)".to_owned(),
            Expect::Rebased { merged, .. } if merged.is_empty() => "a rebase".to_owned(),
            Expect::Rebased { merged, .. } => format!("a rebase merging {}", show_paths(merged)),
            Expect::Conflicts { conflicts, .. } => format!(
                "conflicts: {}",
                conflicts
                    .iter()
                    .map(|c| format!("{} {}", c.code, show(&c.path)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// A conflict in a form both the engine's and the model's map to.
#[derive(Debug, PartialEq, Eq)]
struct ConflictView {
    path: Vec<u8>,
    code: &'static str,
    detail: Vec<u8>,
    base: Option<EntryKind>,
    ours: Option<EntryKind>,
    theirs: Option<EntryKind>,
    hunks: Vec<Hunk>,
}

impl ConflictView {
    fn of(conflict: &Conflict) -> Self {
        let (detail, hunks) = match &conflict.kind {
            ConflictKind::DirectoryRemoved { directory } => (directory.clone(), Vec::new()),
            ConflictKind::Case { existing } => (existing.clone(), Vec::new()),
            ConflictKind::Content { hunks } => (Vec::new(), hunks.iter().map(hunk).collect()),
            _ => (Vec::new(), Vec::new()),
        };
        ConflictView {
            path: conflict.path.clone(),
            code: conflict.kind.code(),
            detail,
            base: conflict.base.clone(),
            ours: conflict.ours.clone(),
            theirs: conflict.theirs.clone(),
            hunks,
        }
    }

    fn line(&self) -> String {
        let mut line = format!("{} {}", self.code, show(&self.path));
        if !self.detail.is_empty() {
            line.push_str(&format!(" ({})", show(&self.detail)));
        }
        line.push_str(&format!(
            " base {}, ours {}, theirs {}",
            show_entry(self.base.as_ref()),
            show_entry(self.ours.as_ref()),
            show_entry(self.theirs.as_ref())
        ));
        if !self.hunks.is_empty() {
            line.push_str(&format!(", hunks {:?}", self.hunks));
        }
        line
    }
}

fn hunk(hunk: &ConflictHunk) -> Hunk {
    Hunk {
        base: (hunk.base.lines.clone(), hunk.base.text.clone()),
        ours: (hunk.ours.lines.clone(), hunk.ours.text.clone()),
        theirs: (hunk.theirs.lines.clone(), hunk.theirs.text.clone()),
    }
}

struct Sim<B: Backend> {
    seed: u64,
    config: Config,
    rng: Rng,
    generator: Generator,
    backend: B,
    oracle: Oracle,
    history: History,
    agents: Vec<Agent>,
    inbox: VecDeque<Message>,
    requests: BTreeMap<ObjectId, Request>,
    /// Requests by base, for pairs to land in both orders.
    by_base: BTreeMap<ObjectId, Vec<ObjectId>>,
    /// The head of main after the landings so far.
    head: ObjectId,
    /// The head the last full check covered.
    checked: Option<ObjectId>,
    /// Simulated time.
    now: u64,
    steps: u64,
    trace: Trace,
    report: Report,
}

impl<B: Backend> Sim<B> {
    fn new(seed: u64, config: Config, backend: B) -> Self {
        let agents = (0..config.agents)
            .map(|a| Agent {
                name: format!("agent-{a}"),
                phase: Phase::Idle,
                wake_at: 0,
                done: 0,
                mailbox: VecDeque::new(),
            })
            .collect();
        Sim {
            seed,
            config,
            rng: Rng::new(seed),
            generator: Generator::default(),
            backend,
            oracle: Oracle::default(),
            history: History::default(),
            agents,
            inbox: VecDeque::new(),
            requests: BTreeMap::new(),
            by_base: BTreeMap::new(),
            head: ObjectId::from_bytes([0; 32]),
            checked: None,
            now: 0,
            steps: 0,
            trace: Trace::default(),
            report: Report::default(),
        }
    }

    fn event(&mut self, actor: &str, text: String, ids: &[ObjectId]) {
        self.trace.push(self.steps, self.now, actor, text, ids);
    }

    fn agent_event(&mut self, a: usize, text: String, ids: &[ObjectId]) {
        let name = self.agents[a].name.clone();
        self.event(&name, text, ids);
    }

    fn run(&mut self) -> Checked {
        self.setup()?;
        self.check_step()?;
        while !self.finished() {
            if self.steps >= self.config.max_steps {
                return Err(fail(
                    "liveness",
                    format!("not finished after {} steps", self.steps),
                ));
            }
            self.step()?;
            self.check_step()?;
        }
        self.finish()
    }

    fn finished(&self) -> bool {
        self.inbox.is_empty()
            && self
                .agents
                .iter()
                .all(|agent| matches!(agent.phase, Phase::Finished))
    }

    /// Commits the initial state and points main at it.
    fn setup(&mut self) -> Checked {
        let wide = self.rng.chance(self.config.wide);
        if wide {
            self.report.wide_seeds += 1;
        }
        let files = self.generator.initial(&mut self.rng, wide);
        let edits = model::change_list(&Files::new(), &files);
        let author = signature("setup", 0);
        let objects = self.backend.objects();
        for value in files.values() {
            objects
                .put(Kind::Blob, value.content())
                .map_err(engine("store a blob"))?;
        }
        let root = commit(
            objects,
            Vec::new(),
            model::to_changes(&edits),
            author.clone(),
            author,
            b"the initial state\n".to_vec(),
            Vec::new(),
        )
        .map_err(engine("commit the initial state"))?;
        admit(objects, root, |_| false).map_err(|error| {
            fail(
                "I6",
                format!("the first changeset is not admissible: {error}"),
            )
        })?;
        let tree = read_changeset(objects, root)
            .map_err(engine("read the first changeset"))?
            .root;
        let expected = self.oracle.root(&files);
        if tree != expected {
            let differences = describe_tree(objects, tree, &files);
            return Err(fail(
                "commit",
                format!("the first tree differs from the initial state:\n{differences}"),
            ));
        }
        self.backend
            .create_branch(root)
            .map_err(engine("create the branch"))?;
        let count = files.len();
        let wide = wide_count(&files);
        self.history.push(MainEntry {
            head: root,
            request: None,
            state: Rc::new(files),
            root: expected,
            wide,
        });
        self.head = root;
        self.event(
            "setup",
            format!(
                "{MAIN} starts at {} with {count} files, {wide} in the wide directory",
                short(root)
            ),
            &[root],
        );
        for agent in &mut self.agents {
            agent.wake_at = self.rng.below(8) as u64;
        }
        Ok(())
    }

    fn awake(&self) -> Vec<usize> {
        (0..self.agents.len())
            .filter(|&a| {
                let agent = &self.agents[a];
                !matches!(agent.phase, Phase::Finished) && agent.wake_at <= self.now
            })
            .collect()
    }

    /// One step: the lander handles a request, or one agent acts.
    fn step(&mut self) -> Checked {
        self.steps += 1;
        self.now += 1;
        let mut awake = self.awake();
        if awake.is_empty() && self.inbox.is_empty() {
            // Nobody can act yet: skip ahead to the next agent to wake.
            let next = self
                .agents
                .iter()
                .filter(|agent| !matches!(agent.phase, Phase::Finished))
                .map(|agent| agent.wake_at)
                .min()
                .unwrap_or(self.now);
            self.now = self.now.max(next);
            awake = self.awake();
        }
        let lander = !self.inbox.is_empty()
            && (awake.is_empty() || self.rng.chance(self.config.lander_share));
        if lander {
            self.lander_step()
        } else {
            let a = awake[self.rng.below(awake.len())];
            self.agent_step(a)
        }
    }

    fn agent_step(&mut self, a: usize) -> Checked {
        match std::mem::replace(&mut self.agents[a].phase, Phase::Idle) {
            Phase::Idle => self.start_change(a),
            Phase::Editing(edit) => self.commit_change(a, edit),
            Phase::Waiting { change, deadline } => self.wait(a, change, deadline),
            Phase::Finished => {
                self.agents[a].phase = Phase::Finished;
                Ok(())
            }
        }
    }

    /// The head as an agent reads it, which must be main's.
    fn agent_head(&self, a: usize) -> Checked<ObjectId> {
        let head = self
            .backend
            .agent_head(a)
            .map_err(engine("read the head"))?;
        if head != Some(self.head) {
            return Err(fail(
                "I2",
                format!(
                    "{} reads {MAIN} at {head:?}; the landings end at {}",
                    self.agents[a].name,
                    short(self.head)
                ),
            ));
        }
        Ok(self.head)
    }

    /// A pause: mostly short, sometimes long enough for bases to go stale.
    fn think_time(&mut self) -> u64 {
        let pause = match self.rng.below(20) {
            0..=13 => self.rng.below(4),
            14..=18 => self.rng.range(4, 60),
            _ => self.rng.range(60, 400),
        };
        pause as u64
    }

    /// How long an agent waits for an answer before it sends again.
    fn timeout(&mut self) -> u64 {
        let shortest = self.config.timeout.max(1) as usize;
        self.rng.range(shortest, shortest * 6) as u64
    }

    /// Takes a snapshot of main and plans a change on it.
    fn start_change(&mut self, a: usize) -> Checked {
        if self.agents[a].done >= self.config.tasks {
            self.agents[a].phase = Phase::Finished;
            self.agent_event(a, "finished".to_owned(), &[]);
            return Ok(());
        }
        let head = self.agent_head(a)?;
        let base_state = self.history.state(head)?;
        let (ops, state) = self.generator.change(&mut self.rng, &base_state);
        self.agent_event(
            a,
            format!(
                "snapshot of {MAIN} at {}, plans {} edits",
                short(head),
                ops.len()
            ),
            &[head],
        );
        self.agents[a].phase = Phase::Editing(Edit {
            base: head,
            base_state,
            ops,
            state,
            attempt: 0,
        });
        self.agents[a].wake_at = self.now + self.think_time();
        Ok(())
    }

    /// Commits the change on its snapshot and asks the lander to land it.
    fn commit_change(&mut self, a: usize, mut edit: Edit) -> Checked {
        if self.rng.chance(self.config.copy) {
            // Take a file as it is on the newest head.
            let latest = Rc::clone(&self.history.last().state);
            let files: Vec<(&Vec<u8>, &Value)> = latest
                .iter()
                .filter(|(path, _)| !model::inside(WIDE, path))
                .collect();
            if !files.is_empty() {
                let (path, value) = files[self.rng.below(files.len())];
                let op = Op::Write {
                    path: path.clone(),
                    value: value.clone(),
                };
                ops::apply(&mut edit.state, &op);
                edit.ops.push(op);
            }
        }
        let edits = model::change_list(&edit.base_state, &edit.state);
        if edits.is_empty() && !self.rng.chance(self.config.empty) {
            self.agents[a].done += 1;
            self.agents[a].wake_at = self.now + self.think_time();
            self.agent_event(a, "has nothing to change".to_owned(), &[]);
            return Ok(());
        }
        let name = self.agents[a].name.clone();
        let author = signature(&name, self.now);
        let message = format!("{name}: change {}\n", self.agents[a].done + 1).into_bytes();
        let objects = self.backend.agent_objects(a);
        for (_, value) in &edits {
            if let Some(value) = value {
                objects
                    .put(Kind::Blob, value.content())
                    .map_err(engine("store a blob"))?;
            }
        }
        let change = commit(
            objects,
            vec![edit.base],
            model::to_changes(&edits),
            author.clone(),
            author,
            message,
            Vec::new(),
        )
        .map_err(engine("commit"))?;
        self.backend
            .agent_sync(a)
            .map_err(engine("sync an agent's objects"))?;
        if self.requests.contains_key(&change) {
            return Err(fail(
                "commit",
                format!("{} was committed twice", short(change)),
            ));
        }
        self.report.commits += 1;
        let summary = show_edits(&edit.base_state, &edits);
        self.agent_event(
            a,
            format!(
                "commit {} on {}: {summary}",
                short(change),
                short(edit.base)
            ),
            &[change, edit.base],
        );
        // Changes to the wide directory make every check read and rebuild
        // it; check a quarter of them.
        if !touches_wide(&edits) || self.rng.chance(0.25) {
            self.check_commit(change, edit.base, &edit.state, &edits)?;
        }
        self.requests.insert(
            change,
            Request {
                agent: a,
                base: edit.base,
                base_state: edit.base_state,
                state: Rc::new(edit.state),
                ops: edit.ops,
                edits,
                attempt: edit.attempt,
                landed: None,
            },
        );
        self.by_base.entry(edit.base).or_default().push(change);
        if self.rng.chance(self.config.commute) {
            self.commute(change)?;
        }
        self.send(a, change, Delivery::First);
        let deadline = self.now + self.timeout();
        self.agents[a].phase = Phase::Waiting { change, deadline };
        self.agents[a].wake_at = deadline;
        Ok(())
    }

    /// What a store checks before it accepts a changeset, and that the
    /// changeset is what the agent meant.
    fn check_commit(
        &mut self,
        change: ObjectId,
        base: ObjectId,
        state: &Files,
        edits: &Edits,
    ) -> Checked {
        let objects = self.backend.objects();
        admit(objects, change, |id| self.history.contains(id)).map_err(|error| {
            fail(
                "I6",
                format!(
                    "the committed changeset {} is not admissible: {error}",
                    short(change)
                ),
            )
            .about(&[change])
        })?;
        let changeset = read_changeset(objects, change).map_err(engine("read a changeset"))?;
        let expected = self.oracle.root(state);
        if changeset.root != expected {
            let differences = describe_tree(objects, changeset.root, state);
            return Err(fail(
                "commit",
                format!(
                    "the tree of {} differs from the agent's files:\n{differences}",
                    short(change)
                ),
            )
            .about(&[change]));
        }
        let base_root = read_changeset(objects, base)
            .map_err(engine("read a changeset"))?
            .root;
        let listed = diff(objects, base_root, changeset.root).map_err(engine("diff"))?;
        let expected = model::to_changes(edits);
        if listed != expected {
            return Err(fail(
                "commit",
                format!(
                    "diff of {} against its base differs from its change list:\n  diff: {}\n  list: {}",
                    short(change),
                    show_changes(&listed),
                    show_changes(&expected)
                ),
            )
            .about(&[change]));
        }
        Ok(())
    }

    /// Puts a request in the lander's inbox; the network may deliver it
    /// twice.
    fn send(&mut self, a: usize, change: ObjectId, delivery: Delivery) {
        self.inbox.push_back(Message {
            change,
            agent: a,
            delivery,
        });
        self.agent_event(
            a,
            format!("asks to land {} ({})", short(change), delivery.name()),
            &[change],
        );
        if matches!(delivery, Delivery::First | Delivery::Retry)
            && self.rng.chance(self.config.duplicate)
        {
            self.report.duplicates += 1;
            self.inbox.push_back(Message {
                change,
                agent: a,
                delivery: Delivery::Duplicate,
            });
            self.agent_event(
                a,
                format!("the request for {} is delivered twice", short(change)),
                &[change],
            );
        }
    }

    /// Handles the answers about the changeset the agent waits for, or sends
    /// it again once the wait is over.
    fn wait(&mut self, a: usize, change: ObjectId, deadline: u64) -> Checked {
        while let Some(reply) = self.agents[a].mailbox.pop_front() {
            if reply.change != change {
                continue;
            }
            return match reply.answer {
                Answer::Landed | Answer::AlreadyApplied => {
                    self.agents[a].done += 1;
                    self.agents[a].wake_at = self.now + self.think_time();
                    if self.rng.chance(self.config.resubmit) {
                        self.report.resubmits += 1;
                        self.send(a, change, Delivery::Resubmit);
                    }
                    Ok(())
                }
                Answer::Conflicts => self.redo(a, change),
            };
        }
        let deadline = if self.now >= deadline {
            self.report.retries += 1;
            self.send(a, change, Delivery::Retry);
            self.now + self.timeout()
        } else {
            deadline
        };
        self.agents[a].phase = Phase::Waiting { change, deadline };
        self.agents[a].wake_at = deadline;
        Ok(())
    }

    /// After a conflict: updates to the new head and redoes the edit there,
    /// or puts the change's own files over the head's.
    fn redo(&mut self, a: usize, change: ObjectId) -> Checked {
        let request = &self.requests[&change];
        let attempt = request.attempt + 1;
        let (ops, edits) = (request.ops.clone(), request.edits.clone());
        if attempt > MAX_ATTEMPTS {
            self.report.abandoned += 1;
            self.agents[a].done += 1;
            self.agent_event(
                a,
                format!("gives up on {} after {attempt} conflicts", short(change)),
                &[change],
            );
            return Ok(());
        }
        let head = self.agent_head(a)?;
        let base_state = self.history.state(head)?;
        let (how, ops) = if self.rng.chance(0.4) {
            let ops = edits
                .into_iter()
                .map(|(path, value)| match value {
                    Some(value) => Op::Write { path, value },
                    None => Op::Delete { path },
                })
                .collect();
            ("puts its own files over", ops)
        } else {
            ("redoes its edits on", self.generator.refresh(&ops))
        };
        let mut state = (*base_state).clone();
        for op in &ops {
            ops::apply(&mut state, op);
        }
        self.report.redos += 1;
        self.agent_event(
            a,
            format!("{} conflicted; {how} {}", short(change), short(head)),
            &[change, head],
        );
        self.agents[a].phase = Phase::Editing(Edit {
            base: head,
            base_state,
            ops,
            state,
            attempt,
        });
        self.agents[a].wake_at = self.now + self.think_time();
        Ok(())
    }

    /// The lander takes a request and runs its serialized section.
    fn lander_step(&mut self) -> Checked {
        let at = if self.inbox.len() > 1 && self.rng.chance(self.config.reorder) {
            self.rng.below(self.inbox.len())
        } else {
            0
        };
        let message = self
            .inbox
            .remove(at)
            .expect("the lander runs only when requests wait");
        let change = message.change;
        let crash = self.rng.chance(self.config.crash);
        let lost = !crash && self.rng.chance(self.config.lost_ack);
        let head = self.head;
        self.report.requests += 1;

        let expect = self.expect(change, head)?;
        let request = LandRequest {
            change,
            committer: signature("lander", self.now),
            crash,
            description: format!("land {}", short(change)),
        };
        let landed = self.backend.land(&request).map_err(|error| {
            fail("engine", format!("landing {}: {error}", short(change))).about(&[change])
        })?;
        if landed.head != Some(head) {
            return Err(fail(
                "I1",
                format!(
                    "the lander read the head {:?}; the landings end at {}",
                    landed.head,
                    short(head)
                ),
            ));
        }
        let summary = show_outcome(&landed.outcome);
        let requester = self.agents[message.agent].name.clone();
        let text = format!(
            "{} from {requester} ({}) on {}: {summary}",
            short(change),
            message.delivery.name(),
            short(head)
        );
        let verdict = match self.judge(head, change, expect, &landed.outcome) {
            Ok(verdict) => verdict,
            Err(failed) => {
                self.event("lander", text, &[change, head]);
                return Err(failed.about(&[change, head]));
            }
        };
        let moves_to = if crash {
            None
        } else {
            verdict.as_ref().map(|(id, _)| *id)
        };
        let mut ids = vec![change, head];
        ids.extend(verdict.as_ref().map(|(id, _)| *id));
        let note = if crash {
            "; crashed before the compare-and-swap"
        } else if lost {
            "; the answer is lost"
        } else {
            ""
        };
        self.event("lander", format!("{text}{note}"), &ids);
        if landed.moved != moves_to {
            let invariant = if matches!(landed.outcome, LandOutcome::Conflicts(_)) {
                "I5"
            } else {
                "I1"
            };
            return Err(fail(
                invariant,
                format!(
                    "the lander moved {MAIN} to {:?} after deciding {summary}",
                    landed.moved
                ),
            )
            .about(&[change, head]));
        }
        if crash {
            self.report.crashes += 1;
            return Ok(());
        }
        let answer = match &landed.outcome {
            LandOutcome::FastForward(_) => {
                self.report.fast_forwards += 1;
                Answer::Landed
            }
            LandOutcome::Rebased { merged_paths, .. } => {
                self.report.rebased += 1;
                self.report.merged_paths += merged_paths.len() as u64;
                Answer::Landed
            }
            LandOutcome::AlreadyApplied => {
                self.report.already_applied += 1;
                Answer::AlreadyApplied
            }
            LandOutcome::Conflicts(conflicts) => {
                self.report.conflicts += 1;
                for conflict in conflicts {
                    *self
                        .report
                        .conflict_kinds
                        .entry(conflict.kind.code().to_owned())
                        .or_default() += 1;
                }
                Answer::Conflicts
            }
        };
        if let Some((new, state)) = verdict {
            self.record_landing(head, change, new, state)?;
        }
        if lost {
            self.report.lost_acks += 1;
        } else {
            let now = self.now;
            let agent = &mut self.agents[message.agent];
            agent.mailbox.push_back(Reply { change, answer });
            agent.wake_at = agent.wake_at.min(now + 1);
        }
        Ok(())
    }

    /// What the model says landing `change` on `head` gives.
    fn expect(&mut self, change: ObjectId, head: ObjectId) -> Checked<Expect> {
        let request = &self.requests[&change];
        if let Some(at) = request.landed {
            return Ok(Expect::Landed(at));
        }
        if request.base == head {
            return Ok(Expect::FastForward(Rc::clone(&request.state)));
        }
        let base = Rc::clone(&request.base_state);
        let theirs = Rc::clone(&request.state);
        let ours = self.history.state(head)?;
        predict(&base, &ours, &theirs)
    }

    /// Compares what the lander decided with what the model expects, and
    /// returns where the branch moves.
    fn judge(
        &mut self,
        head: ObjectId,
        change: ObjectId,
        expect: Expect,
        outcome: &LandOutcome,
    ) -> Checked<Option<(ObjectId, Rc<Files>)>> {
        match (expect, outcome) {
            (Expect::Landed(_) | Expect::Unchanged, LandOutcome::AlreadyApplied) => Ok(None),
            (Expect::Landed(at), other) => Err(fail(
                "I4",
                format!(
                    "{} landed already, as {}, and landing it again gave {}",
                    short(change),
                    short(at),
                    show_outcome(other)
                ),
            )
            .about(&[at])),
            (Expect::FastForward(state), LandOutcome::FastForward(id)) if *id == change => {
                Ok(Some((change, state)))
            }
            (
                Expect::Rebased {
                    state,
                    resolved,
                    merged,
                },
                LandOutcome::Rebased {
                    changeset,
                    merged_paths,
                },
            ) => {
                self.check_rebased(head, change, *changeset, &state, &resolved)?;
                if *merged_paths != merged {
                    return Err(fail(
                        "model",
                        format!(
                            "the engine merged {}, the model {}",
                            show_paths(merged_paths),
                            show_paths(&merged)
                        ),
                    ));
                }
                Ok(Some((*changeset, Rc::new(state))))
            }
            (
                Expect::Conflicts {
                    conflicts,
                    base,
                    ours,
                    theirs,
                },
                LandOutcome::Conflicts(found),
            ) => {
                self.check_conflicts(&conflicts, &base, &ours, &theirs, found)?;
                Ok(None)
            }
            (expect, outcome) => Err(fail(
                "model",
                format!(
                    "the model expects {}, the lander answered {}",
                    expect.describe(),
                    show_outcome(outcome)
                ),
            )),
        }
    }

    /// A rebased changeset: on the head, naming the change it came from, with
    /// the model's change list and tree.
    fn check_rebased(
        &mut self,
        head: ObjectId,
        change: ObjectId,
        new: ObjectId,
        state: &Files,
        resolved: &Edits,
    ) -> Checked {
        let objects = self.backend.objects();
        let changeset = read_changeset(objects, new).map_err(engine("read a changeset"))?;
        if changeset.parents.first() != Some(&head) {
            return Err(fail(
                "I1",
                format!(
                    "the rebased changeset {} has the parents {}, not the head {} first",
                    short(new),
                    show_ids(&changeset.parents),
                    short(head)
                ),
            ));
        }
        if rebased_from(&changeset) != Some(change) {
            return Err(fail(
                "I4",
                format!(
                    "the rebased changeset {} does not name {} as the one it was rebased from",
                    short(new),
                    short(change)
                ),
            ));
        }
        let list = read_changes(objects, new, &changeset).map_err(engine("read a change list"))?;
        let expected = model::to_changes(resolved);
        if list.as_ref() != expected.as_slice() {
            return Err(fail(
                "model",
                format!(
                    "the rebased change list differs from the model's:\n  engine: {}\n  model: {}",
                    show_changes(&list),
                    show_changes(&expected)
                ),
            ));
        }
        let root = self.oracle.root(state);
        if changeset.root != root {
            let differences = describe_tree(objects, changeset.root, state);
            return Err(fail(
                "I3",
                format!(
                    "the tree of the rebased changeset {} differs from the model's state:\n{differences}",
                    short(new)
                ),
            ));
        }
        Ok(())
    }

    /// The engine's conflicts, compared with the model's: paths, kinds, the
    /// entries on each side, and the hunks.
    fn check_conflicts(
        &mut self,
        expected: &[ModelConflict],
        base: &Files,
        ours: &Files,
        theirs: &Files,
        found: &[Conflict],
    ) -> Checked {
        let mut want: Vec<ConflictView> = expected
            .iter()
            .map(|conflict| ConflictView {
                path: conflict.path.clone(),
                code: conflict.code,
                detail: conflict.detail.clone(),
                base: self.oracle.entry_at(base, &conflict.path),
                ours: self.oracle.entry_at(ours, &conflict.path),
                theirs: self.oracle.entry_at(theirs, &conflict.path),
                hunks: conflict.hunks.clone(),
            })
            .collect();
        let mut got: Vec<ConflictView> = found.iter().map(ConflictView::of).collect();
        let key = |view: &ConflictView| (view.path.clone(), view.code, view.detail.clone());
        want.sort_by_key(key);
        got.sort_by_key(key);
        if want == got {
            return Ok(());
        }
        let lines = |views: &[ConflictView]| {
            views
                .iter()
                .map(|view| format!("\n    {}", view.line()))
                .collect::<String>()
        };
        Err(fail(
            "model",
            format!(
                "the conflicts differ\n  engine:{}\n  model:{}",
                lines(&got),
                lines(&want)
            ),
        ))
    }

    /// Main moved from `old` to `new` for `change`: check the move and record
    /// it.
    fn record_landing(
        &mut self,
        old: ObjectId,
        change: ObjectId,
        new: ObjectId,
        state: Rc<Files>,
    ) -> Checked {
        let objects = self.backend.objects();
        let changeset = read_changeset(objects, new).map_err(engine("read a changeset"))?;
        if changeset.parents.first() != Some(&old) {
            return Err(fail(
                "I1",
                format!(
                    "the new head {} has the parents {}, not the old head {} first",
                    short(new),
                    show_ids(&changeset.parents),
                    short(old)
                ),
            )
            .about(&[new]));
        }
        if !is_ancestor(objects, old, new).map_err(engine("ancestry"))? {
            return Err(fail(
                "I1",
                format!(
                    "the old head {} is not an ancestor of the new one",
                    short(old)
                ),
            )
            .about(&[new]));
        }
        if let Some(at) = self.requests[&change].landed {
            return Err(fail(
                "I4",
                format!(
                    "{} landed a second time, as {}; it landed before as {}",
                    short(change),
                    short(new),
                    short(at)
                ),
            )
            .about(&[new, at]));
        }
        let origin = if new == change {
            Some(change)
        } else {
            rebased_from(&changeset)
        };
        if origin != Some(change) {
            return Err(fail(
                "I4",
                format!(
                    "the new head {} does not say it lands {}",
                    short(new),
                    short(change)
                ),
            )
            .about(&[new]));
        }
        admit(objects, new, |id| self.history.contains(id)).map_err(|error| {
            fail(
                "I6",
                format!("the new head {} is not admissible: {error}", short(new)),
            )
            .about(&[new])
        })?;
        if matches!(changeset.changes, Changes::External { .. }) {
            self.report.external_lists += 1;
        }
        let wide = wide_count(&state);
        if (self.history.last().wide > FLAT_LIMIT) != (wide > FLAT_LIMIT) {
            self.report.wide_crossings += 1;
        }
        let root = self.oracle.root(&state);
        self.history.push(MainEntry {
            head: new,
            request: Some(change),
            state,
            root,
            wide,
        });
        if let Some(request) = self.requests.get_mut(&change) {
            request.landed = Some(new);
        }
        self.head = new;
        Ok(())
    }

    /// After every step: main is where the landings left it, its history is
    /// the landings, and its tree is the model's state.
    fn check_step(&mut self) -> Checked {
        let actual = self.backend.head().map_err(engine("read the head"))?;
        if actual != Some(self.head) {
            return Err(fail(
                "I2",
                format!(
                    "{MAIN} is at {actual:?}, but the landings end at {}",
                    short(self.head)
                ),
            )
            .about(&[self.head]));
        }
        if self.checked == Some(self.head) {
            return Ok(());
        }
        self.walk_new_landings()?;
        let objects = self.backend.objects();
        let root = read_changeset(objects, self.head)
            .map_err(engine("read the head"))?
            .root;
        let entry = self.history.last();
        if root != entry.root {
            let differences = describe_tree(objects, root, &entry.state);
            return Err(fail(
                "I3",
                format!("the head's tree differs from the model's state:\n{differences}"),
            )
            .about(&[self.head]));
        }
        self.checked = Some(self.head);
        Ok(())
    }

    /// Walks first parents from the head down to the head checked last; the
    /// changesets met must be the landings since.
    fn walk_new_landings(&mut self) -> Checked {
        let objects = self.backend.objects();
        let entries = &self.history.entries;
        let verified = self.history.verified;
        let mut current = self.head;
        for at in (verified + 1..entries.len()).rev() {
            if current != entries[at].head {
                return Err(fail(
                    "I2",
                    format!(
                        "first parents from the head reach {} where landing {at} put {}",
                        short(current),
                        short(entries[at].head)
                    ),
                )
                .about(&[current, entries[at].head]));
            }
            let changeset = read_changeset(objects, current).map_err(engine("read a changeset"))?;
            current = match changeset.parents.first() {
                Some(parent) => *parent,
                None => {
                    return Err(fail(
                        "I2",
                        format!("{} has no parent, yet it is landing {at}", short(current)),
                    ));
                }
            };
        }
        if current != entries[verified].head {
            return Err(fail(
                "I2",
                format!(
                    "first parents from the head reach {} where main had {}",
                    short(current),
                    short(entries[verified].head)
                ),
            ));
        }
        self.history.verified = entries.len() - 1;
        Ok(())
    }

    /// Lands a new changeset and another one built on the same base, by
    /// another agent and on unrelated paths, in both orders, away from the
    /// branch: both orders must agree.
    fn commute(&mut self, x: ObjectId) -> Checked {
        let request = &self.requests[&x];
        let base = request.base;
        let peer = self.by_base[&base].iter().rev().copied().find(|y| {
            let other = &self.requests[y];
            *y != x && other.agent != request.agent && unrelated(&request.edits, &other.edits)
        });
        let Some(y) = peer else {
            return Ok(());
        };
        // Pairs that touch the wide directory read and rebuild it several
        // times each; replay a quarter of them.
        if (touches_wide(&request.edits) || touches_wide(&self.requests[&y].edits))
            && !self.rng.chance(0.25)
        {
            return Ok(());
        }
        let base_state = Rc::clone(&request.base_state);
        let x_state = Rc::clone(&request.state);
        let y_state = Rc::clone(&self.requests[&y].state);
        let x_then_y = self.replay(base, (x, &x_state), (y, &y_state), &base_state)?;
        let y_then_x = self.replay(base, (y, &y_state), (x, &x_state), &base_state)?;
        self.report.commutations += 1;
        let both = [x, y, base];
        let text = match (x_then_y, y_then_x) {
            (Some(a), Some(b)) if a != b => {
                return Err(fail(
                    "I7",
                    format!(
                        "{} and {} land in both orders but give the trees {} and {}",
                        short(x),
                        short(y),
                        short(a),
                        short(b)
                    ),
                )
                .about(&both));
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(fail(
                    "I7",
                    format!(
                        "{} and {} land in one order and conflict in the other ({} first: {}, {} first: {})",
                        short(x),
                        short(y),
                        short(x),
                        landed_or_not(x_then_y),
                        short(y),
                        landed_or_not(y_then_x)
                    ),
                )
                .about(&both));
            }
            (Some(_), Some(_)) => "give the same tree in either order",
            (None, None) => {
                self.report.commuted_conflicting += 1;
                "conflict in either order"
            }
        };
        self.event(
            "check",
            format!("{} and {} on {} {text}", short(x), short(y), short(base)),
            &both,
        );
        Ok(())
    }

    /// Lands `first` on `base`, then `second` on it, and returns the tree,
    /// or `None` if the second conflicts.
    fn replay(
        &mut self,
        base: ObjectId,
        first: (ObjectId, &Rc<Files>),
        second: (ObjectId, &Rc<Files>),
        base_state: &Rc<Files>,
    ) -> Checked<Option<ObjectId>> {
        let committer = signature("replay", self.now);
        let objects = self.backend.objects();
        match land(objects, Some(base), first.0, committer.clone()) {
            Ok(LandOutcome::FastForward(id)) if id == first.0 => {}
            Ok(other) => {
                return Err(fail(
                    "model",
                    format!(
                        "{} built on {} lands on it as {}, not as a fast-forward",
                        short(first.0),
                        short(base),
                        show_outcome(&other)
                    ),
                ));
            }
            Err(error) => return Err(engine("land")(error)),
        }
        let outcome = land(objects, Some(first.0), second.0, committer).map_err(engine("land"))?;
        let expect = predict(base_state, first.1, second.1)?;
        self.judge(first.0, second.0, expect, &outcome)
            .map_err(|failed| failed.about(&[first.0, second.0]))?;
        let objects = self.backend.objects();
        let root_of = |id: ObjectId| -> Checked<ObjectId> {
            Ok(read_changeset(objects, id)
                .map_err(engine("read a changeset"))?
                .root)
        };
        match &outcome {
            LandOutcome::Rebased { changeset, .. } => Ok(Some(root_of(*changeset)?)),
            LandOutcome::AlreadyApplied => Ok(Some(root_of(first.0)?)),
            _ => Ok(None),
        }
    }

    /// Checks at the end of a run, after the store is opened again: the op
    /// log moved main through exactly the landings, main's whole history is
    /// those landings, each request at most once, every changeset admissible,
    /// and the last tree is the model's.
    fn finish(&mut self) -> Checked {
        let heads: Vec<ObjectId> = self.history.entries.iter().map(|e| e.head).collect();
        let mut expected = vec![(None, Some(heads[0]))];
        expected.extend(heads.windows(2).map(|pair| (Some(pair[0]), Some(pair[1]))));
        let moves = self
            .backend
            .branch_moves()
            .map_err(engine("read the op log"))?;
        if moves != expected {
            return Err(fail(
                "I2",
                format!(
                    "the op log moved {MAIN} {} times; there were {} landings",
                    moves.len().saturating_sub(1),
                    expected.len() - 1
                ),
            ));
        }
        let reopened = self
            .backend
            .reopen()
            .map_err(engine("check and reopen the store"))?;
        let head = self.backend.head().map_err(engine("read the head"))?;
        if head != Some(self.head) {
            return Err(fail(
                "I2",
                format!(
                    "after reopening, {MAIN} is at {head:?}, not {}",
                    short(self.head)
                ),
            ));
        }

        let objects = self.backend.objects();
        let mut chain = Vec::new();
        let mut current = Some(self.head);
        while let Some(id) = current {
            chain.push(id);
            if chain.len() > heads.len() {
                break;
            }
            current = read_changeset(objects, id)
                .map_err(engine("read a changeset"))?
                .parents
                .first()
                .copied();
        }
        chain.reverse();
        if chain != heads {
            return Err(fail(
                "I2",
                format!(
                    "main's first-parent history has {} changesets; there were {} landings and the first changeset",
                    chain.len(),
                    heads.len() - 1
                ),
            ));
        }
        let mut origins = BTreeSet::new();
        for entry in &self.history.entries {
            // Each was admitted when it landed; a store opened again must
            // still hold everything that took.
            if reopened {
                admit(objects, entry.head, |id| self.history.contains(id)).map_err(|error| {
                    fail(
                        "I6",
                        format!("{} on main is not admissible: {error}", short(entry.head)),
                    )
                })?;
            }
            let Some(request) = entry.request else {
                continue;
            };
            let changeset =
                read_changeset(objects, entry.head).map_err(engine("read a changeset"))?;
            let origin = if self.requests.contains_key(&entry.head) {
                Some(entry.head)
            } else {
                rebased_from(&changeset)
            };
            if origin != Some(request) || !origins.insert(request) {
                return Err(fail(
                    "I4",
                    format!(
                        "{} on main lands {:?}; the model has it landing {} once",
                        short(entry.head),
                        origin.map(short),
                        short(request)
                    ),
                )
                .about(&[entry.head, request]));
            }
        }
        let root = read_changeset(objects, self.head)
            .map_err(engine("read the head"))?
            .root;
        let last = self.history.last();
        if root != last.root {
            let differences = describe_tree(objects, root, &last.state);
            return Err(fail(
                "I3",
                format!("after reopening, the head's tree differs from the model:\n{differences}"),
            ));
        }
        self.event(
            "check",
            format!(
                "done: {} landings, history, op log and store check out",
                heads.len() - 1
            ),
            &[],
        );
        Ok(())
    }

    /// The report for a failed check, with the events that matter.
    fn failure(&self, fail: Fail) -> Failure {
        let mut ids: BTreeSet<ObjectId> = fail.ids.iter().copied().collect();
        // The requests involved and their bases, and the landings since the
        // oldest of those bases that touched paths related to theirs.
        let mut paths: Vec<&Vec<u8>> = Vec::new();
        let mut oldest: Option<usize> = None;
        let mut consider = |at: Option<&usize>| {
            if let Some(&at) = at {
                oldest = Some(oldest.map_or(at, |oldest: usize| oldest.min(at)));
            }
        };
        for id in &fail.ids {
            if let Some(request) = self.requests.get(id) {
                ids.insert(request.base);
                paths.extend(request.edits.iter().map(|(path, _)| path));
                consider(self.history.index.get(&request.base));
            }
            consider(self.history.index.get(id));
        }
        if let Some(from) = oldest {
            let entries = &self.history.entries;
            for at in from + 1..entries.len() {
                let touched = model::change_list(&entries[at - 1].state, &entries[at].state);
                let related = paths.is_empty()
                    || touched
                        .iter()
                        .any(|(path, _)| paths.iter().any(|other| model::related(other, path)));
                if related || at + 1 == entries.len() {
                    ids.insert(entries[at].head);
                    ids.extend(entries[at].request);
                }
            }
        }
        Failure {
            seed: self.seed,
            backend: self.backend.name(),
            step: self.steps,
            invariant: fail.invariant,
            message: fail.message,
            trace: self.trace.about(&ids, 12),
            full_trace: self.trace.lines(),
        }
    }
}

/// What the model says landing the change from `base` to `theirs` on the
/// head `ours` gives, with every text merge it made checked against the
/// engine's `merge3`.
fn predict(base: &Rc<Files>, ours: &Rc<Files>, theirs: &Rc<Files>) -> Checked<Expect> {
    let mut merges = Vec::new();
    let rebase =
        model::rebase(base, ours, theirs, &mut merges).map_err(|why| fail("model", why))?;
    for case in &merges {
        check_merge3(case)?;
    }
    Ok(match rebase {
        Rebase::Clean { resolved, .. } if resolved.is_empty() => Expect::Unchanged,
        Rebase::Clean {
            state,
            resolved,
            merged,
        } => Expect::Rebased {
            state,
            resolved,
            merged,
        },
        Rebase::Conflicts(conflicts) => Expect::Conflicts {
            conflicts,
            base: Rc::clone(base),
            ours: Rc::clone(ours),
            theirs: Rc::clone(theirs),
        },
    })
}

/// The engine's `merge3` on the inputs of a model merge must give the same
/// text, or the same conflicting regions.
fn check_merge3(case: &MergeCase) -> Checked {
    let engine = merge3(&case.base, &case.ours, &case.theirs);
    let same = match (&engine, &case.result) {
        (MergeResult::Clean(a), TextMerge::Clean(b)) => a == b,
        (MergeResult::Conflict { hunks }, TextMerge::Conflict(expected)) => {
            hunks.iter().map(hunk).collect::<Vec<_>>() == *expected
        }
        _ => false,
    };
    if same {
        return Ok(());
    }
    let text = |bytes: &[u8]| format!("{:?}", String::from_utf8_lossy(bytes));
    Err(fail(
        "merge",
        format!(
            "merging {}:\n  base {}\n  ours {}\n  theirs {}\n  merge3: {engine:?}\n  model: {:?}",
            show(&case.path),
            text(&case.base),
            text(&case.ours),
            text(&case.theirs),
            case.result
        ),
    ))
}

/// The changeset a rebased changeset names as its origin: its one
/// `rebased-from` field.
fn rebased_from(changeset: &Changeset) -> Option<ObjectId> {
    let mut fields = changeset
        .extras
        .iter()
        .filter(|(key, _)| key.as_slice() == REBASED_FROM);
    let (_, value) = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// Whether no path of one change list is, or is inside, a path of the other.
fn unrelated(a: &Edits, b: &Edits) -> bool {
    a.iter()
        .all(|(p, _)| b.iter().all(|(q, _)| !model::related(p, q)))
}

/// Whether a change list touches the wide directory.
fn touches_wide(edits: &Edits) -> bool {
    edits.iter().any(|(path, _)| model::inside(WIDE, path))
}

fn wide_count(files: &Files) -> usize {
    model::files_below(files, WIDE).count()
}

fn signature(name: &str, time: u64) -> Signature {
    Signature {
        name: name.as_bytes().to_vec(),
        email: format!("{name}@example.org").into_bytes(),
        seconds: 1_700_000_000 + time as i64,
        offset_minutes: 0,
    }
}

fn landed_or_not(tree: Option<ObjectId>) -> &'static str {
    if tree.is_some() { "lands" } else { "conflicts" }
}

fn show_ids(ids: &[ObjectId]) -> String {
    let shown: Vec<String> = ids.iter().map(|id| short(*id)).collect();
    format!("[{}]", shown.join(", "))
}

fn show_paths(paths: &[Vec<u8>]) -> String {
    let shown: Vec<String> = paths.iter().map(|path| show(path)).collect();
    format!("[{}]", shown.join(", "))
}

fn show_outcome(outcome: &LandOutcome) -> String {
    match outcome {
        LandOutcome::FastForward(id) => format!("fast-forward to {}", short(*id)),
        LandOutcome::Rebased {
            changeset,
            merged_paths,
        } if merged_paths.is_empty() => format!("rebased as {}", short(*changeset)),
        LandOutcome::Rebased {
            changeset,
            merged_paths,
        } => format!(
            "rebased as {}, merging {}",
            short(*changeset),
            show_paths(merged_paths)
        ),
        LandOutcome::AlreadyApplied => "already applied".to_owned(),
        LandOutcome::Conflicts(conflicts) => {
            let shown: Vec<String> = conflicts
                .iter()
                .map(|c| format!("{} {}", c.kind.code(), show(&c.path)))
                .collect();
            format!("conflicts: {}", shown.join(", "))
        }
    }
}

/// A change list for traces: `+` for a new file, `~` for a changed one, `-`
/// for a deleted one; long lists are cut short.
fn show_edits(base: &Files, edits: &Edits) -> String {
    const SHOWN: usize = 6;
    let mut parts: Vec<String> = edits
        .iter()
        .take(SHOWN)
        .map(|(path, value)| {
            let sign = match (base.contains_key(path), value) {
                (_, None) => '-',
                (true, Some(_)) => '~',
                (false, Some(_)) => '+',
            };
            format!("{sign}{}", show(path))
        })
        .collect();
    if edits.len() > SHOWN {
        parts.push(format!("and {} more", edits.len() - SHOWN));
    }
    if parts.is_empty() {
        return "no changes".to_owned();
    }
    parts.join(" ")
}

fn show_changes(changes: &[Change]) -> String {
    let shown: Vec<String> = changes
        .iter()
        .map(|change| match &change.op {
            ChangeOp::Delete => format!("-{}", show(&change.path)),
            op => format!(
                "{}={}",
                show(&change.path),
                show_entry(op.to_entry().as_ref())
            ),
        })
        .collect();
    format!("[{}]", shown.join(", "))
}

fn show_entry(entry: Option<&EntryKind>) -> String {
    match entry {
        None => "nothing".to_owned(),
        Some(EntryKind::File { blob, size }) => format!("file {} ({size} bytes)", short(*blob)),
        Some(EntryKind::Executable { blob, size }) => {
            format!("executable {} ({size} bytes)", short(*blob))
        }
        Some(EntryKind::Symlink { blob, .. }) => format!("symlink {}", short(*blob)),
        Some(EntryKind::Tree { tree }) => format!("directory {}", short(*tree)),
        Some(EntryKind::Gitlink { .. }) => "gitlink".to_owned(),
    }
}

/// Every file of a tree, by path.
fn tree_files<O: Objects>(
    objects: &O,
    root: ObjectId,
) -> Result<BTreeMap<Vec<u8>, EntryKind>, braid_engine::Error> {
    let mut files = BTreeMap::new();
    let mut pending = vec![(Vec::new(), root)];
    while let Some((dir, tree)) = pending.pop() {
        for entry in read_tree(objects, tree)? {
            let path = model::join(&dir, &entry.name);
            match entry.kind {
                EntryKind::Tree { tree } => pending.push((path, tree)),
                kind => {
                    files.insert(path, kind);
                }
            }
        }
    }
    Ok(files)
}

/// The files where a tree and a state differ, for failure messages.
fn describe_tree<O: Objects>(objects: &O, root: ObjectId, expected: &Files) -> String {
    const SHOWN: usize = 12;
    let actual = match tree_files(objects, root) {
        Ok(files) => files,
        Err(error) => return format!("  cannot read the tree {}: {error}", short(root)),
    };
    let paths: BTreeSet<&Vec<u8>> = actual.keys().chain(expected.keys()).collect();
    let mut lines = Vec::new();
    for path in paths {
        let want = expected.get(path);
        let got = actual.get(path);
        if want.map(Value::entry).as_ref() == got {
            continue;
        }
        if lines.len() == SHOWN {
            lines.push("  ...".to_owned());
            break;
        }
        let want = want.map_or_else(|| "nothing".to_owned(), |value| format!("{value:?}"));
        lines.push(format!(
            "  {}: the model has {want}, the tree has {}",
            show(path),
            show_entry(got)
        ));
    }
    if lines.is_empty() {
        return "  (the same files: the trees differ in structure only)".to_owned();
    }
    lines.join("\n")
}
