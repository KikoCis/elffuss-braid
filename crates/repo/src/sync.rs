// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `git sync`: keeping a braid branch and a branch of a git remote in step,
//! so that a git host stays the public face of a repository while agents
//! land their changes in braid. See [`Repo::git_sync`].

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use braid_engine::{Conflict, MergeOutcome, is_ancestor, merge};
use braid_format::{Kind, MemoryObjects, ObjectId, Objects, Signature};
use braid_git::{Exporter, GitKind, GitOid, ImportOptions, RefTarget};
use braid_store::{Operation, Store};

use crate::git::{MAP_REF, Move, RESERVED, WorkingMap, remote_ref};
use crate::remote::{Fetched, Git, Pushed, Remote, Scratch};
use crate::{DIR, Error, Repo};

/// The directory inside [`DIR`] that holds the mirrors of git remotes.
pub const MIRRORS_DIR: &str = "git";

/// What [`Repo::git_sync`] does besides its defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncOptions {
    /// The branch of the remote, when its name differs from the braid
    /// branch's.
    pub remote_branch: Option<String>,
    /// Find out what the sync would do, and write nothing: no braid object,
    /// ref or operation, no mirror, no push. The remote branch is fetched
    /// into a temporary repository, removed afterwards.
    pub dry_run: bool,
    /// Environment variables to set for the git commands, on top of the
    /// process's own.
    pub git_env: Vec<(OsString, OsString)>,
}

/// What a sync did, or would do.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyncOutcome {
    /// The braid branch and the remote branch were at the same changeset.
    UpToDate,
    /// The braid branch did not exist; it was created at the remote's head.
    Created,
    /// The remote was ahead: the braid branch fast-forwarded to it.
    FastForwarded,
    /// Braid was ahead, or the remote had no such branch: the remote
    /// fast-forwarded to the braid branch, or the branch was created there.
    Pushed,
    /// Both had moved: braid merged them, moved its branch to the merge, and
    /// the remote fast-forwarded to it.
    Merged {
        /// The merge changeset.
        merge: ObjectId,
        /// The paths both sides changed, merged line by line or by kind.
        merged_paths: Vec<Vec<u8>>,
    },
    /// Both had moved, and the changes conflict. Nothing moved, and nothing
    /// was pushed.
    Conflicts(Vec<Conflict>),
    /// The push was refused because the remote branch moved after the fetch.
    /// Braid changed nothing, except that its branch is at the merge when
    /// one was made; the next sync takes the new commits in.
    RemoteMoved {
        /// Where the remote branch was found after the push failed.
        found: Option<GitOid>,
    },
    /// The remote refused the push for another reason, such as a protected
    /// branch. Braid changed nothing, except that its branch is at the merge
    /// when one was made.
    Rejected {
        /// Git's reason.
        reason: String,
    },
}

impl SyncOutcome {
    /// A short, stable name for the outcome, for reports and JSON:
    /// `up_to_date`, `created`, `fast_forwarded`, `pushed`, `merged`,
    /// `conflicts`, `remote_moved` or `rejected`.
    pub fn code(&self) -> &'static str {
        match self {
            SyncOutcome::UpToDate => "up_to_date",
            SyncOutcome::Created => "created",
            SyncOutcome::FastForwarded => "fast_forwarded",
            SyncOutcome::Pushed => "pushed",
            SyncOutcome::Merged { .. } => "merged",
            SyncOutcome::Conflicts(_) => "conflicts",
            SyncOutcome::RemoteMoved { .. } => "remote_moved",
            SyncOutcome::Rejected { .. } => "rejected",
        }
    }

    /// Whether the two branches are in step at the end: every outcome but
    /// conflicts and refused pushes.
    pub fn is_success(&self) -> bool {
        !matches!(
            self,
            SyncOutcome::Conflicts(_)
                | SyncOutcome::RemoteMoved { .. }
                | SyncOutcome::Rejected { .. }
        )
    }
}

