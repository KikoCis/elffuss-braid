// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! A workspace: a working copy on a branch, sharing the repository's store
//! with every other workspace.
//!
//! Every operation that changes a workspace holds its lock from start to
//! end, and reads the state only once it holds it, so two processes never
//! change one workspace at once. Workspaces never wait for each other,
//! except for the short op log transaction in which a landing moves a
//! branch.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use braid_engine::{
    Conflict, EMPTY_TREE, first_parent_chain_contains, read_changes, read_changeset,
};
use braid_format::{Change, ChangeOp, Kind, ObjectId, Objects, Signature};
use braid_repo::{DIR, Repo, WORKSPACES_DIR, WorkspaceLink};
use braid_store::{RefUpdate, Store, TransactError};

use crate::blocker::{Blocker, blockers};
use crate::cache::{self, Cache};
use crate::checkout::{self, Mode, Plan};
use crate::error::Error;
use crate::fsprobe::FileSystem;
use crate::identity::Identity;
use crate::markers::has_markers;
use crate::paths::{is_within, os_name, split_last};
use crate::rebase::{First, Replayed, agent_extras, conflict_tree, rebased_extras, replay};
use crate::safe::{Found, Root, Stat, StatKind, open_file, read_link};
use crate::scan::{self, Delta, Scan};
use crate::state::{Checkout, Lock, Rebase, RecordedConflict, State};
use crate::status::{self, Status};
use crate::tree::root_of;

/// A workspace of a repository.
#[derive(Debug)]
pub struct Workspace<'r> {
    repo: &'r Repo,
    name: String,
    dir: PathBuf,
    state_dir: PathBuf,
}

/// How far `status` walks the branch's history to count how far ahead of
/// the workspace it is.
pub const BEHIND_LIMIT: usize = 5000;

/// What `status` reports.
#[derive(Clone, Debug)]
pub struct StatusReport {
    /// The workspace's state.
    pub state: State,
    /// How the working copy differs from the base.
    pub status: Status,
    /// The head of the workspace's branch, if the branch exists.
    pub head: Option<ObjectId>,
    /// The workspace's own changesets that the branch does not have yet,
    /// oldest first. While a rebase is in progress they include the
    /// changeset being rebased and those waiting after it, by the ids they
    /// had before the rebase: none of them has landed.
    pub unlanded: Vec<ObjectId>,
    /// How many changesets the branch's head is ahead of the workspace:
    /// those from the head down to the changeset the workspace's own start
    /// from, along first parents. `None` when there are more than
    /// [`BEHIND_LIMIT`].
    pub behind_by: Option<usize>,
    /// For a rebase in progress: the conflicts, each with whether conflict
    /// markers remain in its file.
    pub conflicts: Vec<(RecordedConflict, bool)>,
    /// What stops [`Workspace::commit`] from recording the working copy as
    /// it is: the reasons it refuses with.
    pub blocking: Vec<Blocker>,
}

impl StatusReport {
    /// Whether the branch has changesets that the workspace does not.
    pub fn behind(&self) -> bool {
        self.behind_by != Some(0)
    }

    /// Whether [`Workspace::commit`] would record a changeset now: there is
    /// something to commit, or a rebase to finish, and nothing blocks it.
    pub fn ready_to_commit(&self) -> bool {
        self.blocking.is_empty() && (!self.status.is_clean() || self.state.rebase.is_some())
    }
}

/// What `commit` asks for.
#[derive(Clone, Debug)]
pub struct CommitRequest {
    /// The message; while a rebase is in progress, the rebased changeset's
    /// message is used if there is none.
    pub message: Option<Vec<u8>>,
    /// Commit only what lies at or below these paths; everything if empty.
    pub paths: Vec<Vec<u8>>,
    /// The author, if given; otherwise the rebased changeset's author while
    /// a rebase is in progress, and the committer otherwise.
    pub author: Option<Identity>,
    /// Who records the changeset.
    pub committer: Identity,
    /// The label of the agent making it, recorded as its
    /// [`AGENT`](braid_engine::AGENT) field. While a rebase is in progress,
    /// the rebased changeset's own agent stays, if it names one.
    pub agent: String,
    /// Commit even if conflict markers remain or conflicts are not marked
    /// resolved; new files waiting for `braid track` stay out.
    pub force: bool,
}

/// Where a rebase stands after a commit or an update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebaseProgress {
    /// No rebase was in progress.
    None,
    /// The rebase is finished: the workspace is at `tip`.
    Finished {
        /// The last rebased changeset.
        tip: ObjectId,
    },
    /// The next changeset conflicts too: resolve and commit again.
    Stopped {
        /// The changeset that conflicts.
        change: ObjectId,
        /// Its conflicts.
        conflicts: Vec<Conflict>,
    },
}

/// What `commit` did.
#[derive(Clone, Debug)]
pub struct CommitOutcome {
    /// The new changeset; `None` when a rebase finished with nothing left to
    /// record.
    pub changeset: Option<ObjectId>,
    /// How many paths it changed.
    pub changes: usize,
    /// Whether it finished a rebase with the message given instead of the
    /// rebased changeset's own.
    pub message_replaced: bool,
    /// Where the rebase in progress, if any, stands now.
    pub rebase: RebaseProgress,
}

/// How a landing ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Landed {
    /// Nothing to land: the branch has the workspace's base already.
    Nothing,
    /// The branch moved to the change itself.
    FastForward(ObjectId),
    /// The change was rebased onto the head; the branch moved to the result.
    Rebased(ObjectId),
    /// The branch already had everything the change does; it did not move.
    AlreadyApplied,
    /// The change conflicts with the head; the branch did not move.
    Conflicts {
        /// The changeset that conflicts.
        change: ObjectId,
        /// The conflicts.
        conflicts: Vec<Conflict>,
    },
}

/// One changeset of a workspace, and the changeset that holds it now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    /// The changeset as the workspace had it.
    pub from: ObjectId,
    /// What holds it now: the same changeset after a fast-forward, the
    /// rebased one after a rebase, or `None` when it did not go anywhere:
    /// the branch had its changes already, or a conflict stopped it or one
    /// before it.
    pub to: Option<ObjectId>,
}

