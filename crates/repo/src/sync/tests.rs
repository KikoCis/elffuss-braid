// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Tests of `git sync`, against local bare repositories playing the remote.
//!
//! Every git command here, those of the tests and those sync runs, ignores
//! the user's and the system's configuration and has fixed identities and
//! dates. Remotes keep a reflog of their branch, and every test that pushes
//! checks that it only ever moved forward.

use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use braid_engine::{ConflictKind, commit, read_changeset};
use braid_format::{Change, ChangeOp, Kind, ObjectId, Objects, Signature};
use braid_git::{GitOid, export_commit};
use braid_store::RefUpdate;
use tempfile::TempDir;

use super::*;
use crate::Repo;

/// The environment of every git command: no configuration but the
/// repository's, fixed identities, never a prompt.
fn git_env() -> Vec<(OsString, OsString)> {
    [
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_AUTHOR_NAME", "Ada Lovelace"),
        ("GIT_AUTHOR_EMAIL", "ada@example.org"),
        ("GIT_COMMITTER_NAME", "Charles Babbage"),
        ("GIT_COMMITTER_EMAIL", "charles@example.org"),
        ("GIT_TERMINAL_PROMPT", "0"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect()
}

/// Runs git in `dir` and returns its output, trimmed. `date` is the author
/// and committer date of anything it commits.
fn git_at(dir: &Path, date: i64, args: &[&str]) -> String {
    let mut command = Command::new("git");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
    ] {
        command.env_remove(variable);
    }
    let output = command
        .current_dir(dir)
        .envs(git_env())
        .env("GIT_AUTHOR_DATE", format!("{date} +0100"))
        .env("GIT_COMMITTER_DATE", format!("{date} +0100"))
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .output()
        .expect("git must be installed to run these tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_at(dir, EPOCH, args)
}

/// The first commit time; each commit adds a minute.
const EPOCH: i64 = 1_700_000_000;

/// A file to write, or `None` to delete it.
type Edit<'a> = (&'a str, Option<&'a str>);

/// A remote, a clone of it where others work, and a braid repository.
struct World {
    _dir: TempDir,
    root: PathBuf,
    remote: PathBuf,
    work: PathBuf,
    braid: Repo,
    clock: Cell<i64>,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        git(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let remote = root.join("remote.git");
        // Bare repositories keep no reflog unless asked to.
        git(&remote, &["config", "core.logAllRefUpdates", "true"]);
        let work = root.join("work");
        git(&root, &["init", "--quiet", "work"]);
        git(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        let braid = Repo::init(&root.join("braid")).unwrap();
        World {
            _dir: dir,
            root,
            remote,
            work,
            braid,
            clock: Cell::new(EPOCH),
        }
    }

    fn remote_address(&self) -> &str {
        self.remote.to_str().unwrap()
    }

    fn options(&self) -> SyncOptions {
        SyncOptions {
            git_env: git_env(),
            ..SyncOptions::default()
        }
    }

    fn sync_with(&self, options: &SyncOptions) -> Result<SyncReport, Error> {
        self.braid
            .git_sync(self.remote_address(), "main", "tester", options)
    }

    fn sync(&self) -> SyncReport {
        self.sync_with(&self.options())
            .unwrap_or_else(|err| panic!("{err}"))
    }

    /// The internal steps of one sync.
    fn steps<'a>(&'a self, options: &'a SyncOptions) -> Sync<'a> {
        Sync::new(
            &self.braid,
            self.remote_address(),
            "main",
            "tester",
            options,
        )
        .unwrap()
    }

    /// Commits `edits` in the clone, on top of the remote's main; returns
    /// the commit.
    fn commit_elsewhere(&self, edits: &[Edit<'_>], message: &str) -> String {
        if self.remote_head().is_some() {
            git(&self.work, &["fetch", "--quiet", "origin", "main"]);
            git(
                &self.work,
                &["checkout", "--quiet", "-B", "main", "FETCH_HEAD"],
            );
        }
        for (path, content) in edits {
            let file = self.work.join(path);
            match content {
                Some(content) => {
                    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                    std::fs::write(&file, content).unwrap();
                    git(&self.work, &["add", path]);
                }
                None => {
                    git(&self.work, &["rm", "--quiet", path]);
                }
            }
        }
        self.clock.set(self.clock.get() + 60);
        git_at(
            &self.work,
            self.clock.get(),
            &["commit", "--quiet", "-m", message],
        );
        git(&self.work, &["rev-parse", "HEAD"])
    }

    /// Commits `edits` elsewhere and pushes them to the remote, as a
    /// fast-forward.
    fn push_elsewhere(&self, edits: &[Edit<'_>], message: &str) -> String {
        let commit = self.commit_elsewhere(edits, message);
        git(&self.work, &["push", "--quiet", "origin", "main"]);
        commit
    }

    /// The remote's main, if it has one.
    fn remote_head(&self) -> Option<String> {
        let found = git(
            &self.remote,
            &["for-each-ref", "--format=%(objectname)", "refs/heads/main"],
        );
        (!found.is_empty()).then_some(found)
    }

    fn head(&self) -> Option<ObjectId> {
        self.braid.store().refs().unwrap().get("main")
    }

    /// Makes a changeset in braid on main with `edits`, and lands it.
    fn commit_in_braid(&self, edits: &[Edit<'_>], message: &str) -> ObjectId {
        let store = self.braid.store();
        let changes: Vec<Change> = edits
            .iter()
            .map(|(path, content)| Change {
                path: path.as_bytes().to_vec(),
                op: match content {
                    Some(content) => ChangeOp::File {
                        blob: store.put(Kind::Blob, content.as_bytes()).unwrap(),
                        size: content.len() as u64,
                    },
                    None => ChangeOp::Delete,
                },
            })
            .collect();
        let head = self.head();
        let grace = Signature {
            name: b"Grace Hopper".to_vec(),
            email: b"grace@example.org".to_vec(),
            seconds: 1_700_000_000,
            offset_minutes: -300,
        };
        let id = commit(
            store,
            head.into_iter().collect(),
            changes,
            grace.clone(),
            grace,
            message.as_bytes().to_vec(),
            Vec::new(),
        )
        .unwrap();
        store.sync().unwrap();
        store
            .transact("agent", "land", |refs| {
                Ok::<_, std::convert::Infallible>(vec![RefUpdate::new(
                    "main",
                    refs.get("main"),
                    Some(id),
                )])
            })
            .unwrap();
        id
    }

    fn operations(&self) -> usize {
        self.braid.store().log(usize::MAX).unwrap().len()
    }

    fn objects(&self) -> u64 {
        self.braid.store().fsck().unwrap().objects
    }

    fn map(&self) -> braid_git::GitMap {
        self.braid.git_map().unwrap()
    }

    /// The file at `path` in the remote's main.
    fn remote_file(&self, path: &str) -> String {
        git(&self.remote, &["show", &format!("main:{path}")])
    }

    /// Every commit of the remote's main, and every changeset of braid's
    /// main, are the same history: one changeset per commit, through the
    /// map, and no other.
    fn assert_same_history(&self) {
        let commits: HashSet<String> = git(&self.remote, &["rev-list", "main"])
            .lines()
            .map(str::to_owned)
            .collect();
        let mut changesets = HashSet::new();
        let mut pending: Vec<ObjectId> = self.head().into_iter().collect();
        while let Some(id) = pending.pop() {
            if changesets.insert(id) {
                pending.extend(read_changeset(self.braid.store(), id).unwrap().parents);
            }
        }
        assert_eq!(commits.len(), changesets.len(), "one changeset per commit");
        let map = self.map();
        for commit in &commits {
            let changeset = map
                .changeset(&commit.parse().unwrap())
                .unwrap_or_else(|| panic!("commit {commit} is not in the map"));
            assert!(changesets.contains(&changeset), "commit {commit}");
        }
    }

    /// The remote's main only ever moved forward: each update in its
    /// reflog starts where the one before ended and is a fast-forward.
    fn assert_forward_only(&self) {
        let log = std::fs::read_to_string(self.remote.join("logs/refs/heads/main")).unwrap();
        let zero = "0".repeat(40);
        let mut last = zero.clone();
        for line in log.lines() {
            let mut words = line.split(' ');
            let (old, new) = (words.next().unwrap(), words.next().unwrap());
            assert_eq!(old, last, "the reflog is continuous");
            if old != zero {
                git(&self.remote, &["merge-base", "--is-ancestor", old, new]);
            }
            last = new.to_owned();
        }
        assert_eq!(Some(last), self.remote_head());
    }

    /// A second sync changes nothing: no operation, no object.
    fn assert_sync_is_a_no_op(&self) {
        let (operations, objects) = (self.operations(), self.objects());
        let head = self.remote_head();
        let report = self.sync();
        assert_eq!(report.outcome, SyncOutcome::UpToDate);
        assert!(report.operations.is_empty());
        assert_eq!(report.imported, 0);
        assert_eq!(self.operations(), operations);
        assert_eq!(self.objects(), objects);
        assert_eq!(self.remote_head(), head);
    }
}

fn oid(text: &str) -> GitOid {
    text.parse().unwrap()
}

#[test]
fn the_first_sync_creates_the_branch() {
    let world = World::new();
    world.push_elsewhere(&[("README", Some("hello\n"))], "first");
    let second = world.push_elsewhere(&[("src/lib.rs", Some("pub fn f() {}\n"))], "second");

    let report = world.sync();
    assert_eq!(report.outcome, SyncOutcome::Created);
    assert!(report.outcome.is_success());
    assert_eq!(report.imported, 2);
    assert_eq!(report.before, None);
    let changeset = world.map().changeset(&oid(&second)).unwrap();
    assert_eq!(report.after, Some(changeset));
    assert_eq!(report.fetched, Some((oid(&second), changeset)));
    assert_eq!(report.pushed, None);
    assert_eq!(report.operations.len(), 1);
    assert_eq!(report.operations[0].agent, "tester");
    assert_eq!(world.head(), Some(changeset));
    let refs = world.braid.store().refs().unwrap();
    assert_eq!(refs.get(&report.remote_ref), Some(changeset));
    assert!(report.remote_ref.starts_with("remote/remote-"));
    assert!(report.remote_ref.ends_with("/main"));

    // The mirror, its lock, and the branch as fetched.
    let mirror = world
        .root
        .join("braid/.braid/git")
        .join(format!("{}.git", report.label));
    assert!(mirror.join("HEAD").is_file());
    assert!(
        world
            .root
            .join("braid/.braid/git")
            .join(format!("{}.lock", report.label))
            .is_file()
    );
    assert_eq!(
        git(&mirror, &["rev-parse", "refs/remotes/origin/main"]),
        second
    );

    world.assert_same_history();
    world.assert_sync_is_a_no_op();
}

#[test]
fn braid_ahead_is_pushed() {
    let world = World::new();
    let first = world.push_elsewhere(&[("a", Some("1\n")), ("b", Some("1\n"))], "first");
    world.sync();
    let one = world.commit_in_braid(&[("a", Some("2\n"))], "Made in braid\n");
    let two = world.commit_in_braid(&[("c/d", Some("new\n")), ("b", None)], "Again\n");

    let report = world.sync();
    assert_eq!(report.outcome, SyncOutcome::Pushed);
    assert_eq!(report.before, Some(two));
    assert_eq!(report.after, Some(two));
    assert_eq!(report.imported, 0);
    assert_eq!(report.operations.len(), 1);
    let pushed = report.pushed.unwrap();
    assert_eq!(world.remote_head(), Some(pushed.to_hex()));

    // The remote got exactly braid's commits, as the export makes them.
    let map = world.map();
    assert_eq!(map.commit(&two), Some(pushed));
    let store = world.braid.store();
    for changeset in [one, two] {
        let (commit, bytes) = export_commit(store, &map, &changeset).unwrap();
        let shown = Command::new("git")
            .current_dir(&world.remote)
            .envs(git_env())
            .args(["cat-file", "commit", &commit.to_hex()])
            .output()
            .unwrap();
        assert_eq!(shown.stdout, bytes);
    }
    assert_eq!(
        git(&world.remote, &["rev-list", "main"]),
        [pushed, map.commit(&one).unwrap(), oid(&first)]
            .map(|commit| commit.to_hex())
            .join("\n")
    );
    assert_eq!(world.remote_file("a"), "2");
    assert_eq!(world.remote_file("c/d"), "new");
    let refs = world.braid.store().refs().unwrap();
    assert_eq!(refs.get(&report.remote_ref), Some(two));

    world.assert_same_history();
    world.assert_forward_only();
    world.assert_sync_is_a_no_op();
}

#[test]
fn remote_ahead_fast_forwards() {
    let world = World::new();
    world.push_elsewhere(&[("a", Some("1\n"))], "first");
    world.sync();
    let before = world.head();
    world.push_elsewhere(&[("a", Some("2\n"))], "second");
    let third = world.push_elsewhere(&[("b", Some("1\n"))], "third");

    let report = world.sync();
    assert_eq!(report.outcome, SyncOutcome::FastForwarded);
    assert_eq!(report.imported, 2);
    assert_eq!(report.before, before);
    let changeset = world.map().changeset(&oid(&third)).unwrap();
    assert_eq!(report.after, Some(changeset));
    assert_eq!(world.head(), Some(changeset));
    assert_eq!(report.pushed, None);
    assert_eq!(world.remote_head(), Some(third));

    world.assert_same_history();
    world.assert_forward_only();
    world.assert_sync_is_a_no_op();
}

#[test]
fn diverged_with_disjoint_edits_merges_and_pushes() {
    let world = World::new();
    world.push_elsewhere(&[("a", Some("1\n")), ("b", Some("1\n"))], "first");
    world.sync();
    let ours = world.commit_in_braid(&[("a", Some("braid\n"))], "In braid\n");
    let theirs = world.push_elsewhere(&[("b", Some("git\n"))], "In git");

    let report = world.sync();
    let SyncOutcome::Merged {
        merge,
        merged_paths,
    } = report.outcome.clone()
    else {
        panic!("expected a merge, got {:?}", report.outcome);
    };
    assert!(merged_paths.is_empty());
    assert_eq!(report.imported, 1);
    assert_eq!(report.before, Some(ours));
    assert_eq!(report.after, Some(merge));
    assert_eq!(world.head(), Some(merge));
    let theirs_changeset = world.map().changeset(&oid(&theirs)).unwrap();
    let merged = read_changeset(world.braid.store(), merge).unwrap();
    assert_eq!(merged.parents, [ours, theirs_changeset]);
    assert_eq!(report.operations.len(), 2, "the merge, then the push");

    // The remote has both lines and the merge commit, a fast-forward from
    // what it had.
    let pushed = report.pushed.unwrap();
    assert_eq!(world.remote_head(), Some(pushed.to_hex()));
    let parents = git(&world.remote, &["rev-list", "--parents", "-n", "1", "main"]);
    let ours_commit = world.map().commit(&ours).unwrap();
    assert_eq!(parents, format!("{pushed} {ours_commit} {theirs}"));
    assert_eq!(world.remote_file("a"), "braid");
    assert_eq!(world.remote_file("b"), "git");
    let text = git(&world.remote, &["cat-file", "commit", "main"]);
    assert!(
        text.contains("\ncommitter tester <tester@example.org> "),
        "{text}"
    );
    assert!(
        text.ends_with(&format!("Merge {} into main", report.remote_ref)),
        "{text}"
    );

    world.assert_same_history();
    world.assert_forward_only();
    world.assert_sync_is_a_no_op();
}

#[test]
fn diverged_with_conflicting_edits_changes_nothing() {
    let world = World::new();
    world.push_elsewhere(&[("f", Some("1\n2\n3\n"))], "first");
    world.sync();
    let tracked = world.braid.store().refs().unwrap();
    let ours = world.commit_in_braid(&[("f", Some("1\nbraid\n3\n"))], "In braid\n");
    let theirs = world.push_elsewhere(&[("f", Some("1\ngit\n3\n"))], "In git");
    let operations = world.operations();

    let report = world.sync();
    let SyncOutcome::Conflicts(conflicts) = &report.outcome else {
        panic!("expected conflicts, got {:?}", report.outcome);
    };
    assert!(!report.outcome.is_success());
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path, b"f");
    assert!(matches!(conflicts[0].kind, ConflictKind::Content { .. }));
    assert!(report.operations.is_empty());
    assert_eq!(report.after, Some(ours));
    assert_eq!(report.pushed, None);

    // Nothing moved in braid, nothing was pushed.
    assert_eq!(world.operations(), operations);
    assert_eq!(world.head(), Some(ours));
    let refs = world.braid.store().refs().unwrap();
    assert_eq!(
        refs.get(&report.remote_ref),
        tracked.get(&report.remote_ref)
    );
    assert_eq!(refs.get(MAP_REF), tracked.get(MAP_REF));
    assert_eq!(world.remote_head(), Some(theirs));
    world.assert_forward_only();

    // Again: the same conflicts, still nothing moved.
    let again = world.sync();
    assert_eq!(again.outcome, report.outcome);
    assert_eq!(world.operations(), operations);
}

#[test]
fn a_lease_that_fails_changes_nothing_and_the_next_sync_converges() {
    let world = World::new();
    world.push_elsewhere(&[("a", Some("1\n")), ("b", Some("1\n"))], "first");
    world.sync();
    let ours = world.commit_in_braid(&[("a", Some("braid\n"))], "In braid\n");
    let refs_before = world.braid.store().refs().unwrap();
    let operations = world.operations();

    // Sync fetches and decides to push; then someone else pushes first.
    let options = world.options();
    let sync = world.steps(&options);
    let (lock, mirror) = sync.open_mirror().unwrap();
    let store = world.braid.store();
    let prepared = sync.prepare(store, &mirror).unwrap();
    let decision = sync.decide(store, &prepared).unwrap();
    assert!(matches!(decision, Decision::Push(id) if id == ours));
    let theirs = world.push_elsewhere(&[("b", Some("git\n"))], "In git first");
    let report = sync.execute(&mirror, prepared, decision).unwrap();
    drop(lock);

    assert_eq!(
        report.outcome,
        SyncOutcome::RemoteMoved {
            found: Some(oid(&theirs))
        }
    );
    assert!(!report.outcome.is_success());
    assert_eq!(report.pushed, None);
    assert!(report.operations.is_empty());
    // Nothing moved in braid: no ref, no operation.
    assert_eq!(
        world.braid.store().refs().unwrap().as_map(),
        refs_before.as_map()
    );
    assert_eq!(world.operations(), operations);
    assert_eq!(world.remote_head(), Some(theirs.clone()));

    // The next sync takes the new commit in, merges, and pushes.
    let report = world.sync();
    assert!(
        matches!(report.outcome, SyncOutcome::Merged { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(world.remote_file("a"), "braid");
    assert_eq!(world.remote_file("b"), "git");
    world.assert_same_history();
    world.assert_forward_only();
    world.assert_sync_is_a_no_op();
}

#[test]
fn a_crash_after_the_push_loses_nothing() {
    let world = World::new();
    world.push_elsewhere(&[("a", Some("1\n"))], "first");
    world.sync();
    world.commit_in_braid(&[("a", Some("2\n"))], "One\n");
    let two = world.commit_in_braid(&[("b", Some("1\n"))], "Two\n");

    // The steps of a sync up to the push, with a map that is never saved:
    // the process dies right after the push.
    let pushed = {
        let options = world.options();
        let sync = world.steps(&options);
        let (_lock, mirror) = sync.open_mirror().unwrap();
        let store = world.braid.store();
        let mut prepared = sync.prepare(store, &mirror).unwrap();
        let Decision::Push(target) = sync.decide(store, &prepared).unwrap() else {
            panic!("expected a push");
        };
        let commit = sync.export(&mirror, &mut prepared.map, target).unwrap();
        let expected = prepared.fetched.map(|(commit, _)| commit);
        assert!(sync.push(&mirror, commit, expected).unwrap().is_ok());
        commit
    };
    assert_eq!(world.remote_head(), Some(pushed.to_hex()));
    assert_eq!(
        world.map().changeset(&pushed),
        None,
        "the map was not saved"
    );
    let (operations, objects) = (world.operations(), world.objects());

    // The next sync knows the remote's new commits as braid's own: nothing
    // is imported, and only the map is saved.
    let report = world.sync();
    assert_eq!(report.outcome, SyncOutcome::UpToDate);
    assert_eq!(report.imported, 0);
    assert_eq!(report.fetched.map(|(commit, _)| commit), Some(pushed));
    assert_eq!(report.after, Some(two));
    assert_eq!(report.operations.len(), 1);
    assert_eq!(world.operations(), operations + 1);
    assert_eq!(
        world.objects(),
        objects + 1,
        "the new map, and nothing else"
    );
    assert_eq!(world.map().changeset(&pushed), Some(two));
    let refs = world.braid.store().refs().unwrap();
    assert_eq!(refs.get(&report.remote_ref), Some(two));

    world.assert_same_history();
    world.assert_forward_only();
    world.assert_sync_is_a_no_op();
}

#[test]
fn a_remote_without_the_branch_gets_it() {
    let world = World::new();
    let first = world.commit_in_braid(&[("a", Some("1\n"))], "Root made in braid\n");
    let second = world.commit_in_braid(&[("b", Some("2\n"))], "Second\n");

    let report = world.sync();
    assert_eq!(report.outcome, SyncOutcome::Pushed);
    assert_eq!(report.fetched, None);
    let pushed = report.pushed.unwrap();
    assert_eq!(world.remote_head(), Some(pushed.to_hex()));
    assert_eq!(world.map().commit(&second), Some(pushed));
    assert!(world.map().commit(&first).is_some());
    assert_eq!(git(&world.remote, &["rev-list", "--count", "main"]), "2");

    world.assert_same_history();
    world.assert_forward_only();
    world.assert_sync_is_a_no_op();
}

#[test]
fn nothing_to_sync() {
    let world = World::new();
    let result = world.sync_with(&world.options());
    assert!(
        matches!(&result, Err(Error::NothingToSync { branch }) if branch == "main"),
        "{result:?}"
    );
}

#[test]
fn a_dry_run_writes_nothing() {
    let world = World::new();
    let dry = SyncOptions {
        dry_run: true,
        ..world.options()
    };
    world.push_elsewhere(&[("a", Some("1\n")), ("b", Some("1\n"))], "first");

    // Before any sync: not even a mirror.
    let report = world.sync_with(&dry).unwrap();
    assert!(report.dry_run);
    assert_eq!(report.outcome, SyncOutcome::Created);
    assert_eq!(world.head(), None);
    assert_eq!(world.operations(), 0);
    assert_eq!(world.objects(), 0);
    assert!(!world.root.join("braid/.braid/git").exists());

    world.sync();
    let ours = world.commit_in_braid(&[("a", Some("braid\n"))], "In braid\n");
    let theirs = world.push_elsewhere(&[("b", Some("git\n"))], "In git");
    let mirror_refs = |world: &World| {
        let mirror = world.root.join("braid/.braid/git").join(format!(
            "{}.git",
            Remote::new(world.remote_address()).unwrap().label()
        ));
        git(&mirror, &["for-each-ref"])
    };
    let refs = world.braid.store().refs().unwrap();
    let (operations, objects, mirrored) =
        (world.operations(), world.objects(), mirror_refs(&world));

    let report = world.sync_with(&dry).unwrap();
    let SyncOutcome::Merged { merge, .. } = report.outcome else {
        panic!("expected a merge, got {:?}", report.outcome);
    };
    assert_eq!(report.imported, 1);
    assert!(report.pushed.is_some());
    assert!(report.operations.is_empty());
    assert_eq!(world.braid.store().refs().unwrap().as_map(), refs.as_map());
    assert_eq!(world.operations(), operations);
    assert_eq!(world.objects(), objects);
    assert!(!world.braid.store().contains(&merge).unwrap());
    assert_eq!(mirror_refs(&world), mirrored);
    assert_eq!(world.remote_head(), Some(theirs));
    assert_eq!(world.head(), Some(ours));

    // The real sync does what the dry run said.
    let real = world.sync();
    assert!(matches!(real.outcome, SyncOutcome::Merged { .. }));
    world.assert_forward_only();
}

#[test]
fn a_remote_branch_with_another_name() {
    let world = World::new();
    world.push_elsewhere(&[("a", Some("1\n"))], "first");
    git(&world.remote, &["branch", "-m", "main", "trunk"]);
    let options = SyncOptions {
        remote_branch: Some("trunk".to_owned()),
        ..world.options()
    };
    let report = world.sync_with(&options).unwrap();
    assert_eq!(report.outcome, SyncOutcome::Created);
    assert_eq!(report.remote_branch, "trunk");
    assert!(report.remote_ref.ends_with("/trunk"));
    world.commit_in_braid(&[("a", Some("2\n"))], "In braid\n");
    let report = world.sync_with(&options).unwrap();
    assert_eq!(report.outcome, SyncOutcome::Pushed);
    assert_eq!(
        git(&world.remote, &["rev-parse", "trunk"]),
        report.pushed.unwrap().to_hex()
    );
}

#[test]
fn the_merge_committer_comes_from_the_configuration() {
    let world = World::new();
    world.push_elsewhere(&[("a", Some("1\n")), ("b", Some("1\n"))], "first");
    world.sync();
    std::fs::write(
        world.root.join("braid/.braid/config"),
        "name = Braid\nemail = braid-bot@example.org\n",
    )
    .unwrap();
    world.commit_in_braid(&[("a", Some("braid\n"))], "In braid\n");
    world.push_elsewhere(&[("b", Some("git\n"))], "In git");
    let report = world.sync();
    assert!(matches!(report.outcome, SyncOutcome::Merged { .. }));
    let text = git(&world.remote, &["cat-file", "commit", "main"]);
    assert!(
        text.contains("\ncommitter tester <braid-bot@example.org> "),
        "{text}"
    );
}

#[test]
fn names_sync_refuses() {
    let world = World::new();
    let options = world.options();
    let sync = |remote: &str, branch: &str, agent: &str| {
        world.braid.git_sync(remote, branch, agent, &options)
    };
    let remote = world.remote_address();
    for branch in [
        "remote/x/main",
        "braid/git-map",
        "tags/v1",
        "git/refs/heads/x",
        "",
    ] {
        assert!(
            matches!(
                sync(remote, branch, "tester"),
                Err(Error::InvalidBranch { .. })
            ),
            "{branch:?}"
        );
    }
    for branch in ["a..b", "a:b", "a b", "HEAD"] {
        assert!(
            matches!(
                sync(remote, branch, "tester"),
                Err(Error::InvalidBranch { .. })
            ),
            "{branch:?}"
        );
    }
    assert!(matches!(
        sync("--upload-pack=touch", "main", "tester"),
        Err(Error::InvalidRemote { .. })
    ));
    assert!(matches!(
        sync(remote, "main", "<bot>"),
        Err(Error::InvalidAgent { .. })
    ));
}

#[test]
fn git_export_records_the_map() {
    let world = World::new();
    let first = world.commit_in_braid(&[("a", Some("1\n"))], "One\n");
    let second = world.commit_in_braid(&[("b", Some("1\n"))], "Two\n");
    let operations = world.operations();
    world.braid.git_export(&world.remote, &[]).unwrap();
    let map = world.map();
    let commit = map.commit(&second).unwrap();
    assert!(map.commit(&first).is_some());
    assert_eq!(world.remote_head(), Some(commit.to_hex()));
    let log = world.braid.store().log(1).unwrap();
    assert_eq!(world.operations(), operations + 1);
    assert_eq!(log[0].agent, crate::GIT_EXPORT_AGENT);

    // Importing what was exported brings nothing new.
    let import = world.braid.git_import(&world.remote, "tester").unwrap();
    assert_eq!(import.report.commits, 0);
    assert_eq!(import.report.refs[0].braid, second);
    // Exporting again records nothing.
    world.braid.git_export(&world.remote, &[]).unwrap();
    assert_eq!(
        world.operations(),
        operations + 1 + usize::from(import.operation.is_some())
    );
}

#[test]
fn saving_the_map_merges_what_another_process_saved() {
    let world = World::new();
    world.push_elsewhere(&[("a", Some("1\n"))], "first");
    world.sync();
    let repo = &world.braid;
    let base = repo.store().refs().unwrap().get(MAP_REF);
    let mut ours = repo.working_map(base).unwrap();
    let mut theirs = repo.working_map(base).unwrap();
    let ours_commit = world.commit_in_braid(&[("x", Some("1\n"))], "Ours\n");
    let theirs_commit = world.commit_in_braid(&[("y", Some("1\n"))], "Theirs\n");
    for (map, changeset) in [(&mut ours, ours_commit), (&mut theirs, theirs_commit)] {
        let mut exporter = braid_git::Exporter::new(repo.store(), &map.map);
        exporter.commit_id(&changeset).unwrap();
        let pairings = exporter.pairings();
        map.map.record(&pairings);
    }
    repo.save_with_map("a", "theirs", &mut theirs, &[]).unwrap();
    repo.save_with_map("b", "ours", &mut ours, &[]).unwrap();
    let saved = world.map();
    assert!(saved.commit(&ours_commit).is_some());
    assert!(saved.commit(&theirs_commit).is_some());
    assert_eq!(saved, ours.map);
}

#[test]
#[should_panic(expected = "\"merge-base\", \"--is-ancestor\"")]
fn the_forward_check_catches_a_forced_update() {
    let world = World::new();
    let first = world.push_elsewhere(&[("a", Some("1\n"))], "first");
    world.push_elsewhere(&[("a", Some("2\n"))], "second");
    git(&world.remote, &["update-ref", "refs/heads/main", &first]);
    world.assert_forward_only();
}

#[test]
fn history_rewritten_on_the_remote_is_merged_not_overwritten() {
    let world = World::new();
    let first = world.push_elsewhere(&[("a", Some("1\n")), ("b", Some("1\n"))], "first");
    let second = world.push_elsewhere(&[("a", Some("2\n"))], "second");
    world.sync();
    let synced = world.head().unwrap();
    // Someone drops `second` from the remote and pushes something else.
    git(&world.work, &["checkout", "--quiet", "-B", "main", &first]);
    world.clock.set(world.clock.get() + 60);
    std::fs::write(world.work.join("b"), "rewritten\n").unwrap();
    git(&world.work, &["add", "b"]);
    git_at(
        &world.work,
        world.clock.get(),
        &["commit", "--quiet", "-m", "rewritten"],
    );
    git(
        &world.work,
        &["push", "--quiet", "--force", "origin", "main"],
    );
    let rewritten = world.remote_head().unwrap();

    // Braid keeps what it had and merges: the remote fast-forwards from the
    // rewritten history and gets `second` back.
    let report = world.sync();
    let SyncOutcome::Merged { merge, .. } = report.outcome else {
        panic!("expected a merge, got {:?}", report.outcome);
    };
    assert_eq!(
        read_changeset(world.braid.store(), merge).unwrap().parents[0],
        synced
    );
    let pushed = report.pushed.unwrap().to_hex();
    git(
        &world.remote,
        &["merge-base", "--is-ancestor", &rewritten, &pushed],
    );
    git(
        &world.remote,
        &["merge-base", "--is-ancestor", &second, &pushed],
    );
    assert_eq!(world.remote_file("a"), "2");
    assert_eq!(world.remote_file("b"), "rewritten");
    world.assert_same_history();
    world.assert_sync_is_a_no_op();
}

#[test]
fn an_unreachable_remote_is_an_error() {
    let world = World::new();
    world.commit_in_braid(&[("a", Some("1\n"))], "One\n");
    let operations = world.operations();
    let missing = world.root.join("nowhere.git");
    let result = world.braid.git_sync(
        missing.to_str().unwrap(),
        "main",
        "tester",
        &world.options(),
    );
    assert!(
        matches!(
            &result,
            Err(Error::GitCommand {
                command: "fetch",
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(world.operations(), operations);
}

#[test]
fn a_crash_between_the_merge_and_its_push_loses_nothing() {
    let world = World::new();
    world.push_elsewhere(&[("a", Some("1\n")), ("b", Some("1\n"))], "first");
    world.sync();
    let ours = world.commit_in_braid(&[("a", Some("braid\n"))], "In braid\n");
    let theirs = world.push_elsewhere(&[("b", Some("git\n"))], "In git");

    // The braid branch moves to the merge, and the process dies before the
    // push.
    let merge = {
        let options = world.options();
        let sync = world.steps(&options);
        let (_lock, mirror) = sync.open_mirror().unwrap();
        let store = world.braid.store();
        let mut prepared = sync.prepare(store, &mirror).unwrap();
        let Decision::Merge { changeset, .. } = sync.decide(store, &prepared).unwrap() else {
            panic!("expected a merge");
        };
        sync.export(&mirror, &mut prepared.map, changeset).unwrap();
        let remote = prepared.fetched.map(|(_, changeset)| changeset);
        sync.save(
            "merge",
            &mut prepared.map,
            Some((Some(ours), changeset)),
            remote,
        )
        .unwrap();
        changeset
    };
    assert_eq!(world.head(), Some(merge));
    assert_eq!(world.remote_head(), Some(theirs));

    // The next sync finds braid ahead, and pushes the merge.
    let report = world.sync();
    assert_eq!(report.outcome, SyncOutcome::Pushed);
    assert_eq!(report.imported, 0);
    assert_eq!(report.after, Some(merge));
    assert_eq!(world.map().commit(&merge), report.pushed);
    world.assert_same_history();
    world.assert_forward_only();
    world.assert_sync_is_a_no_op();
}

#[test]
fn syncs_with_one_remote_take_turns() {
    let world = World::new();
    world.push_elsewhere(&[("a", Some("1\n"))], "first");
    world.sync();
    world.commit_in_braid(&[("a", Some("2\n"))], "In braid\n");
    let (repo, remote, options) = (&world.braid, world.remote_address(), world.options());
    let outcomes: Vec<SyncOutcome> = std::thread::scope(|scope| {
        let runs: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| repo.git_sync(remote, "main", "tester", &options)))
            .collect();
        runs.into_iter()
            .map(|run| run.join().unwrap().unwrap().outcome)
            .collect()
    });
    let pushed = outcomes
        .iter()
        .filter(|outcome| **outcome == SyncOutcome::Pushed)
        .count();
    assert_eq!(pushed, 1, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, SyncOutcome::Pushed | SyncOutcome::UpToDate)),
        "{outcomes:?}"
    );
    world.assert_same_history();
    world.assert_forward_only();
    world.assert_sync_is_a_no_op();
}
