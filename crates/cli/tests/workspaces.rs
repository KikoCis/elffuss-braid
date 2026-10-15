// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! End-to-end tests of workspaces: several of them on one repository,
//! committing and landing in any order, conflicts, the secrets guard,
//! changesets made without a working copy, and many processes at once.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const AUTHOR: &str = "Ada <ada@example.org>";

/// A repository in a temporary directory, driven through the binary.
struct Repo {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let temp = tempfile::tempdir().unwrap();
        // Canonical, so paths compare equal to what braid records.
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let root = base.join("repo");
        let out = braid(&base, &["init", root.to_str().unwrap()]);
        assert!(out.status.success(), "{}", stderr(&out));
        Repo { _temp: temp, root }
    }

    /// Creates a workspace next to the repository's own directory.
    fn workspace(&self, name: &str) -> PathBuf {
        let dir = self.root.parent().unwrap().join(name);
        json(
            &self.root,
            &["workspace", "add", name, dir.to_str().unwrap()],
        );
        dir
    }

    fn main(&self) -> Option<String> {
        json(&self.root, &["refs"])["refs"]["main"]
            .as_str()
            .map(str::to_owned)
    }

    fn fsck_is_clean(&self) -> bool {
        json(&self.root, &["fsck"])["clean"] == true
    }
}

/// Runs braid in `dir` with a fixed author and agent.
fn braid(dir: &Path, args: &[&str]) -> Output {
    braid_as("tester", dir, args)
}

/// Runs braid in `dir` with a fixed author, as the agent `agent`.
fn braid_as(agent: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_braid"))
        .current_dir(dir)
        .args(args)
        .env("BRAID_AUTHOR", AUTHOR)
        .env("BRAID_AGENT", agent)
        .output()
        .unwrap()
}

