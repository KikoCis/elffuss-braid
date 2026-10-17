// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! End-to-end tests of `braid git sync`, with local bare repositories playing
//! the remote. Git runs without the user's or the system's configuration,
//! with fixed identities and dates, in the tests and in braid alike.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const EPOCH: i64 = 1_700_000_000;

/// Environment variables for every command: git ignores the user's and
/// the system's configuration and never prompts.
const GIT_ENV: [(&str, &str); 7] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Ada Lovelace"),
    ("GIT_AUTHOR_EMAIL", "ada@example.org"),
    ("GIT_COMMITTER_NAME", "Charles Babbage"),
    ("GIT_COMMITTER_EMAIL", "charles@example.org"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

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
        .envs(GIT_ENV)
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_at(dir, EPOCH, args)
}

/// A bare remote with a reflog, clones of it where people work, and a braid
/// repository.
struct World {
    _dir: TempDir,
    root: PathBuf,
    remote: PathBuf,
    braid: PathBuf,
    clock: Cell<i64>,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        git(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let remote = root.join("remote.git");
        git(&remote, &["config", "core.logAllRefUpdates", "true"]);
        let braid = root.join("braid");
        let world = World {
            _dir: dir,
            root,
            remote,
            braid,
            clock: Cell::new(EPOCH),
        };
        let init = world
            .braid_command(&["init"])
            .arg(&world.braid)
            .output()
            .unwrap();
        assert!(init.status.success());
        world
    }

    /// A clone of the remote named `name`, made once, on the remote's main.
    fn clone(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        if !path.exists() {
            git(&self.root, &["init", "--quiet", name]);
            git(
                &path,
                &["remote", "add", "origin", self.remote.to_str().unwrap()],
            );
        }
        if !git(&self.remote, &["for-each-ref", "refs/heads/main"]).is_empty() {
            git(&path, &["fetch", "--quiet", "origin", "main"]);
            git(&path, &["checkout", "--quiet", "-B", "main", "FETCH_HEAD"]);
        }
        path
    }

    /// Commits `files` in the clone `name`, on top of the remote's main.
    fn commit(&self, name: &str, files: &[(&str, &str)], message: &str) -> (PathBuf, String) {
        let path = self.clone(name);
        for (file, content) in files {
            std::fs::write(path.join(file), content).unwrap();
            git(&path, &["add", file]);
        }
        self.clock.set(self.clock.get() + 60);
        git_at(
            &path,
            self.clock.get(),
            &["commit", "--quiet", "-m", message],
        );
        let commit = git(&path, &["rev-parse", "HEAD"]);
        (path, commit)
    }

    /// Commits `files` in a clone and pushes them to the remote's main.
    fn push(&self, files: &[(&str, &str)], message: &str) -> String {
        let (path, commit) = self.commit("work", files, message);
        git(&path, &["push", "--quiet", "origin", "main"]);
        commit
    }

    fn braid_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_braid"));
        command
            .envs(GIT_ENV)
            .env("BRAID_AGENT", "tester")
            .args(args);
        command
    }

    /// Runs braid in the repository.
    fn braid(&self, args: &[&str]) -> Output {
        self.braid_command(args)
            .arg("--repo")
            .arg(&self.braid)
            .output()
            .unwrap()
    }

    /// Runs braid with `--json` and returns its exit status and output.
    fn braid_json(&self, args: &[&str]) -> (Option<i32>, Value) {
        let output = self
            .braid_command(args)
            .arg("--repo")
            .arg(&self.braid)
            .arg("--json")
            .output()
            .unwrap();
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
            panic!(
                "{err}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code(), value)
    }

    fn sync(&self) -> (Option<i32>, Value) {
        self.braid_json(&["git", "sync", self.remote.to_str().unwrap()])
    }

    fn refs(&self) -> Value {
        self.braid_json(&["refs"]).1["refs"].clone()
    }

    fn operations(&self) -> usize {
        self.braid_json(&["op", "log", "-n", "1000"])
            .1
            .as_array()
            .unwrap()
            .len()
    }

    fn remote_head(&self) -> String {
        git(&self.remote, &["rev-parse", "refs/heads/main"])
    }

    /// The remote's main only ever moved forward.
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
        assert_eq!(last, self.remote_head());
    }
}

#[test]
fn the_first_sync_creates_the_branch_and_the_next_does_nothing() {
    let world = World::new();
    world.push(&[("README", "hello\n")], "First");
    let second = world.push(&[("lib.rs", "pub fn f() {}\n")], "Second");

    let (code, report) = world.sync();
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["outcome"], "created");
    assert_eq!(report["imported"], 2);
    assert_eq!(report["dry_run"], false);
    assert_eq!(report["fetched"]["commit"], second.as_str());
    assert_eq!(report["after"], report["fetched"]["changeset"]);
    assert_eq!(report["operations"].as_array().unwrap().len(), 1);
    let refs = world.refs();
    assert_eq!(refs["main"], report["after"]);
    let remote_ref = report["remote_ref"].as_str().unwrap();
    assert!(remote_ref.starts_with("remote/remote-"), "{remote_ref}");
    assert_eq!(refs[remote_ref], report["after"]);

    let (code, again) = world.sync();
    assert_eq!(code, Some(0));
    assert_eq!(again["outcome"], "up_to_date");
    assert_eq!(again["operations"], Value::Array(Vec::new()));
    assert_eq!(world.refs(), refs);

    let log = world.braid_json(&["log", "main"]).1;
    assert_eq!(log.as_array().unwrap().len(), 2);
}