/// What [`Repo::git_sync`] did, or with a dry run, would do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncReport {
    /// The remote's label, which names its mirror and its braid ref.
    pub label: String,
    /// The braid branch.
    pub branch: String,
    /// The branch of the remote.
    pub remote_branch: String,
    /// The braid ref that records where the remote branch was last seen.
    pub remote_ref: String,
    /// What happened.
    pub outcome: SyncOutcome,
    /// The braid branch before the sync.
    pub before: Option<ObjectId>,
    /// The braid branch after it.
    pub after: Option<ObjectId>,
    /// The remote branch as fetched, its commit and its changeset, or `None`
    /// when the remote has no such branch.
    pub fetched: Option<(GitOid, ObjectId)>,
    /// The commit pushed to the remote branch, or with a dry run, the commit
    /// that would be.
    pub pushed: Option<GitOid>,
    /// How many commits were imported from the remote.
    pub imported: usize,
    /// Whether this was a dry run, which writes nothing.
    pub dry_run: bool,
    /// The operations recorded in the op log, oldest first.
    pub operations: Vec<Operation>,
}

/// What the branches call for, once the remote branch is imported.
enum Decision {
    UpToDate,
    Create(ObjectId),
    FastForward(ObjectId),
    Push(ObjectId),
    Merge {
        changeset: ObjectId,
        merged_paths: Vec<Vec<u8>>,
    },
    Conflicts(Vec<Conflict>),
}

/// The state after fetching and importing.
struct Prepared {
    map: WorkingMap,
    head: Option<ObjectId>,
    fetched: Option<(GitOid, ObjectId)>,
    imported: usize,
}

/// A push that did not go through.
enum Refused {
    RemoteMoved(Option<GitOid>),
    Rejected(String),
}

impl Refused {
    fn outcome(self) -> SyncOutcome {
        match self {
            Refused::RemoteMoved(found) => SyncOutcome::RemoteMoved { found },
            Refused::Rejected(reason) => SyncOutcome::Rejected { reason },
        }
    }
}

impl Repo {
    /// Keeps the braid branch `branch` and a branch of the git remote
    /// `remote` in step: fetches and imports the remote branch, then
    /// fast-forwards the braid branch, or pushes it (merging first when both
    /// moved) as a fast-forward with a lease. Braid never force-pushes.
    /// Operations are recorded by `agent`, who is also the committer of a
    /// merge, with the `email` of the repository's [`Config`](crate::Config)
    /// or `<agent>@example.org`.
    ///
    /// # The mirror
    ///
    /// Braid keeps a bare git repository per remote, its mirror, at
    /// `.braid/git/<label>.git`, where `<label>` is the remote's label (see
    /// [`Remote::new`](crate::Remote::new)). In it,
    /// `refs/remotes/origin/<branch>` is the remote branch as last fetched,
    /// and `refs/heads/<branch>` the braid branch as last exported.
    /// `.braid/git/<label>.lock` is held while a sync with that remote runs,
    /// so syncs with one remote take turns.
    ///
    /// # Steps
    ///
    /// 1. The commits of the changesets on the braid branch are computed
    ///    (exports are deterministic) and recorded in the git map, so that
    ///    their copies on the remote import as the same changesets.
    /// 2. The remote branch is fetched into the mirror, with an explicit
    ///    refspec and no tags, and imported. Its changeset G is recorded in
    ///    the braid ref `remote/<label>/<branch>` (see [`remote_ref`]).
    /// 3. With B the head of the braid branch:
    ///    - B is G: nothing to do;
    ///    - there is no B: the braid branch is created at G;
    ///    - B is an ancestor of G: the braid branch fast-forwards to G;
    ///    - G is an ancestor of B, or the remote has no such branch: B is
    ///      exported into the mirror and pushed;
    ///    - otherwise the two are merged (see [`braid_engine::merge`]). A
    ///      clean merge M moves the braid branch from B to M, and M is
    ///      exported and pushed. Conflicts change nothing and are reported.
    ///
    ///    The braid branch only ever moves with a compare and swap, in one
    ///    operation with the git map and `remote/<label>/<branch>`.
    /// 4. A push is atomic and leased: `git push --atomic --porcelain
    ///    --force-with-lease=refs/heads/<branch>:<fetched> <remote>
    ///    <commit>:refs/heads/<branch>`, where `<fetched>` is G's commit as
    ///    this same sync fetched it, or nothing when the remote had no such
    ///    branch. What is pushed descends from G, so the remote only ever
    ///    fast-forwards. If the remote moved meanwhile, the lease fails and
    ///    nothing changes in braid (beyond the merge already made, if any);
    ///    the next sync takes the new commits in.
    ///
    /// # Crashes
    ///
    /// A crash after a push and before braid records it loses nothing: the
    /// next sync computes the same commits again in step 1, so the commits it
    /// fetches are known as the changesets they came from, and nothing is
    /// imported twice. A crash between moving the braid branch to a merge and
    /// pushing it leaves the braid branch ahead, and the next sync pushes it.
    ///
    /// Conflicts and refused pushes are outcomes, not errors: see
    /// [`SyncOutcome`]. With [`SyncOptions::dry_run`], nothing is written.
    pub fn git_sync(
        &self,
        remote: &str,
        branch: &str,
        agent: &str,
        options: &SyncOptions,
    ) -> Result<SyncReport, Error> {
        let sync = Sync::new(self, remote, branch, agent, options)?;
        if options.dry_run {
            sync.dry_run()
        } else {
            sync.run()
        }
    }