/// Runs braid with `--json` as the agent `agent`, expecting exit status
/// `code`.
fn json_as(agent: &str, dir: &Path, args: &[&str], code: i32) -> Value {
    let mut all = args.to_vec();
    all.push("--json");
    let out = braid_as(agent, dir, &all);
    assert_eq!(
        out.status.code(),
        Some(code),
        "braid {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        stderr(&out)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Runs braid with `--json`, expecting exit status `code`.
fn json_status(dir: &Path, args: &[&str], code: i32) -> Value {
    json_as("tester", dir, args, code)
}

fn json(dir: &Path, args: &[&str]) -> Value {
    json_status(dir, args, 0)
}

/// Runs braid expecting failure, and returns its error message.
fn fails(dir: &Path, args: &[&str]) -> String {
    let out = braid(dir, args);
    assert_eq!(out.status.code(), Some(1), "braid {args:?} should fail");
    stderr(&out)
}

fn write(dir: &Path, name: &str, content: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

/// Seeds `main` with these files, through `change create`.
fn seed(repo: &Repo, files: &[(&str, &str)]) -> String {
    let mut args = vec![
        "change".to_owned(),
        "create".to_owned(),
        "--base".to_owned(),
        "none".to_owned(),
    ];
    for (name, content) in files {
        let source = repo
            .root
            .parent()
            .unwrap()
            .join(format!("seed-{}", name.replace('/', "-")));
        std::fs::write(&source, content).unwrap();
        args.push("--set".to_owned());
        args.push(format!("{name}=@{}", source.display()));
    }
    args.extend(["-m", "seed", "--land", "main"].map(str::to_owned));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = json(&repo.root, &args);
    out["change"].as_str().unwrap().to_owned()
}

fn status(dir: &Path) -> Value {
    json(dir, &["status"])
}

/// Every changeset on `main`'s first-parent chain, newest first.
fn history(repo: &Repo) -> Vec<Value> {
    json(&repo.root, &["log", "main", "-n", "100000"])
        .as_array()
        .unwrap()
        .clone()
}

fn rebased_from(changeset: &Value) -> Option<String> {
    changeset["extras"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|pair| (pair[0] == "rebased-from").then(|| pair[1].as_str().unwrap().to_owned()))
}

#[test]
fn disjoint_workspaces_land_in_either_order() {
    for a_first in [true, false] {
        let repo = Repo::new();
        seed(&repo, &[("README.md", "hello\n")]);
        let a = repo.workspace("a");
        let b = repo.workspace("b");
        assert_eq!(read(&a, "README.md"), "hello\n");

        write(&a, "src/a.rs", "pub fn a() {}\n");
        write(&b, "docs/b.md", "# b\n");
        let a_commit = json(&a, &["commit", "-m", "add a"]);
        let b_commit = json(&b, &["commit", "-m", "add b"]);
        assert_eq!(a_commit["changes"], 1);

        let (first, second) = if a_first { (&a, &b) } else { (&b, &a) };
        let landed = json(first, &["land"]);
        assert_eq!(landed["outcome"], "fast-forward");
        let landed = json(second, &["land"]);
        assert_eq!(landed["outcome"], "rebased");
        assert_eq!(landed["previous"], json(first, &["status"])["base"]);

        // Both changes are on main, once each, on a linear history.
        let log = history(&repo);
        assert_eq!(log.len(), 3);
        assert!(
            log.iter()
                .all(|c| c["parents"].as_array().unwrap().len() <= 1)
        );
        let originals: HashSet<String> = log[..2]
            .iter()
            .map(|c| rebased_from(c).unwrap_or_else(|| c["id"].as_str().unwrap().to_owned()))
            .collect();
        let made: HashSet<String> = [&a_commit, &b_commit]
            .iter()
            .map(|c| c["changeset"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(originals, made);

        // The second workspace has both files; the first catches up.
        assert_eq!(read(second, "src/a.rs"), "pub fn a() {}\n");
        assert_eq!(read(second, "docs/b.md"), "# b\n");
        assert_eq!(json(first, &["update"])["outcome"], "moved");
        assert_eq!(read(first, "docs/b.md"), "# b\n");
        assert_eq!(read(first, "src/a.rs"), "pub fn a() {}\n");
        for dir in [&a, &b] {
            let now = status(dir);
            assert_eq!(now["clean"], true);
            assert_eq!(now["base"].as_str(), repo.main().as_deref());
        }
        assert!(repo.fsck_is_clean());
    }
}

#[test]
fn same_lines_conflict_then_resolve() {
    let repo = Repo::new();
    seed(&repo, &[("shared.txt", "one\ntwo\nthree\n")]);
    let a = repo.workspace("a");
    let b = repo.workspace("b");
    write(&a, "shared.txt", "one\nTWO from a\nthree\n");
    write(&b, "shared.txt", "one\nTWO from b\nthree\n");
    json(&a, &["commit", "-m", "a's two"]);
    json(&b, &["commit", "-m", "b's two"]);
    json(&a, &["land"]);
    let main_before = repo.main();

    // The second landing conflicts: exit status 2, and main does not move.
    let landed = json_status(&b, &["land"], 2);
    assert_eq!(landed["outcome"], "conflicts");
    let conflict = &landed["conflicts"][0];
    assert_eq!(conflict["path"], "shared.txt");
    assert_eq!(conflict["kind"], "CONTENT");
    assert_eq!(conflict["hunks"][0]["ours"]["text"], "TWO from a\n");
    assert_eq!(conflict["hunks"][0]["theirs"]["text"], "TWO from b\n");
    assert_eq!(conflict["base"]["kind"], "file");
    assert_eq!(repo.main(), main_before);

    // Update writes the conflict with markers, and a rebase is in progress.
    let updated = json_status(&b, &["update"], 2);
    assert_eq!(updated["outcome"], "conflicts");
    let merged = read(&b, "shared.txt");
    assert!(merged.contains("<<<<<<< main"), "{merged}");
    assert!(
        merged.contains("TWO from a\n=======\nTWO from b\n"),
        "{merged}"
    );
    let now = status(&b);
    assert_eq!(now["rebase"]["conflicts"][0]["markers_remain"], true);
    assert_eq!(now["base"].as_str(), main_before.as_deref());
    // Landing waits for the rebase.
    assert!(fails(&b, &["land"]).contains("rebase is in progress"));

    // Committing with the markers still there is refused.
    let refused = fails(&b, &["commit", "-m", "resolved"]);
    assert!(refused.contains("conflict markers remain"), "{refused}");

    write(&b, "shared.txt", "one\nTWO from a and b\nthree\n");
    let committed = json(&b, &["commit", "-m", "both twos"]);
    assert_eq!(committed["rebase"]["outcome"], "finished");
    let landed = json(&b, &["land"]);
    assert_eq!(landed["outcome"], "fast-forward");
    assert_eq!(repo.main().as_deref(), committed["changeset"].as_str());
    // The resolution's first parent is a's change, and it names the change
    // it resolves.
    let log = history(&repo);
    assert_eq!(log[0]["parents"][0].as_str(), main_before.as_deref());
    assert!(rebased_from(&log[0]).is_some());
    assert_eq!(read(&b, "shared.txt"), "one\nTWO from a and b\nthree\n");
    assert!(repo.fsck_is_clean());
}

#[test]
fn landing_after_main_moved_rebases_and_follows_the_tree() {
    let repo = Repo::new();
    seed(&repo, &[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let ws = repo.workspace("ws");
    // Main moves without this workspace: a disjoint change, made without a
    // working copy.
    let main = repo.main().unwrap();
    let elsewhere = repo.root.parent().unwrap().join("other.txt");
    std::fs::write(&elsewhere, "from elsewhere\n").unwrap();
    json(
        &repo.root,
        &[
            "change",
            "create",
            "--base",
            &main,
            "--set",
            &format!("dir/other.txt=@{}", elsewhere.display()),
            "--delete",
            "b.txt",
            "-m",
            "elsewhere",
            "--land",
            "main",
        ],
    );
    write(&ws, "a.txt", "a, edited\n");
    let committed = json(&ws, &["commit", "-m", "edit a"]);
    let landed = json(&ws, &["land"]);
    assert_eq!(landed["outcome"], "rebased");
    let head = landed["head"].as_str().unwrap().to_owned();
    assert_eq!(repo.main().as_deref(), Some(head.as_str()));
    assert_ne!(Some(head.as_str()), committed["changeset"].as_str());

    // The working copy is the landed tree: main's change came in, the
    // deleted file went away, and nothing is left to commit.
    assert_eq!(read(&ws, "dir/other.txt"), "from elsewhere\n");
    assert!(!ws.join("b.txt").exists());
    assert_eq!(read(&ws, "a.txt"), "a, edited\n");
    let now = status(&ws);
    assert_eq!(now["clean"], true);
    assert_eq!(now["base"].as_str(), Some(head.as_str()));
    let show = json(&repo.root, &["show", &head]);
    let paths: Vec<&str> = show["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["a.txt"]);
    assert!(repo.fsck_is_clean());
}

#[test]
fn the_secrets_guard() {
    let repo = Repo::new();
    seed(&repo, &[("README.md", "hello\n")]);
    let ws = repo.workspace("ws");
    write(&ws, ".env", "TOKEN=secret\n");
    write(&ws, "keys/server.pem", "-----BEGIN-----\n");
    write(&ws, "keys/id_ed25519", "private\n");
    std::fs::write(ws.join("big.bin"), vec![7u8; (1 << 20) + 1]).unwrap();
    write(&ws, "notes.txt", "fine\n");

    let now = status(&ws);
    assert_eq!(now["added"], serde_json::json!(["notes.txt"]));
    assert_eq!(
        now["needs_track"],
        serde_json::json!([".env", "big.bin", "keys/id_ed25519", "keys/server.pem"])
    );
    // Only the ordinary file is committed.
    let committed = json(&ws, &["commit", "-m", "notes"]);
    assert_eq!(committed["changes"], 1);
    let show = json(
        &repo.root,
        &["show", committed["changeset"].as_str().unwrap()],
    );
    assert_eq!(show["changes"].as_array().unwrap().len(), 1);
    // Guarded files are not uncommitted changes: landing goes ahead.
    json(&ws, &["land"]);

    // Marked, they are committed.
    json(&ws, &["track", ".env", "keys"]);
    let now = status(&ws);
    assert_eq!(now["needs_track"], serde_json::json!(["big.bin"]));
    assert_eq!(
        now["added"],
        serde_json::json!([".env", "keys/id_ed25519", "keys/server.pem"])
    );
    let committed = json(&ws, &["commit", "-m", "keys"]);
    assert_eq!(committed["changes"], 3);
    assert_eq!(status(&ws)["needs_track"], serde_json::json!(["big.bin"]));
    // Nothing else to commit while only guarded files remain.
    assert!(fails(&ws, &["commit", "-m", "nothing"]).contains("nothing to commit"));
}

#[test]
fn change_create_with_expectations() {
    let repo = Repo::new();
    let seeded = seed(&repo, &[("config.toml", "a = 1\n")]);
    let show = json(&repo.root, &["show", &seeded]);
    let blob = show["changes"][0]["blob"].as_str().unwrap().to_owned();
    let new = repo.root.parent().unwrap().join("new.toml");
    std::fs::write(&new, "a = 2\n").unwrap();
    let set = format!("config.toml=@{}", new.display());

    // The expectation matches: made and landed.
    let made = json(
        &repo.root,
        &[
            "change",
            "create",
            "--base",
            "main",
            "--set",
            &set,
            "--expect",
            &format!("config.toml={blob}"),
            "--expect",
            "other.toml=absent",
            "-m",
            "a is 2",
            "--land",
            "main",
        ],
    );
    assert_eq!(made["land"]["outcome"], "fast-forward");
    assert_eq!(repo.main().as_deref(), made["change"].as_str());

    // Another agent, still believing the old content, is stopped before
    // anything is made: its base holds another blob now.
    let ops = json(&repo.root, &["op", "log"]).as_array().unwrap().len();
    let refused = fails(
        &repo.root,
        &[
            "change",
            "create",
            "--base",
            "main",
            "--set",
            &set,
            "--expect",
            &format!("config.toml={blob}"),
            "-m",
            "stale",
            "--land",
            "main",
        ],
    );
    assert!(refused.contains("expected"), "{refused}");
    let refused = fails(
        &repo.root,
        &[
            "change",
            "create",
            "--base",
            "main",
            "--set",
            &set,
            "--expect",
            "config.toml=absent",
            "-m",
            "stale",
        ],
    );
    assert!(refused.contains("absent"), "{refused}");
    assert_eq!(repo.main().as_deref(), made["change"].as_str());
    assert_eq!(
        json(&repo.root, &["op", "log"]).as_array().unwrap().len(),
        ops
    );

    // Built on the old base and landed now, the stale edit conflicts: exit
    // status 2, and main stays.
    let stale = repo.root.parent().unwrap().join("stale.toml");
    std::fs::write(&stale, "a = 3\n").unwrap();
    let conflicted = json_status(
        &repo.root,
        &[
            "change",
            "create",
            "--base",
            &seeded,
            "--set",
            &format!("config.toml=@{}", stale.display()),
            "-m",
            "a is 3",
            "--land",
            "main",
        ],
        2,
    );
    assert_eq!(conflicted["land"]["outcome"], "conflicts");
    assert_eq!(conflicted["land"]["conflicts"][0]["kind"], "CONTENT");
    assert_eq!(repo.main().as_deref(), made["change"].as_str());

    // Content from standard input.
    let mut child = Command::new(env!("CARGO_BIN_EXE_braid"))
        .current_dir(&repo.root)
        .args([
            "change",
            "create",
            "--base",
            "main",
            "--set",
            "piped.txt=@-",
            "-m",
            "piped",
            "--json",
        ])
        .env("BRAID_AUTHOR", AUTHOR)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), b"from a pipe\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let piped: Value = serde_json::from_slice(&out.stdout).unwrap();
    let shown = json(&repo.root, &["show", piped["change"].as_str().unwrap()]);
    assert_eq!(shown["changes"][0]["path"], "piped.txt");
    assert_eq!(shown["changes"][0]["size"], 12);
    assert!(repo.fsck_is_clean());
}

#[test]
fn the_short_id_commit_prints_works_everywhere() {
    let repo = Repo::new();
    seed(&repo, &[("README.md", "hello\n")]);
    let ws = repo.workspace("ws");
    write(&ws, "notes.txt", "notes\n");
    let out = braid(&ws, &["commit", "-m", "notes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("Committed "), "{text}");
    let short = text.split_whitespace().nth(1).unwrap().to_owned();
    assert_eq!(short.len(), 12, "{text}");

    // show and log take it, and give the full id in JSON.
    let shown = json(&repo.root, &["show", &short]);
    let full = shown["id"].as_str().unwrap().to_owned();
    assert_eq!(full.len(), 64);
    assert!(full.starts_with(&short));
    assert_eq!(
        json(&repo.root, &["log", &short[..6], "-n", "1"])[0]["id"],
        full.as_str()
    );
    // So do --at and --base.
    let other = repo.root.parent().unwrap().join("other");
    let added = json(
        &repo.root,
        &[
            "workspace",
            "add",
            "other",
            other.to_str().unwrap(),
            "--at",
            &short[..8],
        ],
    );
    assert_eq!(added["base"], full.as_str());
    assert_eq!(read(&other, "notes.txt"), "notes\n");
    let made = json(
        &repo.root,
        &[
            "change",
            "create",
            "--base",
            &short[..7],
            "--delete",
            "notes.txt",
            "-m",
            "no notes",
        ],
    );
    assert_eq!(made["base"], full.as_str());
    // commit --json gives the full id.
    write(&ws, "more.txt", "more\n");
    let committed = json(&ws, &["commit", "-m", "more"]);
    assert_eq!(committed["changeset"].as_str().unwrap().len(), 64);
}

#[test]
fn changesets_name_the_agent_that_made_them() {
    let repo = Repo::new();
    seed(&repo, &[("shared.txt", "one\ntwo\nthree\n")]);
    let a = repo.workspace("a");
    let b = repo.workspace("b");
    write(&a, "shared.txt", "one\nTWO from a\nthree\n");
    json_as("agent-a", &a, &["commit", "-m", "a's two"], 0);
    json_as("agent-a", &a, &["land"], 0);
    write(&b, "b.txt", "b\n");
    json_as("agent-b", &b, &["commit", "-m", "add b"], 0);
    write(&b, "shared.txt", "one\nTWO from b\nthree\n");
    json_as("agent-b", &b, &["commit", "-m", "b's two"], 0);

    // Rebased by an update and resolved under another label, b's changes
    // still name agent-b; landing records the lander in the op log only.
    json_as("fixer", &b, &["update"], 2);
    write(&b, "shared.txt", "one\nTWO from a and b\nthree\n");
    json_as("fixer", &b, &["commit"], 0);
    let landed = json_as("lander", &b, &["land"], 0);
    let log = history(&repo);
    let agents: Vec<&str> = log.iter().map(|c| c["agent"].as_str().unwrap()).collect();
    assert_eq!(agents, ["agent-b", "agent-b", "agent-a", "tester"]);
    for changeset in &log[..2] {
        assert!(rebased_from(changeset).is_some());
        let extras = changeset["extras"].as_array().unwrap();
        assert_eq!(extras[0], serde_json::json!(["agent", "agent-b"]));
    }
    let ops = json(&repo.root, &["op", "log", "-n", "1"]);
    assert_eq!(ops[0]["agent"], "lander");
    assert_eq!(ops[0]["seq"], landed["operation"]);

    // The text forms show it too.
    let out = braid(&repo.root, &["show", "main"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("\nagent     agent-b\n"), "{text}");
    let out = braid(&repo.root, &["log", "-n", "1"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("  Ada (agent-b)  b's two"), "{text}");
}

#[test]
fn removing_a_workspace() {
    let repo = Repo::new();
    seed(&repo, &[("README.md", "hello\n")]);
    let ws = repo.workspace("ws");
    write(&ws, "draft.txt", "not committed\n");
    let refused = fails(&repo.root, &["workspace", "remove", "ws"]);
    assert!(refused.contains("uncommitted changes"), "{refused}");
    assert!(ws.join("draft.txt").exists());

    // Committed but not landed: still refused.
    json(&ws, &["commit", "-m", "draft"]);
    let refused = fails(&repo.root, &["workspace", "remove", "ws"]);
    assert!(refused.contains("does not have"), "{refused}");

    // Landed: removed, directory and all.
    json(&ws, &["land"]);
    json(&repo.root, &["workspace", "remove", "ws"]);
    assert!(!ws.exists());
    assert!(
        json(&repo.root, &["workspace", "list"])
            .as_array()
            .unwrap()
            .is_empty()
    );

    // --force removes a dirty one.
    let other = repo.workspace("other");
    write(&other, "scratch.txt", "x\n");
    json(&repo.root, &["workspace", "remove", "other", "--force"]);
    assert!(!other.exists());
}

#[test]
fn workspaces_are_found_from_anywhere_inside() {
    let repo = Repo::new();
    seed(&repo, &[("src/lib.rs", "pub fn f() {}\n")]);
    let ws = repo.workspace("ws");
    let inside = ws.join("src");
    write(&ws, "src/lib.rs", "pub fn g() {}\n");
    let now = status(&inside);
    assert_eq!(now["workspace"], "ws");
    assert_eq!(now["modified"], serde_json::json!(["src/lib.rs"]));
    // Paths are relative to the current directory.
    write(&ws, "src/new.rs", "x\n");
    write(&ws, "top.txt", "y\n");
    let committed = json(&inside, &["commit", "-m", "only src", "new.rs", "lib.rs"]);
    assert_eq!(committed["changes"], 2);
    assert_eq!(status(&ws)["added"], serde_json::json!(["top.txt"]));
    // The repository root is not a workspace.
    assert!(fails(&repo.root, &["status"]).contains("not inside a workspace"));
    // --workspace names one from anywhere in the repository.
    assert_eq!(
        json(&repo.root, &["status", "--workspace", "ws"])["workspace"],
        "ws"
    );
}

#[test]
fn stacked_changesets_land_in_order() {
    let repo = Repo::new();
    seed(&repo, &[("a.txt", "1\n"), ("b.txt", "1\n")]);
    let ws = repo.workspace("ws");
    let other = repo.workspace("other");
    write(&ws, "a.txt", "2\n");
    let first = json(&ws, &["commit", "-m", "first"]);
    write(&ws, "a.txt", "3\n");
    let second = json(&ws, &["commit", "-m", "second"]);
    assert_eq!(status(&ws)["unlanded"].as_array().unwrap().len(), 2);
    write(&other, "b.txt", "other\n");
    json(&other, &["commit", "-m", "other"]);
    json(&other, &["land"]);

    // Both are rebased onto the other workspace's change, in order.
    let landed = json(&ws, &["land"]);
    assert_eq!(landed["outcome"], "rebased");
    let log = history(&repo);
    let messages: Vec<&str> = log.iter().map(|c| c["message"].as_str().unwrap()).collect();
    assert_eq!(messages, ["second", "first", "other", "seed"]);
    assert_eq!(
        rebased_from(&log[0]).as_deref(),
        second["changeset"].as_str()
    );
    assert_eq!(
        rebased_from(&log[1]).as_deref(),
        first["changeset"].as_str()
    );
    assert_eq!(read(&ws, "a.txt"), "3\n");
    assert_eq!(read(&ws, "b.txt"), "other\n");
    assert!(repo.fsck_is_clean());
}

#[test]
fn a_conflict_that_is_not_about_content_needs_resolve() {
    let repo = Repo::new();
    seed(&repo, &[("gone.txt", "1\n"), ("keep.txt", "k\n")]);
    let a = repo.workspace("a");
    let b = repo.workspace("b");
    std::fs::remove_file(a.join("gone.txt")).unwrap();
    json(&a, &["commit", "-m", "delete it"]);
    json(&a, &["land"]);
    write(&b, "gone.txt", "2\n");
    json(&b, &["commit", "-m", "edit it"]);
    let updated = json_status(&b, &["update"], 2);
    assert_eq!(updated["conflicts"][0]["kind"], "MODIFY_DELETE");
    // The head's version stays: deleted.
    assert!(!b.join("gone.txt").exists());
    let refused = fails(&b, &["commit", "-m", "x"]);
    assert!(refused.contains("braid resolve"), "{refused}");
    let resolved = json(&b, &["resolve", "gone.txt"]);
    assert!(resolved["remaining"].as_array().unwrap().is_empty());
    // Keeping the deletion leaves nothing of the change: the rebase ends
    // without a changeset.
    let committed = json(&b, &["commit"]);
    assert!(committed["changeset"].is_null());
    assert_eq!(committed["rebase"]["outcome"], "finished");
    assert_eq!(status(&b)["base"].as_str(), repo.main().as_deref());
    assert_eq!(json(&b, &["land"])["outcome"], "nothing");
}

/// Eight processes, each with its own workspace, edit their own file and
/// their own line of a shared file, commit and land, round after round,
/// all at once. Afterwards every change is on main exactly once, main's
/// history is linear, and the store is clean.
#[test]
fn eight_agents_land_at_once() {
    const AGENTS: usize = 8;
    const ROUNDS: usize = 4;
    let repo = Repo::new();
    // One line per agent, with two untouched lines between them, so that
    // edits to different agents' lines merge.
    let shared: String = (0..AGENTS)
        .map(|i| format!("agent {i}: start\n--\n--\n"))
        .collect();
    seed(&repo, &[("shared.txt", &shared)]);
    let dirs: Vec<PathBuf> = (0..AGENTS)
        .map(|i| repo.workspace(&format!("agent-{i}")))
        .collect();

    let made: Vec<Vec<String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = dirs
            .iter()
            .enumerate()
            .map(|(i, dir)| {
                scope.spawn(move || {
                    let mut commits = Vec::new();
                    for round in 0..ROUNDS {
                        write(
                            dir,
                            &format!("own-{i}.txt"),
                            &format!("agent {i} round {round}\n"),
                        );
                        let text = read(dir, "shared.txt");
                        let lines: Vec<String> = text
                            .lines()
                            .enumerate()
                            .map(|(n, line)| {
                                if n == i * 3 {
                                    format!("agent {i}: round {round}")
                                } else {
                                    line.to_owned()
                                }
                            })
                            .collect();
                        write(dir, "shared.txt", &(lines.join("\n") + "\n"));
                        let committed =
                            json(dir, &["commit", "-m", &format!("agent {i} round {round}")]);
                        commits.push(committed["changeset"].as_str().unwrap().to_owned());
                        loop {
                            let out = braid(dir, &["land", "--json"]);
                            match out.status.code() {
                                Some(0) => break,
                                Some(2) => {
                                    // Someone else's change got in the way:
                                    // rebase onto it and try again.
                                    json(dir, &["update"]);
                                }
                                _ => panic!("land failed: {}", stderr(&out)),
                            }
                        }
                    }
                    commits
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Every changeset on main maps back to one of the changesets made,
    // through the rebases it went through.
    let log = history(&repo);
    assert_eq!(log.len(), AGENTS * ROUNDS + 1);
    let mut seen = HashMap::new();
    for changeset in &log[..log.len() - 1] {
        assert_eq!(changeset["parents"].as_array().unwrap().len(), 1, "a merge");
        let mut id = changeset["id"].as_str().unwrap().to_owned();
        let mut current = changeset.clone();
        while let Some(from) = rebased_from(&current) {
            current = json(&repo.root, &["show", &from]);
            id = from;
        }
        *seen.entry(id).or_insert(0) += 1;
    }
    let all: HashSet<String> = made.iter().flatten().cloned().collect();
    assert_eq!(all.len(), AGENTS * ROUNDS);
    assert_eq!(seen.keys().cloned().collect::<HashSet<_>>(), all);
    assert!(seen.values().all(|count| *count == 1), "{seen:?}");

    // A fresh workspace has every agent's last round.
    let check = repo.workspace("check");
    let text = read(&check, "shared.txt");
    for i in 0..AGENTS {
        assert_eq!(
            read(&check, &format!("own-{i}.txt")),
            format!("agent {i} round {}\n", ROUNDS - 1)
        );
        assert!(
            text.contains(&format!("agent {i}: round {}\n", ROUNDS - 1)),
            "{text}"
        );
    }
    assert!(repo.fsck_is_clean());
}