#[test]
fn the_remote_ahead_fast_forwards() {
    let world = World::new();
    world.push(&[("a", "1\n")], "First");
    world.sync();
    let next = world.push(&[("a", "2\n")], "Next");
    let (code, report) = world.sync();
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["outcome"], "fast_forwarded");
    assert_eq!(report["fetched"]["commit"], next.as_str());
    assert_eq!(world.refs()["main"], report["after"]);
    assert_eq!(report["pushed"], Value::Null);
}

#[test]
fn a_concurrent_push_fails_the_lease_and_the_next_sync_converges() {
    let world = World::new();
    world.push(&[("a", "1\n"), ("b", "1\n")], "First");
    world.sync();

    // Braid gets ahead: it imports a commit that is not on the remote yet.
    let (upstream, ahead) = world.commit("upstream", &[("a", "braid\n")], "Ahead");
    let import = world.braid(&["git", "import", upstream.to_str().unwrap()]);
    assert!(import.status.success());

    // Someone else's commit waits on a side branch of the remote, and the
    // remote's update hook moves main to it while braid's push is under way:
    // between braid's fetch and its push, someone else landed first.
    let (side, theirs) = world.commit("side", &[("b", "theirs\n")], "Theirs");
    git(&side, &["push", "--quiet", "origin", "main:side"]);
    let hook = world.remote.join("hooks/update");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        "#!/bin/sh\n\
         if [ \"$1\" = refs/heads/main ]; then\n\
         \x20 git update-ref refs/heads/main refs/heads/side\n\
         \x20 rm -f \"$0\"\n\
         fi\n",
    )
    .unwrap();
    make_executable(&hook);

    let refs = world.refs();
    let operations = world.operations();
    let (code, report) = world.sync();
    assert_eq!(code, Some(1), "{report}");
    assert_eq!(report["outcome"], "remote_moved");
    assert_eq!(report["found"], theirs.as_str());
    assert_eq!(report["pushed"], Value::Null);
    // Nothing moved in braid, and the remote has the other push only.
    assert_eq!(world.refs(), refs);
    assert_eq!(world.operations(), operations);
    assert_eq!(world.remote_head(), theirs);
    assert!(!hook.exists());

    // The next sync merges both lines and pushes the merge.
    let (code, report) = world.sync();
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["outcome"], "merged");
    let pushed = report["pushed"].as_str().unwrap();
    assert_eq!(world.remote_head(), pushed);
    assert_eq!(
        git(&world.remote, &["rev-list", "--parents", "-n", "1", "main"]),
        format!("{pushed} {ahead} {theirs}")
    );
    assert_eq!(git(&world.remote, &["show", "main:a"]), "braid");
    assert_eq!(git(&world.remote, &["show", "main:b"]), "theirs");
    world.assert_forward_only();

    let (code, again) = world.sync();
    assert_eq!(code, Some(0));
    assert_eq!(again["outcome"], "up_to_date");
    assert_eq!(again["operations"], Value::Array(Vec::new()));
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