/// What a landing did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandReport {
    /// The branch.
    pub branch: String,
    /// Its head before the landing.
    pub head_before: Option<ObjectId>,
    /// How it ended.
    pub landed: Landed,
    /// The op log operation that moved the branch, if it moved.
    pub operation: Option<u64>,
    /// Where each changeset of the stack went, oldest first; when the
    /// branch did not move, none went anywhere.
    pub placed: Vec<Placed>,
    /// The paths both sides changed, merged line by line or by kind, over
    /// the whole stack, sorted.
    pub merged_paths: Vec<Vec<u8>>,
    /// The paths of the working copy that the landing then wrote or
    /// removed, sorted; none for [`land_change`](crate::land_change).
    pub updated_paths: Vec<Vec<u8>>,
}

impl LandReport {
    /// The branch's head after the landing.
    pub fn head(&self) -> Option<ObjectId> {
        match &self.landed {
            Landed::FastForward(id) | Landed::Rebased(id) => Some(*id),
            _ => self.head_before,
        }
    }
}

/// What `update` did, with the outcome in [`UpdateReport::updated`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateReport {
    /// The branch the workspace updates from.
    pub branch: String,
    /// Its head, which an update reads and never moves; `None` when the
    /// branch does not exist.
    pub head: Option<ObjectId>,
    /// How the update ended.
    pub updated: Updated,
    /// The workspace's base before the update.
    pub base_before: Option<ObjectId>,
    /// Its base after it.
    pub base: Option<ObjectId>,
    /// Where each of the workspace's own changesets went, oldest first: the
    /// rebased changeset on top of the head, or `None` from the one that
    /// conflicts on.
    pub placed: Vec<Placed>,
    /// The paths both sides changed, merged line by line or by kind, over
    /// the changesets rebased, sorted.
    pub merged_paths: Vec<Vec<u8>>,
    /// The paths of the working copy that the update wrote or removed,
    /// sorted.
    pub updated_paths: Vec<Vec<u8>>,
}

/// What `update` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Updated {
    /// The workspace was on the head already, or the branch does not exist.
    UpToDate,
    /// The workspace moved to the head.
    Moved {
        /// The head.
        head: ObjectId,
    },
    /// The workspace's own changesets were rebased onto the head.
    Rebased {
        /// The head.
        head: ObjectId,
        /// The last rebased changeset, now the workspace's base.
        tip: ObjectId,
    },
    /// A changeset conflicts with the head: the working copy holds the
    /// head's tree with the change's clean paths and conflict markers, and
    /// a rebase is in progress.
    Conflicts {
        /// The head.
        head: ObjectId,
        /// The changeset that conflicts.
        change: ObjectId,
        /// Its conflicts.
        conflicts: Vec<Conflict>,
    },
}

/// Why a landing transaction stops without moving the branch.
enum Abort {
    Stop,
    Failed(Error),
}

impl From<Error> for Abort {
    fn from(error: Error) -> Self {
        Abort::Failed(error)
    }
}

/// What a landing decided inside its transaction.
enum Decision {
    Nothing,
    AlreadyApplied {
        head: ObjectId,
        placed: Vec<Placed>,
    },
    Moved {
        tip: ObjectId,
        rebased: bool,
        tip_root: ObjectId,
        plan: Plan,
        placed: Vec<Placed>,
        merged_paths: Vec<Vec<u8>>,
    },
    Conflicts {
        change: ObjectId,
        conflicts: Vec<Conflict>,
        stack: Vec<ObjectId>,
    },
}

/// How many changesets an op log description names, at most.
const DESCRIBED: usize = 8;

/// What the op log says a landing did, after `prefix`: how it went, each
/// changeset and the one it became (short ids), and how many paths were
/// merged, as in `rebased c57e1007e4f3 -> 3afaed346293 (1 path merged)`.
pub(crate) fn describe_landing(
    prefix: &str,
    outcome: &str,
    placed: &[Placed],
    merged_paths: usize,
) -> String {
    let mut moves: Vec<String> = placed
        .iter()
        .take(DESCRIBED)
        .map(|placed| match placed.to {
            Some(to) => format!("{} -> {}", placed.from.short(), to.short()),
            None => format!("{} already applied", placed.from.short()),
        })
        .collect();
    if placed.len() > DESCRIBED {
        moves.push(format!("and {} more", placed.len() - DESCRIBED));
    }
    let moves = match placed.len() {
        1 => moves.concat(),
        n => format!("{n} changesets: {}", moves.join(", ")),
    };
    let merged = match merged_paths {
        1 => "1 path".to_owned(),
        n => format!("{n} paths"),
    };
    format!("{prefix}: {outcome} {moves} ({merged} merged)")
}

impl<'r> Workspace<'r> {
    /// The directory that holds every workspace's state.
    pub fn states_dir(repo: &Repo) -> PathBuf {
        repo.root().join(DIR).join(WORKSPACES_DIR)
    }

