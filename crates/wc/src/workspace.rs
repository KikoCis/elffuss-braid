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

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use braid_engine::{Conflict, EMPTY_TREE, first_parent_chain_contains, read_changeset};
use braid_format::{Change, ChangeOp, Kind, ObjectId, Objects, Signature};
use braid_repo::{DIR, Repo, WORKSPACES_DIR, WorkspaceLink};
use braid_store::{RefUpdate, Store, TransactError};

use crate::cache::Cache;
use crate::checkout::{self, Mode, Plan};
use crate::error::Error;
use crate::fsprobe::FileSystem;
use crate::identity::Identity;
use crate::markers::has_markers;
use crate::paths::{is_within, os_name, split_last};
use crate::rebase::{First, Replayed, agent_extras, conflict_tree, rebased_extras, replay};
use crate::safe::{Found, Root, Stat, StatKind, open_file, read_link};
use crate::scan;
use crate::state::{Checkout, Lock, Rebase, RecordedConflict, State};
use crate::status::{self, Status};
use crate::tree::{flatten, root_of};

/// A workspace of a repository.
#[derive(Debug)]
pub struct Workspace<'r> {
    repo: &'r Repo,
    name: String,
    dir: PathBuf,
    state_dir: PathBuf,
}

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
    /// oldest first.
    pub unlanded: Vec<ObjectId>,
    /// For a rebase in progress: the conflicts, each with whether conflict
    /// markers remain in its file.
    pub conflicts: Vec<(RecordedConflict, bool)>,
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
    /// resolved.
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

/// What a landing did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandReport {
    /// The branch.
    pub branch: String,
    /// Its head before the landing.
    pub previous: Option<ObjectId>,
    /// How it ended.
    pub landed: Landed,
    /// The op log operation that moved the branch, if it moved.
    pub operation: Option<u64>,
}