    /// The committer of the changesets sync makes: the agent label, and the
    /// email of the configuration or `<agent>@example.org`.
    fn sync_committer(&self, agent: &str) -> Result<Signature, Error> {
        if let Some(reason) = unfit_for_git(agent) {
            return Err(Error::InvalidAgent {
                agent: agent.to_owned(),
                reason,
            });
        }
        let email = match self.config()?.email {
            Some(email) => {
                if let Some(reason) = unfit_for_git(&email) {
                    return Err(Error::InvalidConfig {
                        key: "email",
                        reason,
                    });
                }
                email
            }
            None => format!("{}@example.org", email_name(agent)),
        };
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0));
        Ok(Signature {
            name: agent.as_bytes().to_vec(),
            email: email.into_bytes(),
            seconds,
            offset_minutes: 0,
        })
    }
}

/// Why a name or an address cannot be written in a git signature, if it
/// cannot.
fn unfit_for_git(text: &str) -> Option<&'static str> {
    if text.trim().is_empty() {
        Some("it is empty")
    } else if text.contains(['<', '>', '\n', '\0']) {
        Some("it holds '<', '>', a newline or NUL")
    } else {
        None
    }
}

/// The agent label as the local part of an email address.
fn email_name(agent: &str) -> String {
    let mut name = String::new();
    for c in agent.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            c
        } else {
            '-'
        };
        if !(c == '-' && name.ends_with('-')) {
            name.push(c);
        }
    }
    let name = name.trim_matches(['-', '.']);
    if name.is_empty() {
        "braid".to_owned()
    } else {
        name.to_owned()
    }
}

/// Checks the names: a braid branch outside braid's own namespaces, and a
/// remote branch git accepts.
fn check_branches(branch: &str, remote_branch: &str) -> Result<(), Error> {
    let invalid = |name: &str, reason| Error::InvalidBranch {
        name: name.to_owned(),
        reason,
    };
    if branch.is_empty() || branch.contains('\0') {
        return Err(invalid(branch, "it is empty or holds NUL"));
    }
    if RESERVED.iter().any(|prefix| branch.starts_with(prefix)) {
        return Err(invalid(branch, "it is in one of braid's own namespaces"));
    }
    if !braid_git::is_branch_name(remote_branch) {
        return Err(invalid(remote_branch, "git does not accept it as a branch"));
    }
    Ok(())
}

/// One sync with one remote.
struct Sync<'r> {
    repo: &'r Repo,
    git: Git<'r>,
    remote: Remote,
    agent: &'r str,
    committer: Signature,
    /// The braid branch.
    branch: String,
    /// The branch of the remote.
    remote_branch: String,
    /// Its full name, `refs/heads/<remote branch>`: on the remote, and in
    /// the mirror for the braid branch as last exported.
    branch_ref: String,
    /// The braid ref recording the remote branch.
    remote_ref: String,
    /// The mirror's ref for the remote branch as fetched.
    tracking_ref: String,
    /// The directory of the mirrors, absolute.
    mirrors: PathBuf,
}

impl<'r> Sync<'r> {
    fn new(
        repo: &'r Repo,
        remote: &str,
        branch: &str,
        agent: &'r str,
        options: &'r SyncOptions,
    ) -> Result<Self, Error> {
        let remote = Remote::new(remote)?;
        let remote_branch = options
            .remote_branch
            .clone()
            .unwrap_or_else(|| branch.to_owned());
        check_branches(branch, &remote_branch)?;
        Ok(Sync {
            repo,
            git: Git::new(&options.git_env),
            remote_ref: remote_ref(remote.label(), &remote_branch),
            branch_ref: format!("refs/heads/{remote_branch}"),
            tracking_ref: format!("refs/remotes/origin/{remote_branch}"),
            committer: repo.sync_committer(agent)?,
            mirrors: std::path::absolute(repo.root.join(DIR).join(MIRRORS_DIR))?,
            branch: branch.to_owned(),
            remote_branch,
            remote,
            agent,
        })
    }