    /// Creates a workspace named `name` in `dir`, which must not exist or be
    /// empty, at the changeset `at` (`None` for the empty tree), tracking
    /// `branch`. Writes the whole tree. Returns the workspace and how many
    /// files it wrote. On failure, nothing is left behind.
    pub fn create(
        repo: &'r Repo,
        name: &str,
        dir: &Path,
        at: Option<ObjectId>,
        branch: &str,
    ) -> Result<(Workspace<'r>, usize), Error> {
        if !WorkspaceLink::is_valid_name(name) {
            return Err(Error::InvalidName(name.to_owned()));
        }
        let states = Self::states_dir(repo);
        fs::create_dir_all(&states).map_err(Error::io("create", &states))?;
        let state_dir = states.join(name);
        match fs::create_dir(&state_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Error::WorkspaceExists(name.to_owned()));
            }
            Err(err) => return Err(Error::io("create", &state_dir)(err)),
        }
        let mut prepared = None;
        let made = Self::populate(repo, name, dir, &state_dir, at, branch, &mut prepared);
        if made.is_err() {
            match prepared {
                Some(Prepared::Created) => {
                    let _ = fs::remove_dir_all(dir);
                }
                // It was empty: everything in it now is this workspace's.
                Some(Prepared::WasEmpty) => clear(dir),
                None => {}
            }
            let _ = fs::remove_dir_all(&state_dir);
        }
        made
    }

    fn populate(
        repo: &'r Repo,
        name: &str,
        dir: &Path,
        state_dir: &Path,
        at: Option<ObjectId>,
        branch: &str,
        prepared: &mut Option<Prepared>,
    ) -> Result<(Workspace<'r>, usize), Error> {
        let _lock = Lock::acquire(state_dir)?;
        match fs::read_dir(dir) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    return Err(Error::DirectoryNotEmpty(dir.to_path_buf()));
                }
                *prepared = Some(Prepared::WasEmpty);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(dir).map_err(Error::io("create", dir))?;
                *prepared = Some(Prepared::Created);
            }
            Err(err) => return Err(Error::io("read", dir)(err)),
        }
        let dir = fs::canonicalize(dir).map_err(Error::io("open", dir))?;
        let repository = fs::canonicalize(repo.root()).map_err(Error::io("open", repo.root()))?;
        let link = WorkspaceLink {
            repository,
            name: name.to_owned(),
        };
        let link_path = dir.join(DIR);
        fs::File::create_new(&link_path)
            .and_then(|mut file| io::Write::write_all(&mut file, &link.encode()))
            .map_err(Error::io("write", &link_path))?;

        let workspace = Workspace {
            repo,
            name: name.to_owned(),
            dir: dir.clone(),
            state_dir: state_dir.to_path_buf(),
        };
        let root = Root::open(&dir)?;
        let fs = FileSystem::probe(root.dir());
        let store = repo.store();
        let to = root_of(store, at)?;
        let plan = checkout::plan(store, &root, fs, EMPTY_TREE, to, Mode::Checked)?;
        let mut state = State {
            path: dir,
            branch: branch.to_owned(),
            base: at,
            upstream: at,
            fs,
            checkout: None,
            tracked: Vec::new(),
            rebase: None,
        };
        let written = workspace.write(&root, &mut state, &plan, EMPTY_TREE, to)?;
        Ok((workspace, written.len()))
    }

    /// Opens the workspace named `name`, at the directory its state
    /// records.
    pub fn open(repo: &'r Repo, name: &str) -> Result<Workspace<'r>, Error> {
        if !WorkspaceLink::is_valid_name(name) {
            return Err(Error::InvalidName(name.to_owned()));
        }
        let state_dir = Self::states_dir(repo).join(name);
        if !state_dir.join(crate::state::FILE).is_file() {
            return Err(Error::NoSuchWorkspace(name.to_owned()));
        }
        let state = State::load(&state_dir)?;
        Ok(Workspace {
            repo,
            name: name.to_owned(),
            dir: state.path,
            state_dir,
        })
    }

    /// Opens the workspace named `name` at `dir`, where its `.braid` file
    /// was found. If the directory moved, the state follows it the next
    /// time the workspace changes.
    pub fn at(repo: &'r Repo, name: &str, dir: &Path) -> Result<Workspace<'r>, Error> {
        let mut workspace = Self::open(repo, name)?;
        workspace.dir = dir.to_path_buf();
        Ok(workspace)
    }

    /// Every workspace of `repo`, by name, with its state.
    pub fn list(repo: &Repo) -> Result<Vec<(String, State)>, Error> {
        let states = Self::states_dir(repo);
        let entries = match fs::read_dir(&states) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(Error::io("read", &states)(err)),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(Error::io("read", &states))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !WorkspaceLink::is_valid_name(&name)
                || !entry.path().join(crate::state::FILE).is_file()
            {
                continue;
            }
            found.push((name, State::load(&entry.path())?));
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(found)
    }

    /// Removes the workspace named `name`: its directory and its state.
    /// Refused while it has uncommitted changes, files waiting for `braid
    /// track`, changesets the branch does not have, or a rebase in progress,
    /// unless `force`. The directory is removed only if its `.braid` file
    /// still names this workspace of this repository.
    pub fn remove(repo: &Repo, name: &str, force: bool) -> Result<(), Error> {
        let workspace = Workspace::open(repo, name)?;
        let lock = Lock::acquire(&workspace.state_dir)?;
        let state = State::load(&workspace.state_dir)?;
        let owned = workspace.owns_directory(&state.path);
        if !force {
            if state.rebase.is_some() {
                return Err(Error::RebaseInProgress);
            }
            if owned {
                let root = Root::open(&state.path)?;
                let status = workspace.scan_status(&root, &state)?;
                let dirty: Vec<&Vec<u8>> = status
                    .changes
                    .iter()
                    .map(|change| &change.path)
                    .chain(&status.needs_track)
                    .collect();
                if let Some(first) = dirty.first() {
                    return Err(Error::Uncommitted {
                        count: dirty.len(),
                        first: first.to_vec(),
                    });
                }
            }
            let store = repo.store();
            let head = store.refs()?.get(&state.branch);
            let unlanded = find_stack(store, head, state.base, state.upstream)?;
            if !unlanded.is_empty() {
                return Err(Error::Unlanded {
                    count: unlanded.len(),
                    branch: state.branch.clone(),
                });
            }
        }
        if owned {
            fs::remove_dir_all(&state.path).map_err(Error::io("remove", &state.path))?;
        }
        drop(lock);
        fs::remove_dir_all(&workspace.state_dir).map_err(Error::io("remove", &workspace.state_dir))
    }

    /// Whether `dir` holds a `.braid` file naming this workspace.
    fn owns_directory(&self, dir: &Path) -> bool {
        let Ok(link) = WorkspaceLink::read(&dir.join(DIR)) else {
            return false;
        };
        let same_repo = match (
            fs::canonicalize(&link.repository),
            fs::canonicalize(self.repo.root()),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        link.name == self.name && same_repo
    }

    /// The workspace's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The workspace directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The repository.
    pub fn repo(&self) -> &'r Repo {
        self.repo
    }

    /// The workspace's state as last saved, read without the lock.
    pub fn state(&self) -> Result<State, Error> {
        State::load(&self.state_dir)
    }

    /// Takes the lock, reads the state, opens the working copy, and
    /// finishes a checkout that an earlier command left unfinished.
    fn begin(&self) -> Result<(Lock, State, Root), Error> {
        let lock = Lock::acquire(&self.state_dir)?;
        let mut state = State::load(&self.state_dir)?;
        let root = Root::open(&self.dir)?;
        if state.path != self.dir {
            state.path = self.dir.clone();
            state.save(&self.state_dir)?;
        }
        if let Some(Checkout { from, to }) = state.checkout {
            let plan = checkout::plan(self.store(), &root, state.fs, from, to, Mode::Forced)?;
            checkout::apply(self.store(), &root, &plan)?;
            state.checkout = None;
            state.save(&self.state_dir)?;
        }
        Ok((lock, state, root))
    }

    fn store(&self) -> &'r Store {
        self.repo.store()
    }

    /// Writes a planned checkout from tree `from` to tree `to` and saves
    /// `state`. The state is saved first with the checkout marked as
    /// unfinished, so that a command interrupted half way through is
    /// finished by the next one. Returns the paths written or removed.
    fn write(
        &self,
        root: &Root,
        state: &mut State,
        plan: &Plan,
        from: ObjectId,
        to: ObjectId,
    ) -> Result<Vec<Vec<u8>>, Error> {
        if plan.len() == 0 {
            state.save(&self.state_dir)?;
            return Ok(Vec::new());
        }
        state.checkout = Some(Checkout { from, to });
        state.save(&self.state_dir)?;
        checkout::apply(self.store(), root, plan)?;
        state.checkout = None;
        state.save(&self.state_dir)?;
        Ok(plan.paths())
    }

    /// Scans the working copy and returns where it differs from the base,
    /// saving the stat cache if it changed.
    fn scan(&self, root: &Root, state: &State) -> Result<Vec<Delta>, Error> {
        let store = self.store();
        let base = root_of(store, state.base)?;
        let cache = Cache::load(&self.state_dir);
        let Scan {
            deltas,
            cache: next,
            ..
        } = scan::scan(root, store, base, &cache, state.fs)?;
        if next.changed {
            cache::save(&self.state_dir, next.time, &next.records)?;
        }
        Ok(deltas)
    }

    /// Scans the working copy and compares it with the base.
    fn scan_status(&self, root: &Root, state: &State) -> Result<Status, Error> {
        let deltas = self.scan(root, state)?;
        Ok(status::compare(&deltas, &state.tracked))
    }

    /// Fails if the working copy has uncommitted changes.
    fn require_clean(&self, root: &Root, state: &State) -> Result<(), Error> {
        let status = self.scan_status(root, state)?;
        match status.changes.first() {
            Some(first) => Err(Error::Uncommitted {
                count: status.changes.len(),
                first: first.path.clone(),
            }),
            None => Ok(()),
        }
    }

    /// How the working copy differs from the base, and where the workspace
    /// stands against its branch.
    pub fn status(&self) -> Result<StatusReport, Error> {
        let (_lock, state, root) = self.begin()?;
        let status = self.scan_status(&root, &state)?;
        let store = self.store();
        let head = store.refs()?.get(&state.branch);
        let mut unlanded = find_stack(store, head, state.base, state.upstream)?;
        // The changeset the workspace's own start from.
        let fork = match unlanded.first() {
            Some(&oldest) => read_changeset(store, oldest)?.parents.first().copied(),
            None => state.base,
        };
        let behind_by = distance(store, head, fork, BEHIND_LIMIT)?;
        if let Some(rebase) = &state.rebase {
            unlanded.push(rebase.change);
            unlanded.extend(&rebase.pending);
        }
        let conflicts = conflict_states(&root, state.rebase.as_ref())?;
        let blocking = blockers(&conflicts, &status.needs_track, |_| true);
        Ok(StatusReport {
            state,
            status,
            head,
            unlanded,
            behind_by,
            conflicts,
            blocking,
        })
    }

    /// Records the working copy as a changeset on the base, and makes it
    /// the new base. There is no staging area: every change counts, or
    /// every change at or below `paths`.
    ///
    /// It is refused with [`Error::Blocked`], which lists every reason as
    /// [`StatusReport::blocking`] does, while new files at or below `paths`
    /// wait for `braid track`, and, during a rebase, while conflict markers
    /// remain in files with content conflicts or other conflicts are not
    /// marked resolved. With `force` it goes ahead, and the files waiting
    /// for `braid track` stay out.
    ///
    /// While a rebase is in progress the changeset is the resolution: it
    /// takes no paths, keeps the rebased changeset's author, extra fields
    /// and message unless one is given, and once it is made, the
    /// changesets that were waiting are rebased onto it.
    pub fn commit(&self, request: &CommitRequest) -> Result<CommitOutcome, Error> {
        let (_lock, mut state, root) = self.begin()?;
        let store = self.store();
        let deltas = self.scan(&root, &state)?;
        let status = status::compare(&deltas, &state.tracked);
        if state.rebase.is_some() && !request.paths.is_empty() {
            return Err(Error::PathsDuringRebase);
        }
        let in_scope = |path: &[u8]| {
            request.paths.is_empty() || request.paths.iter().any(|p| is_within(path, p))
        };
        if !request.force {
            let conflicts = conflict_states(&root, state.rebase.as_ref())?;
            let blocking = blockers(&conflicts, &status.needs_track, in_scope);
            if !blocking.is_empty() {
                return Err(Error::Blocked(blocking));
            }
        }
        let original = match &state.rebase {
            Some(rebase) => Some(read_changeset(store, rebase.change)?),
            None => None,
        };
        let selected: Vec<&status::PathChange> = status
            .changes
            .iter()
            .filter(|change| in_scope(&change.path))
            .collect();

        if selected.is_empty() {
            let Some(rebase) = state.rebase.take() else {
                return Err(Error::NothingToCommit);
            };
            // The resolution is the tree the rebase went onto: nothing of
            // this changeset is left to record.
            let progress = self.continue_rebase(
                &root,
                &mut state,
                rebase.pending,
                rebase.onto,
                &request.committer.now(),
            )?;
            state.save(&self.state_dir)?;
            return Ok(CommitOutcome {
                changeset: None,
                changes: 0,
                message_replaced: false,
                rebase: progress,
            });
        }

        let message = match (&request.message, &original) {
            (Some(message), _) => message.clone(),
            (None, Some(original)) => original.message.clone(),
            (None, None) => return Err(Error::NoMessage),
        };
        let message_replaced = request.message.is_some() && original.is_some();
        let olds: HashMap<&[u8], &ChangeOp> = deltas
            .iter()
            .filter_map(|delta| Some((delta.path.as_slice(), delta.old.as_ref()?)))
            .collect();
        let mut changes = Vec::with_capacity(selected.len());
        for change in &selected {
            let op = match &change.op {
                ChangeOp::Delete | ChangeOp::Gitlink { .. } => change.op.clone(),
                _ => {
                    // The base's file at the same path: what the store may
                    // keep the new content as a delta against.
                    let similar = match olds.get(change.path.as_slice()) {
                        Some(
                            ChangeOp::File { blob, .. }
                            | ChangeOp::Executable { blob, .. }
                            | ChangeOp::Symlink { blob, .. },
                        ) => Some(*blob),
                        _ => None,
                    };
                    self.store_file(&root, &change.path, similar)?
                }
            };
            changes.push(Change {
                path: change.path.clone(),
                op,
            });
        }
        let committer = request.committer.now();
        let author = match (&request.author, &original) {
            (Some(author), _) => author.now(),
            (None, Some(original)) => original.author.clone(),
            (None, None) => committer.clone(),
        };
        let extras = match (&state.rebase, &original) {
            (Some(rebase), Some(original)) => {
                rebased_extras(&original.extras, rebase.change, &request.agent)
            }
            _ => agent_extras(&request.agent),
        };
        let parents: Vec<ObjectId> = state.base.into_iter().collect();
        let count = changes.len();
        let id = braid_engine::commit(
            store,
            parents,
            changes,
            author,
            committer.clone(),
            message,
            extras,
        )?;
        store.sync()?;

        // Marks for files that are now in the base have done their job.
        let committed: HashSet<&[u8]> = selected
            .iter()
            .map(|change| change.path.as_slice())
            .collect();
        state
            .tracked
            .retain(|mark| !committed.contains(mark.as_slice()));
        state.base = Some(id);
        let progress = match state.rebase.take() {
            None => RebaseProgress::None,
            Some(rebase) => {
                self.continue_rebase(&root, &mut state, rebase.pending, id, &committer)?
            }
        };
        state.save(&self.state_dir)?;
        Ok(CommitOutcome {
            changeset: Some(id),
            changes: count,
            message_replaced,
            rebase: progress,
        })
    }

    /// Reads a changed file again and stores it as a blob: the content
    /// committed is the content read now. `similar` is the blob the base
    /// has at the same path, a hint for the store. A large file is streamed
    /// into the store in chunks, never held in memory whole.
    fn store_file(
        &self,
        root: &Root,
        path: &[u8],
        similar: Option<ObjectId>,
    ) -> Result<ChangeOp, Error> {
        let (parent, name) = split_last(path);
        let gone = || Error::Io {
            action: "read",
            path: root.path().join(crate::paths::show(path).as_ref()),
            source: io::Error::new(
                io::ErrorKind::NotFound,
                "the file changed while it was committed; try again",
            ),
        };
        let Found::Here(dir) = root.open_dir(parent)? else {
            return Err(gone());
        };
        let os = os_name(name).ok_or_else(gone)?;
        let meta = dir.symlink_metadata(os).map_err(|_| gone())?;
        let stat = Stat::of(&meta);
        let store = self.store();
        match stat.kind {
            StatKind::Symlink => {
                let target = read_link(&dir, os).map_err(|err| root.error("read", path, err))?;
                let blob = store.put(Kind::Blob, &target)?;
                Ok(ChangeOp::Symlink {
                    blob,
                    size: target.len() as u64,
                })
            }
            StatKind::File => {
                let mut file = open_file(&dir, os).map_err(|err| root.error("read", path, err))?;
                let executable = Stat::of(
                    &file
                        .metadata()
                        .map_err(|err| root.error("read", path, err))?,
                )
                .executable;
                let (blob, size) = if meta.len() >= braid_store::CHUNKED_MIN {
                    store
                        .put_blob_from(&mut file, Some(meta.len()))
                        .map_err(|err| match err {
                            braid_store::Error::Stream { source, .. } => {
                                root.error("read", path, source)
                            }
                            other => other.into(),
                        })?
                } else {
                    let mut content = Vec::new();
                    file.read_to_end(&mut content)
                        .map_err(|err| root.error("read", path, err))?;
                    let blob = match &similar {
                        Some(similar) => store.put_similar(Kind::Blob, &content, similar)?,
                        None => store.put(Kind::Blob, &content)?,
                    };
                    (blob, content.len() as u64)
                };
                Ok(if executable {
                    ChangeOp::Executable { blob, size }
                } else {
                    ChangeOp::File { blob, size }
                })
            }
            StatKind::Dir | StatKind::Other => Err(gone()),
        }
    }

    /// Rebases the changesets that waited behind a resolved conflict onto
    /// `tip`, the resolution, and brings the working copy along.
    fn continue_rebase(
        &self,
        root: &Root,
        state: &mut State,
        pending: Vec<ObjectId>,
        tip: ObjectId,
        committer: &Signature,
    ) -> Result<RebaseProgress, Error> {
        state.base = Some(tip);
        if pending.is_empty() {
            return Ok(RebaseProgress::Finished { tip });
        }
        let store = self.store();
        let from = root_of(store, Some(tip))?;
        match replay(
            store,
            &state.branch,
            Some(tip),
            &pending,
            committer,
            First::Pick,
        )? {
            Replayed::Done { tip: last, .. } => {
                let to = root_of(store, Some(last))?;
                let plan = checkout::plan(store, root, state.fs, from, to, Mode::Checked)?;
                state.base = Some(last);
                self.write(root, state, &plan, from, to)?;
                Ok(RebaseProgress::Finished { tip: last })
            }
            Replayed::Stopped {
                index,
                onto,
                conflicts,
                ..
            } => {
                let change = pending[index];
                self.stop_on_conflicts(
                    root,
                    state,
                    from,
                    onto,
                    change,
                    &pending[index + 1..],
                    &conflicts,
                )?;
                Ok(RebaseProgress::Stopped { change, conflicts })
            }
        }
    }

    /// Writes the working copy for a rebase stopped by conflicts and
    /// records the rebase: `onto`'s tree, with `change`'s clean paths and
    /// conflict markers, as the base `onto`. Returns the paths written or
    /// removed.
    #[allow(clippy::too_many_arguments)]
    fn stop_on_conflicts(
        &self,
        root: &Root,
        state: &mut State,
        from: ObjectId,
        onto: ObjectId,
        change: ObjectId,
        pending: &[ObjectId],
        conflicts: &[Conflict],
    ) -> Result<Vec<Vec<u8>>, Error> {
        let store = self.store();
        let changeset = read_changeset(store, change)?;
        let subject = subject(&changeset.message);
        let ours = format!("{} ({})", state.branch, onto.short());
        let theirs = format!("{} ({subject})", change.short());
        let (tree, recorded) = conflict_tree(store, onto, change, conflicts, (&ours, &theirs))?;
        let plan = checkout::plan(store, root, state.fs, from, tree, Mode::Checked)?;
        // The files the change sets were committed once already, those the
        // secrets guard holds back too, after `braid track`: they stay
        // tracked through the resolution, rather than waiting to be
        // tracked again or dropped from it.
        for item in read_changes(store, change, &changeset)?.iter() {
            let size = match &item.op {
                ChangeOp::File { size, .. }
                | ChangeOp::Executable { size, .. }
                | ChangeOp::Symlink { size, .. } => *size,
                ChangeOp::Gitlink { .. } | ChangeOp::Delete => continue,
            };
            if status::is_guarded(&item.path, size) && !state.tracked.contains(&item.path) {
                state.tracked.push(item.path.clone());
            }
        }
        state.tracked.sort();
        state.base = Some(onto);
        state.rebase = Some(Rebase {
            change,
            onto,
            pending: pending.to_vec(),
            conflicts: recorded,
        });
        self.write(root, state, &plan, from, tree)
    }

    /// Lands the workspace's own changesets on `onto` (by default the
    /// branch it tracks), in one op log transaction: the head is read, the
    /// changesets are landed on it with the engine (fast-forward or
    /// rebase), and the branch moves with a compare-and-swap. The operation
    /// says how it went, which changeset became which, and how many paths
    /// were merged (see [`LandReport`]). With conflicts the branch does not
    /// move and nothing in the workspace changes. Once the branch has
    /// moved, the working copy is brought from the base's tree to the
    /// landed tree, writing only the difference, and the landed changeset
    /// becomes the base. Refused with uncommitted changes or a rebase in
    /// progress.
    pub fn land(
        &self,
        onto: Option<&str>,
        agent: &str,
        committer: &Identity,
    ) -> Result<LandReport, Error> {
        let (_lock, mut state, root) = self.begin()?;
        if state.rebase.is_some() {
            return Err(Error::RebaseInProgress);
        }
        self.require_clean(&root, &state)?;
        let branch = onto.unwrap_or(&state.branch).to_owned();
        let store = self.store();
        let base_root = root_of(store, state.base)?;
        let signature = committer.now();
        let mut head_before = None;
        let mut decision = None;
        let prefix = format!("land workspace {} on {branch}", self.name);
        let result = store.transact_with(agent, |refs| -> Result<_, Abort> {
            let head = refs.get(&branch);
            head_before = head;
            let stack = find_stack(store, head, state.base, state.upstream)?;
            if stack.is_empty() {
                decision = Some(Decision::Nothing);
                return Err(Abort::Stop);
            }
            match replay(
                store,
                &branch,
                head,
                &stack,
                &signature,
                First::MustBeOnBranch,
            )? {
                Replayed::Done { tip, placed, .. } if Some(tip) == head => {
                    decision = Some(Decision::AlreadyApplied { head: tip, placed });
                    Err(Abort::Stop)
                }
                Replayed::Done {
                    tip,
                    rebased,
                    placed,
                    merged_paths,
                } => {
                    // Check the working copy can follow before the branch
                    // moves.
                    let tip_root = root_of(store, Some(tip))?;
                    let plan =
                        checkout::plan(store, &root, state.fs, base_root, tip_root, Mode::Checked)?;
                    let outcome = if rebased { "rebased" } else { "fast-forward" };
                    let description =
                        describe_landing(&prefix, outcome, &placed, merged_paths.len());
                    decision = Some(Decision::Moved {
                        tip,
                        rebased,
                        tip_root,
                        plan,
                        placed,
                        merged_paths,
                    });
                    let updates = vec![RefUpdate::new(branch.clone(), head, Some(tip))];
                    Ok((updates, description))
                }
                Replayed::Stopped {
                    index, conflicts, ..
                } => {
                    decision = Some(Decision::Conflicts {
                        change: stack[index],
                        conflicts,
                        stack,
                    });
                    Err(Abort::Stop)
                }
            }
        });
        let operation = match result {
            Ok(op) => Some(op.seq),
            Err(TransactError::Aborted(Abort::Stop)) => None,
            Err(TransactError::Aborted(Abort::Failed(err))) => return Err(err),
            Err(TransactError::Store(err)) => return Err(err.into()),
        };
        let mut report = LandReport {
            branch,
            head_before,
            landed: Landed::Nothing,
            operation,
            placed: Vec::new(),
            merged_paths: Vec::new(),
            updated_paths: Vec::new(),
        };
        match decision {
            Some(Decision::Moved {
                tip,
                rebased,
                tip_root,
                plan,
                placed,
                merged_paths,
            }) => {
                state.base = Some(tip);
                state.upstream = Some(tip);
                report.updated_paths = self.write(&root, &mut state, &plan, base_root, tip_root)?;
                report.landed = if rebased {
                    Landed::Rebased(tip)
                } else {
                    Landed::FastForward(tip)
                };
                report.placed = placed;
                report.merged_paths = merged_paths;
            }
            Some(Decision::AlreadyApplied { head, placed }) => {
                // The branch has it all: the workspace moves to the head.
                report.updated_paths = self.move_to(&root, &mut state, base_root, head)?;
                report.landed = Landed::AlreadyApplied;
                report.placed = placed;
            }
            Some(Decision::Conflicts {
                change,
                conflicts,
                stack,
            }) => {
                report.landed = Landed::Conflicts { change, conflicts };
                report.placed = unplaced(&stack);
            }
            Some(Decision::Nothing) | None => {}
        }
        Ok(report)
    }

    /// Brings the working copy from tree `from` to `head`'s tree and makes
    /// `head` the base and the upstream. Returns the paths written or
    /// removed.
    fn move_to(
        &self,
        root: &Root,
        state: &mut State,
        from: ObjectId,
        head: ObjectId,
    ) -> Result<Vec<Vec<u8>>, Error> {
        let store = self.store();
        let to = root_of(store, Some(head))?;
        let plan = checkout::plan(store, root, state.fs, from, to, Mode::Checked)?;
        state.base = Some(head);
        state.upstream = Some(head);
        self.write(root, state, &plan, from, to)
    }

    /// Brings the workspace onto the current head of its branch. Without
    /// changesets of its own, it moves to the head. With some, they are
    /// rebased onto the head with the engine, without moving the branch;
    /// cleanly, the workspace moves onto the rebased changesets. With
    /// conflicts, the working copy becomes the head's tree with the
    /// change's clean paths and, for content conflicts, files with conflict
    /// markers; other conflicts keep the head's version and are reported. A
    /// rebase is then in progress until `braid commit`. Refused with
    /// uncommitted changes or a rebase in progress.
    pub fn update(&self, committer: &Identity) -> Result<UpdateReport, Error> {
        let (_lock, mut state, root) = self.begin()?;
        if state.rebase.is_some() {
            return Err(Error::RebaseInProgress);
        }
        self.require_clean(&root, &state)?;
        let store = self.store();
        let mut report = UpdateReport {
            branch: state.branch.clone(),
            head: None,
            updated: Updated::UpToDate,
            base_before: state.base,
            base: state.base,
            placed: Vec::new(),
            merged_paths: Vec::new(),
            updated_paths: Vec::new(),
        };
        let Some(head) = store.refs()?.get(&state.branch) else {
            return Ok(report);
        };
        report.head = Some(head);
        let base_root = root_of(store, state.base)?;
        let stack = find_stack(store, Some(head), state.base, state.upstream)?;
        if stack.is_empty() {
            if state.base == Some(head) {
                if state.upstream != Some(head) {
                    state.upstream = Some(head);
                    state.save(&self.state_dir)?;
                }
                return Ok(report);
            }
            report.updated_paths = self.move_to(&root, &mut state, base_root, head)?;
            report.updated = Updated::Moved { head };
            report.base = state.base;
            return Ok(report);
        }
        let signature = committer.now();
        match replay(
            store,
            &state.branch,
            Some(head),
            &stack,
            &signature,
            First::Anywhere,
        )? {
            Replayed::Done { tip, placed, .. } if tip == head => {
                report.updated_paths = self.move_to(&root, &mut state, base_root, head)?;
                report.updated = Updated::Moved { head };
                report.placed = placed;
            }
            Replayed::Done {
                tip,
                placed,
                merged_paths,
                ..
            } => {
                let to = root_of(store, Some(tip))?;
                let plan = checkout::plan(store, &root, state.fs, base_root, to, Mode::Checked)?;
                state.base = Some(tip);
                state.upstream = Some(head);
                report.updated_paths = self.write(&root, &mut state, &plan, base_root, to)?;
                report.updated = Updated::Rebased { head, tip };
                report.placed = placed;
                report.merged_paths = merged_paths;
            }
            Replayed::Stopped {
                index,
                onto,
                conflicts,
                mut placed,
            } => {
                let change = stack[index];
                state.upstream = Some(head);
                report.updated_paths = self.stop_on_conflicts(
                    &root,
                    &mut state,
                    base_root,
                    onto,
                    change,
                    &stack[index + 1..],
                    &conflicts,
                )?;
                placed.extend(unplaced(&stack[index..]));
                report.updated = Updated::Conflicts {
                    head,
                    change,
                    conflicts,
                };
                report.placed = placed;
            }
        }
        report.base = state.base;
        Ok(report)
    }

    /// Marks conflicts of the rebase in progress resolved: those at each of
    /// `paths`, which must not be content conflicts with markers (those are
    /// resolved by editing the markers out). Returns the conflicts still
    /// unresolved.
    pub fn resolve(&self, paths: &[Vec<u8>]) -> Result<Vec<RecordedConflict>, Error> {
        let (_lock, mut state, _root) = self.begin()?;
        let rebase = state.rebase.as_mut().ok_or(Error::NoRebase)?;
        for path in paths {
            let mut found = false;
            for conflict in rebase.conflicts.iter_mut().filter(|c| &c.path == path) {
                if conflict.markers {
                    return Err(Error::NotResolvable {
                        path: path.clone(),
                        reason: "a content conflict: edit the conflict markers out instead",
                    });
                }
                conflict.resolved = true;
                found = true;
            }
            if !found {
                return Err(Error::NotResolvable {
                    path: path.clone(),
                    reason: "no conflict at this path",
                });
            }
        }
        let remaining = rebase
            .conflicts
            .iter()
            .filter(|c| !c.markers && !c.resolved)
            .cloned()
            .collect();
        state.save(&self.state_dir)?;
        Ok(remaining)
    }

    /// Marks paths so that the secrets guard lets the new files at or below
    /// them be committed. Returns every mark.
    pub fn track(&self, paths: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, Error> {
        let (_lock, mut state, _root) = self.begin()?;
        for path in paths {
            if !state.tracked.contains(path) {
                state.tracked.push(path.clone());
            }
        }
        state.tracked.sort();
        state.save(&self.state_dir)?;
        Ok(state.tracked)
    }
}

/// What `create` did to the workspace's directory before filling it.
#[derive(Clone, Copy, Debug)]
enum Prepared {
    Created,
    WasEmpty,
}

/// Removes everything inside `dir`, best effort, leaving it empty.
fn clear(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let _ = match entry.file_type() {
            Ok(kind) if kind.is_dir() => fs::remove_dir_all(&path),
            _ => fs::remove_file(&path),
        };
    }
}

