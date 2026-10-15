// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! End-to-end tests of the commands that read derived data: `log -- <path>`,
//! `blame` and `derive`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn braid() -> Command {
    Command::new(env!("CARGO_BIN_EXE_braid"))
}

/// A repository with its own scratch directory for file contents.
struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    inputs: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let inputs = dir.path().join("inputs");
        std::fs::create_dir(&inputs).unwrap();
        let init = braid().arg("init").arg(&root).output().unwrap();
        assert!(init.status.success());
        Repo {
            _dir: dir,
            root,
            inputs,
        }
    }

    fn braid(&self, agent: &str) -> Command {
        let mut command = braid();
        command
            .arg("--repo")
            .arg(&self.root)
            .env("BRAID_AGENT", agent)
            .env("BRAID_AUTHOR", "Ada Lovelace <ada@example.org>")
            .env("TZ", "UTC0");
        command
    }

    /// Lands on main, as `agent`, a changeset on main's head that sets and
    /// deletes these files. Returns its id.
    fn change(
        &self,
        agent: &str,
        sets: &[(&str, &str)],
        deletes: &[&str],
        message: &str,
    ) -> String {
        let base = self.head().unwrap_or_else(|| "none".to_owned());
        let mut command = self.braid(agent);
        command.args([
            "change", "create", "--base", &base, "-m", message, "--land", "main",
        ]);
        for (n, (path, content)) in sets.iter().enumerate() {
            let input = self.inputs.join(format!("{message}-{n}"));
            std::fs::write(&input, content).unwrap();
            command
                .arg("--set")
                .arg(format!("{path}=@{}", input.display()));
        }
        for path in deletes {
            command.args(["--delete", path]);
        }
        let made = json(&mut command);
        made["change"].as_str().unwrap().to_owned()
    }

    fn head(&self) -> Option<String> {
        let refs = json(self.braid("tester").arg("refs"));
        refs["refs"]["main"].as_str().map(str::to_owned)
    }
}

fn output(command: &mut Command) -> std::process::Output {
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn json(command: &mut Command) -> Value {
    serde_json::from_slice(&output(command.arg("--json")).stdout).unwrap()
}

fn text(command: &mut Command) -> String {
    String::from_utf8(output(command).stdout).unwrap()
}

fn error(command: &mut Command) -> String {
    let out = command.output().unwrap();
    assert!(!out.status.success(), "{command:?} should fail");
    String::from_utf8(out.stderr).unwrap()
}

/// The ids `log` lists. `--json` goes first: after `--`, it would be a
/// path.
fn log_ids(repo: &Repo, args: &[&str]) -> Vec<String> {
    let out = output(repo.braid("tester").args(["--json", "log"]).args(args));
    let log: Value = serde_json::from_slice(&out.stdout).unwrap();
    log.as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_owned())
        .collect()
}

/// A small history: four changesets by three agents.
fn history() -> (Repo, [String; 4]) {
    let repo = Repo::new();
    let one = repo.change(
        "agent-1",
        &[
            ("README.md", "# Demo\n"),
            ("src/lib.rs", "fn a() {}\nfn b() {}\n"),
            ("src/util.rs", "fn u() {}\n"),
        ],
        &[],
        "one",
    );
    let two = repo.change(
        "agent-2",
        &[("src/lib.rs", "fn a() {}\nfn b2() {}\nfn c() {}\n")],
        &[],
        "two",
    );
    let three = repo.change("agent-1", &[], &["src/util.rs"], "three");
    let four = repo.change(
        "agent-3",
        &[("README.md", "# Demo\n\nMore.\n")],
        &[],
        "four",
    );
    (repo, [one, two, three, four])
}