impl LandReport {
    /// The branch's head after the landing.
    pub fn head(&self) -> Option<ObjectId> {
        match &self.landed {
            Landed::FastForward(id) | Landed::Rebased(id) => Some(*id),
            _ => self.previous,
        }
    }
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
    },
    Moved {
        tip: ObjectId,
        rebased: bool,
        tip_root: ObjectId,
        plan: Plan,
    },
    Conflicts {
        change: ObjectId,
        conflicts: Vec<Conflict>,
    },
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
        Ok((workspace, written))
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
    /// finished by the next one.
    fn write(
        &self,
        root: &Root,
        state: &mut State,
        plan: &Plan,
        from: ObjectId,
        to: ObjectId,
    ) -> Result<usize, Error> {
        if plan.len() == 0 {
            state.save(&self.state_dir)?;
            return Ok(0);
        }
        state.checkout = Some(Checkout { from, to });
        state.save(&self.state_dir)?;
        let written = checkout::apply(self.store(), root, plan)?;
        state.checkout = None;
        state.save(&self.state_dir)?;
        Ok(written)
    }

    /// Scans the working copy, saves the new stat cache, and compares it
    /// with the base.
    fn scan_status(&self, root: &Root, state: &State) -> Result<Status, Error> {
        let store = self.store();
        let base = flatten(store, root_of(store, state.base)?)?;
        let cache = Cache::load(&self.state_dir);
        let scan = scan::scan(root, &base, &cache)?;
        scan.cache.save(&self.state_dir)?;
        Ok(status::compare(&base, &scan, &state.tracked))
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
        let unlanded = find_stack(store, head, state.base, state.upstream)?;
        let mut conflicts = Vec::new();
        if let Some(rebase) = &state.rebase {
            for conflict in &rebase.conflicts {
                let remain = conflict.markers && markers_remain(&root, &conflict.path)?;
                conflicts.push((conflict.clone(), remain));
            }
        }
        Ok(StatusReport {
            state,
            status,
            head,
            unlanded,
            conflicts,
        })
    }

    /// Records the working copy as a changeset on the base, and makes it
    /// the new base. There is no staging area: every change counts, or
    /// every change at or below `paths`, except new files that the secrets
    /// guard holds back until `braid track` marks them.
    ///
    /// While a rebase is in progress the changeset is the resolution: it is
    /// refused while conflict markers remain in files with content
    /// conflicts or other conflicts are not marked resolved (unless
    /// `force`), it takes no paths, and once it is made, the changesets
    /// that were waiting are rebased onto it.
    pub fn commit(&self, request: &CommitRequest) -> Result<CommitOutcome, Error> {
        let (_lock, mut state, root) = self.begin()?;
        let store = self.store();
        let status = self.scan_status(&root, &state)?;
        let original = match &state.rebase {
            Some(rebase) => {
                if !request.paths.is_empty() {
                    return Err(Error::PathsDuringRebase);
                }
                if !request.force {
                    check_resolved(&root, rebase)?;
                }
                Some(read_changeset(store, rebase.change)?)
            }
            None => None,
        };
        let selected: Vec<&status::PathChange> = status
            .changes
            .iter()
            .filter(|change| {
                request.paths.is_empty() || request.paths.iter().any(|p| is_within(&change.path, p))
            })
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
                rebase: progress,
            });
        }

        let message = match (&request.message, &original) {
            (Some(message), _) => message.clone(),
            (None, Some(original)) => original.message.clone(),
            (None, None) => return Err(Error::NoMessage),
        };
        let mut changes = Vec::with_capacity(selected.len());
        for change in &selected {
            let op = match &change.op {
                ChangeOp::Delete | ChangeOp::Gitlink { .. } => change.op.clone(),
                _ => self.store_file(&root, &change.path)?,
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
            rebase: progress,
        })
    }

    /// Reads a changed file again and stores it as a blob: the content
    /// committed is the content read now.
    fn store_file(&self, root: &Root, path: &[u8]) -> Result<ChangeOp, Error> {
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
                let mut content = Vec::new();
                file.read_to_end(&mut content)
                    .map_err(|err| root.error("read", path, err))?;
                let blob = store.put(Kind::Blob, &content)?;
                let size = content.len() as u64;
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
    /// conflict markers, as the base `onto`.
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
    ) -> Result<(), Error> {
        let store = self.store();
        let subject = subject(&read_changeset(store, change)?.message);
        let ours = format!("{} ({})", state.branch, onto.short());
        let theirs = format!("{} ({subject})", change.short());
        let (tree, recorded) = conflict_tree(store, onto, change, conflicts, (&ours, &theirs))?;
        let plan = checkout::plan(store, root, state.fs, from, tree, Mode::Checked)?;
        state.base = Some(onto);
        state.rebase = Some(Rebase {
            change,
            onto,
            pending: pending.to_vec(),
            conflicts: recorded,
        });
        self.write(root, state, &plan, from, tree)?;
        Ok(())
    }

    /// Lands the workspace's own changesets on `onto` (by default the
    /// branch it tracks), in one op log transaction: the head is read, the
    /// changesets are landed on it with the engine (fast-forward or
    /// rebase), and the branch moves with a compare-and-swap. With
    /// conflicts the branch does not move and nothing in the workspace
    /// changes. Once the branch has moved, the working copy is brought from
    /// the base's tree to the landed tree, writing only the difference, and
    /// the landed changeset becomes the base. Refused with uncommitted
    /// changes or a rebase in progress.
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
        let mut previous = None;
        let mut decision = None;
        let description = format!("land workspace {} on {branch}", self.name);
        let result = store.transact(
            agent,
            &description,
            |refs| -> Result<Vec<RefUpdate>, Abort> {
                let head = refs.get(&branch);
                previous = head;
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
                    Replayed::Done { tip, .. } if Some(tip) == head => {
                        decision = Some(Decision::AlreadyApplied { head: tip });
                        Err(Abort::Stop)
                    }
                    Replayed::Done { tip, rebased } => {
                        // Check the working copy can follow before the branch
                        // moves.
                        let tip_root = root_of(store, Some(tip))?;
                        let plan = checkout::plan(
                            store,
                            &root,
                            state.fs,
                            base_root,
                            tip_root,
                            Mode::Checked,
                        )?;
                        decision = Some(Decision::Moved {
                            tip,
                            rebased,
                            tip_root,
                            plan,
                        });
                        Ok(vec![RefUpdate::new(branch.clone(), head, Some(tip))])
                    }
                    Replayed::Stopped {
                        index, conflicts, ..
                    } => {
                        decision = Some(Decision::Conflicts {
                            change: stack[index],
                            conflicts,
                        });
                        Err(Abort::Stop)
                    }
                }
            },
        );
        let operation = match result {
            Ok(op) => Some(op.seq),
            Err(TransactError::Aborted(Abort::Stop)) => None,
            Err(TransactError::Aborted(Abort::Failed(err))) => return Err(err),
            Err(TransactError::Store(err)) => return Err(err.into()),
        };
        let landed = match decision {
            Some(Decision::Moved {
                tip,
                rebased,
                tip_root,
                plan,
            }) => {
                state.base = Some(tip);
                state.upstream = Some(tip);
                self.write(&root, &mut state, &plan, base_root, tip_root)?;
                if rebased {
                    Landed::Rebased(tip)
                } else {
                    Landed::FastForward(tip)
                }
            }
            Some(Decision::AlreadyApplied { head }) => {
                // The branch has it all: the workspace moves to the head.
                self.move_to(&root, &mut state, base_root, head)?;
                Landed::AlreadyApplied
            }
            Some(Decision::Conflicts { change, conflicts }) => {
                Landed::Conflicts { change, conflicts }
            }
            Some(Decision::Nothing) | None => Landed::Nothing,
        };
        Ok(LandReport {
            branch,
            previous,
            landed,
            operation,
        })
    }

    /// Brings the working copy from tree `from` to `head`'s tree and makes
    /// `head` the base and the upstream.
    fn move_to(
        &self,
        root: &Root,
        state: &mut State,
        from: ObjectId,
        head: ObjectId,
    ) -> Result<(), Error> {
        let store = self.store();
        let to = root_of(store, Some(head))?;
        let plan = checkout::plan(store, root, state.fs, from, to, Mode::Checked)?;
        state.base = Some(head);
        state.upstream = Some(head);
        self.write(root, state, &plan, from, to)?;
        Ok(())
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
    pub fn update(&self, committer: &Identity) -> Result<Updated, Error> {
        let (_lock, mut state, root) = self.begin()?;
        if state.rebase.is_some() {
            return Err(Error::RebaseInProgress);
        }
        self.require_clean(&root, &state)?;
        let store = self.store();
        let Some(head) = store.refs()?.get(&state.branch) else {
            return Ok(Updated::UpToDate);
        };
        let base_root = root_of(store, state.base)?;
        let stack = find_stack(store, Some(head), state.base, state.upstream)?;
        if stack.is_empty() {
            if state.base == Some(head) {
                if state.upstream != Some(head) {
                    state.upstream = Some(head);
                    state.save(&self.state_dir)?;
                }
                return Ok(Updated::UpToDate);
            }
            self.move_to(&root, &mut state, base_root, head)?;
            return Ok(Updated::Moved { head });
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
            Replayed::Done { tip, .. } if tip == head => {
                self.move_to(&root, &mut state, base_root, head)?;
                Ok(Updated::Moved { head })
            }
            Replayed::Done { tip, .. } => {
                let to = root_of(store, Some(tip))?;
                let plan = checkout::plan(store, &root, state.fs, base_root, to, Mode::Checked)?;
                state.base = Some(tip);
                state.upstream = Some(head);
                self.write(&root, &mut state, &plan, base_root, to)?;
                Ok(Updated::Rebased { head, tip })
            }
            Replayed::Stopped {
                index,
                onto,
                conflicts,
            } => {
                let change = stack[index];
                state.upstream = Some(head);
                self.stop_on_conflicts(
                    &root,
                    &mut state,
                    base_root,
                    onto,
                    change,
                    &stack[index + 1..],
                    &conflicts,
                )?;
                Ok(Updated::Conflicts {
                    head,
                    change,
                    conflicts,
                })
            }
        }
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

/// Fails unless every conflict of the rebase is settled: no markers left in
/// the files that had them, and every other conflict marked resolved.
fn check_resolved(root: &Root, rebase: &Rebase) -> Result<(), Error> {
    let mut with_markers = Vec::new();
    let mut unresolved = Vec::new();
    for conflict in &rebase.conflicts {
        if conflict.markers {
            if markers_remain(root, &conflict.path)? {
                with_markers.push(conflict.path.clone());
            }
        } else if !conflict.resolved {
            unresolved.push(conflict.path.clone());
        }
    }
    if !with_markers.is_empty() {
        return Err(Error::MarkersRemain(with_markers));
    }
    if !unresolved.is_empty() {
        return Err(Error::Unresolved(unresolved));
    }
    Ok(())
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