/// The conflicts of the rebase in progress, if any, each with whether
/// conflict markers remain in its file.
fn conflict_states(
    root: &Root,
    rebase: Option<&Rebase>,
) -> Result<Vec<(RecordedConflict, bool)>, Error> {
    let mut states = Vec::new();
    for conflict in rebase.map_or(&[][..], |rebase| &rebase.conflicts) {
        let remain = conflict.markers && markers_remain(root, &conflict.path)?;
        states.push((conflict.clone(), remain));
    }
    Ok(states)
}

/// How many changesets lie on the first-parent chain of `head` above
/// `fork`: 0 when they are the same, the whole chain when `fork` is not on
/// it, and `None` past `limit`.
fn distance<O: Objects>(
    objects: &O,
    head: Option<ObjectId>,
    fork: Option<ObjectId>,
    limit: usize,
) -> Result<Option<usize>, Error> {
    let mut count = 0;
    let mut current = head;
    while current != fork {
        let Some(id) = current else {
            break;
        };
        if count == limit {
            return Ok(None);
        }
        count += 1;
        current = read_changeset(objects, id)?.parents.first().copied();
    }
    Ok(Some(count))
}

/// Whether the regular file at `path` still holds conflict markers. A file
/// that is gone, or is not a regular file any more, holds none.
fn markers_remain(root: &Root, path: &[u8]) -> Result<bool, Error> {
    let (parent, name) = split_last(path);
    let Found::Here(dir) = root.open_dir(parent)? else {
        return Ok(false);
    };
    let Some(os) = os_name(name) else {
        return Ok(false);
    };
    let mut file = match open_file(&dir, os) {
        Ok(file) => file,
        Err(_) => return Ok(false),
    };
    let mut content = Vec::new();
    file.read_to_end(&mut content)
        .map_err(|err| root.error("read", path, err))?;
    Ok(has_markers(&content))
}