#[test]
fn log_lists_the_changes_to_paths() {
    let (repo, [one, two, three, four]) = history();
    assert_eq!(
        log_ids(&repo, &["--", "src/lib.rs"]),
        [two.as_str(), one.as_str()]
    );
    assert_eq!(
        log_ids(&repo, &["main", "--", "src"]),
        [three.as_str(), two.as_str(), one.as_str()]
    );
    assert_eq!(
        log_ids(&repo, &["main", "--", "./src/"]),
        [three.as_str(), two.as_str(), one.as_str()]
    );
    assert_eq!(
        log_ids(&repo, &["main", "--", "src/util.rs"]),
        [three.as_str(), one.as_str()]
    );
    assert_eq!(
        log_ids(&repo, &["main", "-n", "1", "--", "src"]),
        [three.as_str()]
    );
    assert_eq!(
        log_ids(&repo, &["main", "--", "README.md", "src/util.rs"]),
        [four.as_str(), three.as_str(), one.as_str()]
    );
    assert_eq!(log_ids(&repo, &["main", "--", "."]).len(), 4);
    assert!(log_ids(&repo, &["main", "--", "nothing"]).is_empty());
    // From an earlier changeset, and without paths as before.
    assert_eq!(log_ids(&repo, &[&two, "--", "README.md"]), [one.as_str()]);
    assert_eq!(
        log_ids(&repo, &[]),
        [four.as_str(), three.as_str(), two.as_str(), one.as_str()]
    );

    let lines = text(
        repo.braid("tester")
            .args(["log", "main", "--", "src/lib.rs"]),
    );
    let lines: Vec<&str> = lines.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with(&two[..12]), "{lines:?}");
    assert!(lines[0].ends_with("(agent-2)  two"), "{lines:?}");

    let err = error(repo.braid("tester").args(["log", "main", "--", "../x"]));
    assert!(err.contains("not a path in the repository"), "{err}");
}

#[test]
fn blame_names_the_changeset_of_each_line() {
    let (repo, [one, two, _, four]) = history();
    let blame = json(repo.braid("tester").args(["blame", "main", "src/lib.rs"]));
    assert_eq!(blame["changeset"], four.as_str());
    assert_eq!(blame["path"], "src/lib.rs");
    let lines = blame["lines"].as_array().unwrap();
    let summary: Vec<(u64, &str, &str, &str)> = lines
        .iter()
        .map(|line| {
            (
                line["line"].as_u64().unwrap(),
                line["changeset"].as_str().unwrap(),
                line["agent"].as_str().unwrap(),
                line["content"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (1, one.as_str(), "agent-1", "fn a() {}"),
            (2, two.as_str(), "agent-2", "fn b2() {}"),
            (3, two.as_str(), "agent-2", "fn c() {}"),
        ]
    );
    assert_eq!(lines[0]["author"], "Ada Lovelace");
    assert!(lines[0]["time"].as_str().unwrap().ends_with("+0000"));

    // Text: short id, agent, date, line number and the line.
    let out = text(repo.braid("tester").args(["blame", "main", "src/lib.rs"]));
    let rows: Vec<&str> = out.lines().collect();
    assert_eq!(rows.len(), 3);
    let first: Vec<&str> = rows[0].split("  ").collect();
    assert_eq!(first[0], &one[..12]);
    assert_eq!(first[1], "agent-1");
    assert!(first[2].ends_with("+0000"), "{rows:?}");
    assert_eq!(first[3], "1");
    assert_eq!(first[4], "fn a() {}");
    assert!(rows[1].starts_with(&two[..12]), "{rows:?}");

    // At an earlier changeset, a file deleted since can be blamed.
    let util = json(repo.braid("tester").args(["blame", &two, "src/util.rs"]));
    assert_eq!(util["lines"][0]["changeset"], one.as_str());
    for (revision, path) in [
        ("main", "src/util.rs"),
        ("main", "src"),
        ("main", "nothing"),
    ] {
        let err = error(repo.braid("tester").args(["blame", revision, path]));
        assert!(err.contains("is not a file at"), "{err}");
    }
    assert!(
        text(repo.braid("tester").args(["blame", "--help"])).contains("Renames are not followed")
    );
}

#[test]
fn derive_computes_once_and_keeps_the_store_clean() {
    let (repo, [_, two, _, _]) = history();
    let derived = json(repo.braid("tester").arg("derive"));
    assert_eq!(derived["changesets"], 4);
    // Commit graph data and a changed-path filter for each.
    assert_eq!(derived["computed"], 8);
    let again = json(repo.braid("tester").args(["derive", "--all"]));
    assert_eq!(again["changesets"], 4);
    assert_eq!(again["computed"], 0);
    let partial = json(repo.braid("tester").args(["derive", &two]));
    assert_eq!(partial["changesets"], 2);
    assert_eq!(partial["computed"], 0);
    assert!(
        Path::new(&repo.root)
            .join(".braid/store/derived/index")
            .exists()
    );

    let out = text(repo.braid("tester").arg("derive"));
    assert_eq!(out, "4 changesets; 0 derived records computed\n");
    // Derived records are ordinary objects: the store stays clean.
    assert_eq!(json(repo.braid("tester").arg("fsck"))["clean"], true);

    // A repository without main needs revisions or --all.
    let empty = Repo::new();
    let err = error(empty.braid("tester").arg("derive"));
    assert!(err.contains("main"), "{err}");
    assert_eq!(
        json(empty.braid("tester").args(["derive", "--all"]))["changesets"],
        0
    );
}