#[test]
fn conflicts_exit_with_2_and_change_nothing() {
    let world = World::new();
    world.push(&[("f", "1\n2\n3\n")], "First");
    world.sync();
    let (upstream, _) = world.commit("upstream", &[("f", "1\nbraid\n3\n")], "Braid");
    assert!(
        world
            .braid(&["git", "import", upstream.to_str().unwrap()])
            .status
            .success()
    );
    let theirs = world.push(&[("f", "1\ngit\n3\n")], "Git");
    let refs = world.refs();
    let operations = world.operations();

    let (code, report) = world.sync();
    assert_eq!(code, Some(2), "{report}");
    assert_eq!(report["outcome"], "conflicts");
    let conflict = &report["conflicts"][0];
    assert_eq!(conflict["path"], "f");
    assert_eq!(conflict["kind"], "CONTENT");
    assert_eq!(conflict["ours"]["kind"], "file");
    assert_eq!(conflict["hunks"][0]["ours"]["text"], "braid\n");
    assert_eq!(conflict["hunks"][0]["theirs"]["text"], "git\n");
    assert_eq!(conflict["hunks"][0]["base"]["start"], 1);
    assert_eq!(world.refs(), refs);
    assert_eq!(world.operations(), operations);
    assert_eq!(world.remote_head(), theirs);

    // The text form says the same, with the same exit status.
    let output = world.braid(&["git", "sync", world.remote.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("nothing moved"), "{text}");
    assert!(text.contains("CONTENT       f"), "{text}");
    // And what to do: the two sides meet in git.
    assert!(
        text.ends_with(
            "Merge them in git and push the merge, then sync again: https://braid.elffuss.com/agents.md shows how, under \"Keeping the git remote up to date\".\n"
        ),
        "{text}"
    );
    world.assert_forward_only();
}

#[test]
fn a_dry_run_reports_and_writes_nothing() {
    let world = World::new();
    world.push(&[("a", "1\n")], "First");
    world.sync();
    world.push(&[("a", "2\n")], "Next");
    let refs = world.refs();
    let operations = world.operations();
    let (code, report) =
        world.braid_json(&["git", "sync", world.remote.to_str().unwrap(), "--dry-run"]);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["outcome"], "fast_forwarded");
    assert_eq!(report["imported"], 1);
    assert_eq!(world.refs(), refs);
    assert_eq!(world.operations(), operations);

    let output = world.braid(&["git", "sync", world.remote.to_str().unwrap(), "--dry-run"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("Would fast-forward main"), "{text}");
}

#[test]
fn a_remote_branch_by_another_name() {
    let world = World::new();
    world.push(&[("a", "1\n")], "First");
    git(&world.remote, &["branch", "-m", "main", "trunk"]);
    let (code, report) = world.braid_json(&[
        "git",
        "sync",
        world.remote.to_str().unwrap(),
        "--branch",
        "work",
        "--remote-branch",
        "trunk",
    ]);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["outcome"], "created");
    assert_eq!(report["branch"], "work");
    assert_eq!(report["remote_branch"], "trunk");
    assert_eq!(world.refs()["work"], report["after"]);
}

impl World {
    fn sync_direction(&self, direction: &str) -> (Option<i32>, Value) {
        self.braid_json(&[
            "git",
            "sync",
            self.remote.to_str().unwrap(),
            "--direction",
            direction,
        ])
    }

    /// Puts braid ahead of the remote: it imports a commit the remote does
    /// not have.
    fn braid_ahead(&self, files: &[(&str, &str)]) {
        let (upstream, _) = self.commit("upstream", files, "Braid's own");
        let import = self.braid(&["git", "import", upstream.to_str().unwrap()]);
        assert!(import.status.success());
    }
}

#[test]
fn publish_stops_when_someone_else_wrote_to_the_remote() {
    let world = World::new();
    world.push(&[("a", "1\n"), ("b", "1\n")], "First");
    world.sync();
    world.braid_ahead(&[("a", "braid\n")]);
    let theirs = world.push(&[("b", "theirs\n")], "Pushed past braid");

    let refs = world.refs();
    let operations = world.operations();
    for args in [&["--dry-run"][..], &[]] {
        let mut command = vec!["git", "sync", world.remote.to_str().unwrap()];
        command.extend(["--direction", "publish"]);
        command.extend(args);
        let (code, report) = world.braid_json(&command);
        assert_eq!(code, Some(3), "{report}");
        assert_eq!(report["outcome"], "diverged");
        assert_eq!(report["direction"], "publish");
        assert!(report["reason"].as_str().unwrap().contains("remote"));
        assert_eq!(report["pushed"], Value::Null);
        assert_eq!(report["operations"], Value::Array(Vec::new()));
    }
    // Nothing was recorded, merged or pushed.
    assert_eq!(world.refs(), refs);
    assert_eq!(world.operations(), operations);
    assert_eq!(world.remote_head(), theirs);
}

#[test]
fn publish_pushes_when_only_braid_moved() {
    let world = World::new();
    world.push(&[("a", "1\n")], "First");
    world.sync();
    world.braid_ahead(&[("a", "braid\n")]);
    let (code, report) = world.sync_direction("publish");
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["outcome"], "pushed");
    assert_eq!(world.remote_head(), report["pushed"].as_str().unwrap());
    world.assert_forward_only();
}

#[test]
fn follow_takes_the_remote_in_and_stops_on_braid_work_or_a_rewind() {
    let world = World::new();
    let first = world.push(&[("a", "1\n")], "First");
    world.sync();

    // Only the remote moved: follow fast-forwards.
    world.push(&[("a", "2\n")], "Second");
    let (code, report) = world.sync_direction("follow");
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["outcome"], "fast_forwarded");

    // Braid has a changeset of its own: follow stops and pushes nothing.
    let head = world.remote_head();
    world.braid_ahead(&[("b", "braid\n")]);
    let refs = world.refs();
    let (code, report) = world.sync_direction("follow");
    assert_eq!(code, Some(3), "{report}");
    assert_eq!(report["outcome"], "diverged");
    assert_eq!(world.refs(), refs);
    assert_eq!(world.remote_head(), head);

    // The remote went back: follow stops too.
    let world = World::new();
    world.push(&[("a", "1\n")], "First");
    world.push(&[("a", "2\n")], "Second");
    world.sync();
    git(
        &world.remote,
        &["update-ref", "refs/heads/main", first.as_str()],
    );
    let (code, report) = world.sync_direction("follow");
    assert_eq!(code, Some(3), "{report}");
    assert_eq!(report["outcome"], "diverged");
}