/// Changesets that went nowhere.
fn unplaced(stack: &[ObjectId]) -> Vec<Placed> {
    stack
        .iter()
        .map(|&from| Placed { from, to: None })
        .collect()
}

/// The first non-empty line of a message.
fn subject(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// The workspace's own changesets that `head` does not have, oldest first:
/// `base` and its first parents, back to the first one on `head`'s
/// first-parent chain. `hint` is where the last walk ended; when it is on
/// the chain, the walk stops there without reading the head's history.
pub(crate) fn find_stack<O: Objects>(
    objects: &O,
    head: Option<ObjectId>,
    base: Option<ObjectId>,
    hint: Option<ObjectId>,
) -> Result<Vec<ObjectId>, Error> {
    let Some(base) = base else {
        return Ok(Vec::new());
    };
    let parent = |id: ObjectId| -> Result<Option<ObjectId>, Error> {
        Ok(read_changeset(objects, id)?.parents.first().copied())
    };
    let Some(head) = head else {
        // A branch that does not exist has none of them.
        let mut stack = Vec::new();
        let mut current = Some(base);
        while let Some(id) = current {
            stack.push(id);
            current = parent(id)?;
        }
        stack.reverse();
        return Ok(stack);
    };
    if base == head {
        return Ok(Vec::new());
    }
    if let Some(hint) = hint
        && first_parent_chain_contains(objects, head, hint, usize::MAX)?
    {
        let mut stack = Vec::new();
        let mut current = Some(base);
        while let Some(id) = current {
            if id == hint {
                stack.reverse();
                return Ok(stack);
            }
            stack.push(id);
            current = parent(id)?;
        }
    }
    let mut on_branch = HashSet::new();
    let mut current = Some(head);
    while let Some(id) = current {
        on_branch.insert(id);
        current = parent(id)?;
    }
    let mut stack = Vec::new();
    let mut current = Some(base);
    while let Some(id) = current {
        if on_branch.contains(&id) {
            break;
        }
        stack.push(id);
        current = parent(id)?;
    }
    stack.reverse();
    Ok(stack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use braid_format::{Kind, MemoryObjects};

    fn id(n: u8) -> ObjectId {
        ObjectId::hash(Kind::Changeset, &[n])
    }

    #[test]
    fn distances_along_first_parents() {
        let objects = MemoryObjects::new();
        let signature = Signature {
            name: b"Ada".to_vec(),
            email: b"ada@example.org".to_vec(),
            seconds: 1_700_000_000,
            offset_minutes: 0,
        };
        let mut chain = Vec::new();
        for n in 0..10u8 {
            let blob = objects.put(Kind::Blob, &[n]).unwrap();
            let change = Change {
                path: b"f".to_vec(),
                op: ChangeOp::File { blob, size: 1 },
            };
            let id = braid_engine::commit(
                &objects,
                chain.last().copied().into_iter().collect(),
                vec![change],
                signature.clone(),
                signature.clone(),
                vec![n],
                Vec::new(),
            )
            .unwrap();
            chain.push(id);
        }
        let head = Some(chain[9]);
        let walk = |fork, limit| distance(&objects, head, fork, limit).unwrap();
        assert_eq!(walk(Some(chain[9]), 100), Some(0));
        assert_eq!(walk(Some(chain[6]), 100), Some(3));
        // Without a fork, or with one off the chain, the whole chain counts.
        assert_eq!(walk(None, 100), Some(10));
        assert_eq!(walk(Some(id(0)), 100), Some(10));
        // The walk stops at the limit.
        assert_eq!(walk(Some(chain[6]), 3), Some(3));
        assert_eq!(walk(Some(chain[6]), 2), None);
        // A branch that does not exist is not ahead of anything.
        assert_eq!(
            distance(&objects, None, Some(chain[0]), 5).unwrap(),
            Some(0)
        );
    }

    #[test]
    fn landing_descriptions() {
        let one = [Placed {
            from: id(1),
            to: Some(id(2)),
        }];
        assert_eq!(
            describe_landing("land workspace b on main", "rebased", &one, 1),
            format!(
                "land workspace b on main: rebased {} -> {} (1 path merged)",
                id(1).short(),
                id(2).short()
            )
        );
        let two = [
            Placed {
                from: id(1),
                to: None,
            },
            Placed {
                from: id(3),
                to: Some(id(4)),
            },
        ];
        assert_eq!(
            describe_landing("land", "rebased", &two, 0),
            format!(
                "land: rebased 2 changesets: {} already applied, {} -> {} (0 paths merged)",
                id(1).short(),
                id(3).short(),
                id(4).short()
            )
        );
        let many: Vec<Placed> = (0..20)
            .map(|n| Placed {
                from: id(n),
                to: Some(id(n)),
            })
            .collect();
        let text = describe_landing("land", "fast-forward", &many, 2);
        assert!(
            text.starts_with("land: fast-forward 20 changesets: "),
            "{text}"
        );
        assert!(text.ends_with(", and 12 more (2 paths merged)"), "{text}");
    }
}