    /// The remote's mirror.
    fn mirror(&self) -> PathBuf {
        self.mirrors.join(format!("{}.git", self.remote.label()))
    }

    fn run(&self) -> Result<SyncReport, Error> {
        let (_lock, mirror) = self.open_mirror()?;
        let store = self.repo.store();
        let prepared = self.prepare(store, &mirror)?;
        let decision = self.decide(store, &prepared)?;
        self.execute(&mirror, prepared, decision)
    }

    /// Takes the remote's lock, held until the returned file is dropped, and
    /// creates its mirror if there is none: an empty bare repository, made
    /// under a temporary name and renamed into place.
    fn open_mirror(&self) -> Result<(File, PathBuf), Error> {
        fs::create_dir_all(&self.mirrors)?;
        let label = self.remote.label();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.mirrors.join(format!("{label}.lock")))?;
        lock.lock()?;
        let mirror = self.mirror();
        if !mirror.join("HEAD").is_file() {
            let partial = self.mirrors.join(format!("{label}.git.tmp"));
            if partial.exists() {
                fs::remove_dir_all(&partial)?;
            }
            self.git.init_bare(&partial)?;
            fs::rename(&partial, &mirror)?;
        }
        Ok((lock, mirror))
    }

    /// Records the commits of the braid branch, fetches the remote branch
    /// into `fetch_repo` and imports it.
    fn prepare<O: Objects<Error = braid_store::Error>>(
        &self,
        objects: &O,
        fetch_repo: &Path,
    ) -> Result<Prepared, Error> {
        let refs = self.repo.store().refs()?;
        let head = refs.get(&self.branch);
        let mut map = self.repo.working_map(refs.get(MAP_REF))?;
        if let Some(head) = head {
            if !matches!(objects.stat(&head)?, Some((Kind::Changeset, _))) {
                return Err(Error::NotAChangeset {
                    what: format!("branch {:?}", self.branch),
                });
            }
            // Exports are deterministic: the commits of braid's changesets
            // are what a remote holds of them, whether or not an earlier sync
            // got to record them. A head that cannot be exported fails later,
            // where it would be pushed.
            let pairings = {
                let mut exporter = Exporter::new(objects, &map.map);
                exporter.commit_id(&head).map(|_| exporter.pairings())
            };
            if let Ok(pairings) = pairings {
                map.map.record(&pairings);
            }
        }

        let mut imported = 0;
        let fetched = match self.git.fetch(
            fetch_repo,
            self.remote.address(),
            &self.branch_ref,
            &self.tracking_ref,
        )? {
            Fetched::Absent => None,
            Fetched::Branch => {
                let options = ImportOptions {
                    refs: vec![self.tracking_ref.clone().into_bytes()],
                };
                let report = braid_git::import(fetch_repo, objects, &mut map.map, &options)?;
                imported = report.commits;
                let fetched = report.refs.into_iter().next().ok_or(Error::NotAChangeset {
                    what: "the remote branch".to_owned(),
                })?;
                if fetched.kind != GitKind::Commit {
                    return Err(Error::NotAChangeset {
                        what: "the remote branch".to_owned(),
                    });
                }
                Some((fetched.oid, fetched.braid))
            }
        };
        Ok(Prepared {
            map,
            head,
            fetched,
            imported,
        })
    }

    /// What the two heads call for. A merge is made, and stored in
    /// `objects`, when both moved.
    fn decide<O: Objects<Error = braid_store::Error>>(
        &self,
        objects: &O,
        prepared: &Prepared,
    ) -> Result<Decision, Error> {
        let remote = prepared.fetched.map(|(_, changeset)| changeset);
        Ok(match (prepared.head, remote) {
            (None, None) => {
                return Err(Error::NothingToSync {
                    branch: self.branch.clone(),
                });
            }
            (None, Some(remote)) => Decision::Create(remote),
            (Some(head), None) => Decision::Push(head),
            (Some(head), Some(remote)) if head == remote => Decision::UpToDate,
            (Some(head), Some(remote)) if is_ancestor(objects, remote, head)? => {
                Decision::Push(head)
            }
            (Some(head), Some(remote)) => {
                let message = format!("Merge {} into {}\n", self.remote_ref, self.branch);
                match merge(
                    objects,
                    head,
                    remote,
                    self.committer.clone(),
                    message.into_bytes(),
                )? {
                    MergeOutcome::UpToDate => Decision::Push(head),
                    MergeOutcome::FastForward(to) => Decision::FastForward(to),
                    MergeOutcome::Merged {
                        changeset,
                        merged_paths,
                    } => Decision::Merge {
                        changeset,
                        merged_paths,
                    },
                    MergeOutcome::Conflicts(conflicts) => Decision::Conflicts(conflicts),
                }
            }
        })
    }

    fn report(&self, prepared: &Prepared, dry_run: bool) -> SyncReport {
        SyncReport {
            label: self.remote.label().to_owned(),
            branch: self.branch.clone(),
            remote_branch: self.remote_branch.clone(),
            remote_ref: self.remote_ref.clone(),
            outcome: SyncOutcome::UpToDate,
            before: prepared.head,
            after: prepared.head,
            fetched: prepared.fetched,
            pushed: None,
            imported: prepared.imported,
            dry_run,
            operations: Vec::new(),
        }
    }

    /// Carries a decision out.
    fn execute(
        &self,
        mirror: &Path,
        mut prepared: Prepared,
        decision: Decision,
    ) -> Result<SyncReport, Error> {
        let mut report = self.report(&prepared, false);
        let head = prepared.head;
        let fetched = prepared.fetched.map(|(commit, _)| commit);
        let remote = prepared.fetched.map(|(_, changeset)| changeset);
        let map = &mut prepared.map;
        report.outcome = match decision {
            Decision::UpToDate => {
                // What step 1 and the import added to the map, and where the
                // remote branch is.
                report
                    .operations
                    .extend(self.save("up to date", map, None, remote)?);
                SyncOutcome::UpToDate
            }
            Decision::Create(to) => {
                report.operations.extend(self.save(
                    &format!("created {}", self.branch),
                    map,
                    Some((None, to)),
                    Some(to),
                )?);
                report.after = Some(to);
                SyncOutcome::Created
            }
            Decision::FastForward(to) => {
                report.operations.extend(self.save(
                    "fast-forward",
                    map,
                    Some((head, to)),
                    Some(to),
                )?);
                report.after = Some(to);
                SyncOutcome::FastForwarded
            }
            Decision::Conflicts(conflicts) => SyncOutcome::Conflicts(conflicts),
            Decision::Push(target) => {
                let commit = self.export(mirror, map, target)?;
                match self.push(mirror, commit, fetched)? {
                    Ok(()) => {
                        report.operations.extend(self.save(
                            &format!("pushed {commit}"),
                            map,
                            None,
                            Some(target),
                        )?);
                        report.pushed = Some(commit);
                        SyncOutcome::Pushed
                    }
                    Err(refused) => refused.outcome(),
                }
            }
            Decision::Merge {
                changeset,
                merged_paths,
            } => {
                let commit = self.export(mirror, map, changeset)?;
                report.operations.extend(self.save(
                    "merge",
                    map,
                    Some((head, changeset)),
                    remote,
                )?);
                report.after = Some(changeset);
                match self.push(mirror, commit, fetched)? {
                    Ok(()) => {
                        report.operations.extend(self.save(
                            &format!("pushed {commit}"),
                            map,
                            None,
                            Some(changeset),
                        )?);
                        report.pushed = Some(commit);
                        SyncOutcome::Merged {
                            merge: changeset,
                            merged_paths,
                        }
                    }
                    Err(refused) => refused.outcome(),
                }
            }
        };
        Ok(report)
    }

    /// Records an operation: the braid branch moved from one value to
    /// another, the remote ref to its value, and the map when it grew.
    fn save(
        &self,
        what: &str,
        map: &mut WorkingMap,
        branch: Option<(Option<ObjectId>, ObjectId)>,
        remote: Option<ObjectId>,
    ) -> Result<Option<Operation>, Error> {
        let mut moves = Vec::new();
        if let Some((from, to)) = branch {
            moves.push(Move {
                name: self.branch.clone(),
                expect: Some(from),
                new: to,
            });
        }
        if let Some(remote) = remote {
            moves.push(Move {
                name: self.remote_ref.clone(),
                expect: None,
                new: remote,
            });
        }
        let description = format!("git sync {} with {}: {what}", self.branch, self.remote_ref);
        self.repo
            .save_with_map(self.agent, &description, map, &moves)
    }

    /// Exports a changeset into the mirror, as the mirror's branch, and
    /// records in the map what the export generated. Returns its commit.
    fn export(
        &self,
        mirror: &Path,
        map: &mut WorkingMap,
        changeset: ObjectId,
    ) -> Result<GitOid, Error> {
        let report = braid_git::write_to_repo_recording(
            mirror,
            self.repo.store(),
            &mut map.map,
            &[braid_git::RefUpdate {
                name: self.branch_ref.clone().into_bytes(),
                target: RefTarget::Changeset(changeset),
            }],
        )?;
        Ok(report.refs[0].1)
    }

    /// Pushes a commit with a lease on what was fetched.
    fn push(
        &self,
        mirror: &Path,
        commit: GitOid,
        expected: Option<GitOid>,
    ) -> Result<Result<(), Refused>, Error> {
        let address = self.remote.address();
        let reason = match self
            .git
            .push(mirror, address, &self.branch_ref, commit, expected)?
        {
            Pushed::Done => return Ok(Ok(())),
            Pushed::Refused(reason) => reason,
        };
        // Whether the remote moved, or refused for another reason.
        Ok(
            match self.git.ls_remote(mirror, address, &self.branch_ref) {
                Ok(found) if found == Some(commit) => Ok(()),
                Ok(found) if found != expected => Err(Refused::RemoteMoved(found)),
                _ => Err(Refused::Rejected(reason)),
            },
        )
    }

    /// Everything [`run`](Self::run) would do up to the push, on a copy:
    /// the remote branch is fetched into a temporary repository that borrows
    /// the mirror's objects, and braid objects go to memory.
    fn dry_run(&self) -> Result<SyncReport, Error> {
        let mirror = self.mirror();
        let borrow = mirror.join("HEAD").is_file().then_some(mirror.as_path());
        let scratch = Scratch::create(&self.git, borrow)?;
        let overlay = Overlay {
            store: self.repo.store(),
            added: MemoryObjects::new(),
        };
        let prepared = self.prepare(&overlay, scratch.path())?;
        let decision = self.decide(&overlay, &prepared)?;
        let mut report = self.report(&prepared, true);
        let commit_of = |changeset: ObjectId| -> Result<GitOid, Error> {
            Ok(Exporter::new(&overlay, &prepared.map.map).commit_id(&changeset)?)
        };
        report.outcome = match decision {
            Decision::UpToDate => SyncOutcome::UpToDate,
            Decision::Create(to) => {
                report.after = Some(to);
                SyncOutcome::Created
            }
            Decision::FastForward(to) => {
                report.after = Some(to);
                SyncOutcome::FastForwarded
            }
            Decision::Push(target) => {
                report.pushed = Some(commit_of(target)?);
                SyncOutcome::Pushed
            }
            Decision::Merge {
                changeset,
                merged_paths,
            } => {
                report.pushed = Some(commit_of(changeset)?);
                report.after = Some(changeset);
                SyncOutcome::Merged {
                    merge: changeset,
                    merged_paths,
                }
            }
            Decision::Conflicts(conflicts) => SyncOutcome::Conflicts(conflicts),
        };
        Ok(report)
    }
}

/// The store, with what is written kept in memory: what a dry run works on.
struct Overlay<'s> {
    store: &'s Store,
    added: MemoryObjects,
}

impl Objects for Overlay<'_> {
    type Error = braid_store::Error;

    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, Self::Error> {
        let Ok(found) = self.added.get(id);
        match found {
            Some(found) => Ok(Some(found)),
            None => self.store.get(id),
        }
    }

    fn put(&self, kind: Kind, payload: &[u8]) -> Result<ObjectId, Self::Error> {
        let Ok(id) = self.added.put(kind, payload);
        Ok(id)
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, Self::Error> {
        let Ok(found) = self.added.contains(id);
        if found {
            return Ok(true);
        }
        self.store.contains(id)
    }

    fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, Self::Error> {
        let Ok(found) = self.added.stat(id);
        match found {
            Some(found) => Ok(Some(found)),
            None => self.store.stat(id),
        }
    }
}

#[cfg(test)]
mod tests;
