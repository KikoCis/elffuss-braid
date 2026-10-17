// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! End-to-end tests of what an agent session that follows the braid section
//! of a project's AGENTS.md relies on: where `workspace add` puts a
//! workspace, git refusing to work inside one, the branch it tracks, hints
//! that repeat the session's flags, and reading a file as a changeset has
//! it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// Git without the user's or the system's configuration.
const GIT_ENV: [(&str, &str); 6] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Ada Lovelace"),
    ("GIT_AUTHOR_EMAIL", "ada@example.org"),
    ("GIT_COMMITTER_NAME", "Ada Lovelace"),
    ("GIT_COMMITTER_EMAIL", "ada@example.org"),
];

/// Runs braid in `dir`, with an author but no agent from the environment.
fn braid(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_braid"))
        .current_dir(dir)
        .args(args)
        .envs(GIT_ENV)
        .env("BRAID_AUTHOR", "Ada <ada@example.org>")
        .env_remove("BRAID_AGENT")
        .output()
        .unwrap()
}

/// Runs braid expecting exit status `code`, and returns what it printed.
fn run(dir: &Path, args: &[&str], code: i32) -> String {
    let out = braid(dir, args);
    assert_eq!(
        out.status.code(),
        Some(code),
        "braid {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    run(dir, args, 0)
}

fn json(dir: &Path, args: &[&str], code: i32) -> Value {
    let mut all = args.to_vec();
    all.push("--json");
    serde_json::from_str(&run(dir, &all, code)).unwrap()
}

/// Runs braid expecting it to fail, and returns its error.
fn fails(dir: &Path, args: &[&str]) -> String {
    let out = braid(dir, args);
    assert_eq!(out.status.code(), Some(1), "braid {args:?}");
    assert!(out.stdout.is_empty(), "braid {args:?}");
    String::from_utf8(out.stderr).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .envs(GIT_ENV)
        .output()
        .expect("git must be installed to run this test")
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A git project set up for braid as agents.md does it, on `branch`, with
/// one commit of `notes.txt`.
fn project(branch: &str) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let dir = fs::canonicalize(temp.path()).unwrap();
    git_ok(&dir, &["init", "--quiet", "-b", branch]);
    fs::write(dir.join("notes.txt"), "one\ntwo\n").unwrap();
    git_ok(&dir, &["add", "notes.txt"]);
    git_ok(&dir, &["commit", "--quiet", "-m", "First"]);
    ok(&dir, &["init"]);
    let exclude = git_ok(&dir, &["rev-parse", "--git-path", "info/exclude"]);
    let exclude = dir.join(exclude.trim());
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    let mut lines = fs::read_to_string(&exclude).unwrap_or_default();
    lines.push_str(".braid\n");
    fs::write(&exclude, lines).unwrap();
    ok(&dir, &["git", "import", "."]);
    (temp, dir)
}

#[test]
fn a_workspace_goes_in_braid_work_and_git_refuses_to_work_in_it() {
    let (_temp, dir) = project("main");
    let deep = dir.join("src/deep");
    fs::create_dir_all(&deep).unwrap();
    // From anywhere in the project, the workspace goes in .braid/work.
    let made = json(&deep, &["workspace", "add", "agent-1"], 0);
    let workspace = dir.join(".braid/work/agent-1");
    assert_eq!(made["path"], workspace.to_str().unwrap());
    assert_eq!(made["files"], 1);
    assert!(!deep.join("agent-1").exists());
    let guard = fs::read_to_string(workspace.join(".git")).unwrap();
    assert!(guard.starts_with("gitdir: "), "{guard}");

    // The user's own edit in the shared checkout, and the agent's in its
    // workspace.
    fs::write(dir.join("notes.txt"), "one, the user's\ntwo\n").unwrap();
    fs::write(workspace.join("notes.txt"), "one\ntwo, the agent's\n").unwrap();
    for args in [
        &["stash"][..],
        &["status"],
        &["diff"],
        &["reset", "--hard"],
        &["checkout", "--", "."],
        &["clean", "-fdx"],
        &["add", "-A"],
        &["rev-parse", "--show-toplevel"],
    ] {
        let out = git(&workspace, args);
        assert!(!out.status.success(), "git {args:?} worked in a workspace");
        let err = String::from_utf8_lossy(&out.stderr);
        // Only the refusal is the same in every git: older versions repeat
        // the guard's text, which names braid, and newer ones say "(null)".
        assert!(err.contains("not a git repository"), "git {args:?}: {err}");
    }
    assert_eq!(
        fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "one, the user's\ntwo\n"
    );
    assert_eq!(git_ok(&dir, &["stash", "list"]), "");

    // Braid does not see the guard, nor does the shared checkout's git see
    // the workspace.
    let status = json(&workspace, &["status"], 0);
    assert_eq!(status["modified"], serde_json::json!(["notes.txt"]));
    assert_eq!(status["added"], serde_json::json!([]));
    assert_eq!(
        git_ok(&dir, &["status", "--short"]),
        " M notes.txt\n",
        "only the user's edit"
    );
    ok(&workspace, &["commit", "-m", "Agent's edit"]);
    ok(&workspace, &["land"]);
    ok(&dir, &["workspace", "remove", "agent-1"]);
    assert!(!workspace.exists());
}

#[test]
fn outside_a_workspace_the_error_says_how_to_reach_one() {
    let (_temp, dir) = project("main");
    let err = fails(&dir, &["status"]);
    assert!(err.contains("not inside a workspace"), "{err}");
    assert!(err.contains("--workspace <name>"), "{err}");
    assert!(err.contains(".braid/work/<name>"), "{err}");
}

#[test]
fn a_workspace_tracks_a_branch_the_repository_has() {
    let (_temp, dir) = project("master");
    let err = fails(&dir, &["workspace", "add", "s2", "--track", "main"]);
    assert!(
        err.contains("the repository has no branch \"main\"; it has master"),
        "{err}"
    );
    assert!(err.contains("--new-branch"), "{err}");
    assert!(!dir.join(".braid/work/s2").exists());
    assert_eq!(json(&dir, &["workspace", "list"], 0), serde_json::json!([]));
    for own in ["braid/git-map", "tags/v1"] {
        let err = fails(&dir, &["workspace", "add", "s3", "--track", own]);
        assert!(err.contains("braid's own namespaces"), "{err}");
    }

    // The branch it has, or a new one on purpose.
    let made = json(&dir, &["workspace", "add", "s2", "--track", "master"], 0);
    assert_eq!(made["files"], 1);
    let made = json(
        &dir,
        &["workspace", "add", "s4", "--track", "topic", "--new-branch"],
        0,
    );
    assert_eq!(made["branch"], "topic");
    assert_eq!(made["base"], Value::Null);

    // A repository with no branch yet takes any name: the first landing
    // starts it.
    let temp = tempfile::tempdir().unwrap();
    let empty = fs::canonicalize(temp.path()).unwrap();
    ok(&empty, &["init"]);
    let made = json(&empty, &["workspace", "add", "first"], 0);
    assert_eq!(made["branch"], "main");
}

#[test]
fn hints_repeat_the_flags_the_session_gave() {
    let (_temp, dir) = project("main");
    for name in ["agent-1", "agent-2"] {
        ok(&dir, &["--agent", name, "workspace", "add", name]);
    }
    let work = |name: &str| dir.join(".braid/work").join(name);
    fs::write(work("agent-1").join("notes.txt"), "one, from 1\ntwo\n").unwrap();
    fs::write(work("agent-2").join("notes.txt"), "one, from 2\ntwo\n").unwrap();
    let session = |name: &'static str| ["--agent", name, "--workspace", name];
    for name in ["agent-1", "agent-2"] {
        let mut args = session(name).to_vec();
        args.extend(["commit", "-m", name]);
        ok(&dir, &args);
    }
    let mut land = session("agent-1").to_vec();
    land.push("land");
    ok(&dir, &land);

    // From the project root, as a shell that does not keep its directory
    // runs them: the hints name the agent and the workspace.
    let mut land = session("agent-2").to_vec();
    land.push("land");
    let landed = json(&dir, &land, 2);
    assert_eq!(landed["outcome"], "conflicts");
    assert_eq!(
        landed["next"],
        "braid --agent agent-2 --workspace agent-2 update"
    );
    let said = run(&dir, &land, 2);
    assert!(
        said.contains("Run `braid --agent agent-2 --workspace agent-2 update`"),
        "{said}"
    );
    let mut update = session("agent-2").to_vec();
    update.push("update");
    let updated = json(&dir, &update, 2);
    assert_eq!(
        updated["next"],
        "braid --agent agent-2 --workspace agent-2 commit"
    );
    // The same flags after the command name count as well.
    let status = run(
        &dir,
        &["status", "--workspace", "agent-2", "--agent", "agent-2"],
        0,
    );
    assert!(
        status.contains("then `braid --agent agent-2 --workspace agent-2 commit`"),
        "{status}"
    );
    // Settled, committed and landed as the hints say, the op log names the
    // session for each step.
    fs::write(
        work("agent-2").join("notes.txt"),
        "one, from 1 and 2\ntwo\n",
    )
    .unwrap();
    let commit = run(
        &dir,
        &["--agent", "agent-2", "--workspace", "agent-2", "commit"],
        0,
    );
    assert!(
        commit.contains("`braid --agent agent-2 --workspace agent-2 land` when ready"),
        "{commit}"
    );
    ok(&dir, &land);
    let ops = json(&dir, &["op", "log", "-n", "1"], 0);
    assert_eq!(ops[0]["agent"], "agent-2");

    // agent-1 is behind now. A label that a shell would split is quoted.
    let behind = run(
        &dir,
        &["--agent", "agent one", "--workspace", "agent-1", "status"],
        0,
    );
    assert!(
        behind.contains("run `braid --agent 'agent one' --workspace agent-1 update`"),
        "{behind}"
    );
    // With no flags, the hints are as plain as ever.
    let behind = run(&work("agent-1"), &["status"], 0);
    assert!(behind.contains("run `braid update`"), "{behind}");
}

#[test]
fn cat_object_reads_a_file_as_a_changeset_has_it() {
    let (_temp, dir) = project("main");
    ok(&dir, &["workspace", "add", "agent-1"]);
    let workspace = dir.join(".braid/work/agent-1");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(workspace.join("src/lib.rs"), "fn a() {}\n").unwrap();
    ok(&workspace, &["commit", "-m", "Add src/lib.rs"]);
    fs::write(workspace.join("notes.txt"), "one, changed\ntwo\n").unwrap();
    let base = json(&workspace, &["status"], 0)["base"]
        .as_str()
        .unwrap()
        .to_owned();

    // The base's version of an edited file: what reviewing or undoing the
    // edit needs.
    assert_eq!(
        ok(&workspace, &["cat-object", &format!("{base}:notes.txt")]),
        "one\ntwo\n"
    );
    assert_eq!(
        ok(
            &workspace,
            &["cat-object", &format!("{}:src/lib.rs", &base[..12])]
        ),
        "fn a() {}\n"
    );
    assert_eq!(ok(&dir, &["cat-object", "main:notes.txt"]), "one\ntwo\n");
    let blob = json(&dir, &["cat-object", "main:./notes.txt"], 0);
    assert_eq!(blob["kind"], "blob");
    assert_eq!(blob["size"], 8);
    let tree = json(&workspace, &["cat-object", &format!("{base}:src")], 0);
    assert_eq!(tree["kind"], "tree");
    assert_eq!(json(&dir, &["cat-object", "main:"], 0)["kind"], "tree");
    let err = fails(&dir, &["cat-object", "main:src/lib.rs"]);
    assert!(
        err.contains("no file or directory at \"src/lib.rs\""),
        "{err}"
    );
    fails(&dir, &["cat-object", "nope:notes.txt"]);
}
