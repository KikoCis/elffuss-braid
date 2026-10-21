// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! `braid revert` end to end: a revert lands like any change, rebased over
//! later work, and stops with conflicts as `braid land` does when later
//! work touched the same lines.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn braid(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_braid"))
        .current_dir(dir)
        .args(args)
        .env("BRAID_AGENT", "tester")
        .env("BRAID_AUTHOR", "Ada <ada@example.org>")
        .env("BRAID_HUB", "off")
        .output()
        .unwrap()
}

/// Runs braid with `--json`, expecting exit status `code`.
fn run(dir: &Path, args: &[&str], code: i32) -> Value {
    let mut all = args.to_vec();
    all.push("--json");
    let out = braid(dir, &all);
    assert_eq!(
        out.status.code(),
        Some(code),
        "braid {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

fn id(value: &Value) -> String {
    value.as_str().unwrap().to_owned()
}

struct Repo {
    _temp: tempfile::TempDir,
    dir: PathBuf,
    files: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let dir = base.join("repo");
        let files = base.join("files");
        std::fs::create_dir(&files).unwrap();
        run(&base, &["init", dir.to_str().unwrap()], 0);
        Repo {
            _temp: temp,
            dir,
            files,
        }
    }

    /// Lands a change on `main` that sets each path to its text, with
    /// `message`, and returns the changeset that holds it on `main`.
    fn land(&self, sets: &[(&str, &str)], message: &str) -> String {
        let main = self.head();
        let base = main.as_deref().unwrap_or("none");
        let mut args = vec![
            "change".to_owned(),
            "create".to_owned(),
            "--base".to_owned(),
            base.to_owned(),
        ];
        for (n, (path, text)) in sets.iter().enumerate() {
            let file = self.files.join(format!("{message}-{n}"));
            std::fs::write(&file, text).unwrap();
            args.push("--set".to_owned());
            args.push(format!("{path}=@{}", file.display()));
        }
        args.extend(["-m", message, "--land", "main"].map(str::to_owned));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(&self.dir, &args, 0);
        id(&out["land"]["head"])
    }

    fn head(&self) -> Option<String> {
        let refs = run(&self.dir, &["refs"], 0);
        refs["refs"]["main"].as_str().map(str::to_owned)
    }

    fn cat(&self, path: &str) -> String {
        let out = braid(&self.dir, &["cat-object", &format!("main:{path}")]);
        assert!(out.status.success(), "{path}");
        String::from_utf8(out.stdout).unwrap()
    }
}

#[test]
fn a_revert_lands_and_puts_the_files_back() {
    let r = Repo::new();
    r.land(&[("a", "1\n2\n3\n"), ("b", "b\n")], "Seed");
    let culprit = r.land(&[("a", "1\nbad\n3\n"), ("new", "n\n")], "Break a\n\nbody");
    let out = run(&r.dir, &["revert", &culprit[..10]], 0);
    let revert = id(&out["revert"]);
    assert_eq!(out["reverts"], culprit.as_str());
    assert_eq!(out["branch"], "main");
    assert_eq!(out["land"]["outcome"], "fast-forward");
    assert_eq!(out["land"]["head"], revert.as_str());
    assert_eq!(r.cat("a"), "1\n2\n3\n");
    assert!(!braid(&r.dir, &["cat-object", "main:new"]).status.success());

    let shown = run(&r.dir, &["show", &revert], 0);
    assert_eq!(shown["parents"][0], culprit.as_str());
    assert_eq!(
        shown["message"],
        format!("Revert \"Break a\"\n\nThis reverts changeset {culprit}.\n")
    );
    assert_eq!(shown["agent"], "tester");
    assert!(
        shown["extras"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(["reverts", culprit]))
    );
    // A text report says what happened.
    let text = braid(&r.dir, &["revert", &revert]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.starts_with(&format!("Reverted {}", &revert[..12])),
        "{text}"
    );
    assert_eq!(r.cat("a"), "1\nbad\n3\n");
}

#[test]
fn a_revert_rebases_over_later_work() {
    let r = Repo::new();
    r.land(&[("a", "1\n2\n3\n4\n5\n")], "Seed");
    let culprit = r.land(&[("a", "1\n2\nbad\n4\n5\n")], "Break");
    r.land(&[("a", "one\n2\nbad\n4\n5\n"), ("c", "c\n")], "Later");
    let out = run(&r.dir, &["revert", &culprit, "--branch", "main"], 0);
    assert_eq!(out["land"]["outcome"], "rebased");
    assert_eq!(r.head().unwrap(), id(&out["land"]["head"]));
    assert_eq!(r.cat("a"), "one\n2\n3\n4\n5\n");
    assert_eq!(r.cat("c"), "c\n");
}

#[test]
fn a_revert_that_conflicts_moves_nothing_and_exits_with_2() {
    let r = Repo::new();
    r.land(&[("a", "1\n2\n3\n")], "Seed");
    let culprit = r.land(&[("a", "1\nbad\n3\n")], "Break");
    let later = r.land(&[("a", "1\nworse\n3\n")], "Later");
    let out = run(&r.dir, &["revert", &culprit], 2);
    let land = &out["land"];
    assert_eq!(land["outcome"], "conflicts");
    assert_eq!(land["head"], later.as_str());
    assert_eq!(land["change"], out["revert"]);
    let conflict = &land["conflicts"][0];
    assert_eq!(conflict["path"], "a");
    assert_eq!(conflict["kind"], "CONTENT");
    assert_eq!(conflict["hunks"][0]["branch"]["text"], "worse\n");
    assert_eq!(conflict["hunks"][0]["workspace"]["text"], "2\n");
    assert_eq!(r.head().unwrap(), later);
    let text = braid(&r.dir, &["revert", &culprit]);
    assert_eq!(text.status.code(), Some(2));
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("did not move"), "{text}");
}

#[test]
fn no_land_only_makes_the_revert() {
    let r = Repo::new();
    r.land(&[("a", "1\n")], "Seed");
    let culprit = r.land(&[("a", "2\n")], "Change");
    let out = run(&r.dir, &["revert", &culprit, "--no-land"], 0);
    assert_eq!(out["land"], Value::Null);
    assert_eq!(r.head().unwrap(), culprit);
    let revert = id(&out["revert"]);
    let shown = run(&r.dir, &["show", &revert], 0);
    assert_eq!(shown["parents"][0], culprit.as_str());
}

#[test]
fn what_cannot_be_reverted_is_refused() {
    let r = Repo::new();
    let seed = r.land(&[("a", "1\n")], "Seed");
    let out = braid(&r.dir, &["revert", &seed]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no parent"));
    // A changeset that is not on the branch.
    let off = run(
        &r.dir,
        &[
            "change", "create", "--base", &seed, "-m", "Off", "--delete", "a",
        ],
        0,
    );
    let out = braid(&r.dir, &["revert", &id(&off["change"])]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("first-parent chain"), "{err}");
    assert_eq!(r.head().unwrap(), seed);
    // Only the first parent of a merge is a mainline.
    let out = braid(&r.dir, &["revert", &seed, "--mainline", "2"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn a_revert_lands_only_on_a_branch() {
    let r = Repo::new();
    r.land(&[("a", "1\n")], "Seed");
    let culprit = r.land(&[("a", "2\n")], "Change");
    // A ref of braid's own, as verify's green ref is, is no branch.
    run(&r.dir, &["ref", "set", "braid/green/main", &culprit], 0);
    let out = braid(
        &r.dir,
        &["revert", &culprit, "--branch", "braid/green/main"],
    );
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("braid's own namespaces"), "{err}");
    let refs = run(&r.dir, &["refs"], 0);
    assert_eq!(refs["refs"]["braid/green/main"], culprit.as_str());
    assert_eq!(r.head().unwrap(), culprit);
}

/// Git without the user's or the system's configuration.
const GIT_ENV: [(&str, &str); 6] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Ada Lovelace"),
    ("GIT_AUTHOR_EMAIL", "ada@example.org"),
    ("GIT_COMMITTER_NAME", "Ada Lovelace"),
    ("GIT_COMMITTER_EMAIL", "ada@example.org"),
];

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .envs(GIT_ENV)
        .output()
        .expect("git must be installed to run this test");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_merge_is_reverted_against_its_first_parent_with_mainline_1() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let project = base.join("project");
    std::fs::create_dir(&project).unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    std::fs::write(project.join("a"), "a\n").unwrap();
    git(&project, &["add", "a"]);
    git(&project, &["commit", "-q", "-m", "Seed"]);
    git(&project, &["checkout", "-q", "-b", "side"]);
    std::fs::write(project.join("side"), "s\n").unwrap();
    git(&project, &["add", "side"]);
    git(&project, &["commit", "-q", "-m", "Side"]);
    git(&project, &["checkout", "-q", "main"]);
    std::fs::write(project.join("main"), "m\n").unwrap();
    git(&project, &["add", "main"]);
    git(&project, &["commit", "-q", "-m", "Main"]);
    git(
        &project,
        &["merge", "-q", "--no-ff", "-m", "Merge side", "side"],
    );

    let dir = base.join("repo");
    run(&base, &["init", dir.to_str().unwrap()], 0);
    run(&dir, &["git", "import", project.to_str().unwrap()], 0);
    let merge = id(&run(&dir, &["show", "main"], 0)["id"]);
    let parent = id(&run(&dir, &["show", "main"], 0)["parents"][0]);

    // Refused without --mainline, and --mainline on a changeset that is not
    // a merge.
    let out = braid(&dir, &["revert", &merge]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--mainline 1"));
    let out = braid(&dir, &["revert", &parent, "--mainline", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not a merge"));

    let out = run(&dir, &["revert", &merge, "--mainline", "1"], 0);
    assert_eq!(out["land"]["outcome"], "fast-forward");
    let back = run(&dir, &["diff", &parent, "main"], 0);
    assert_eq!(back["changes"], serde_json::json!([]));
    let gone = braid(&dir, &["cat-object", "main:side"]);
    assert!(!gone.status.success());
}

#[cfg(unix)]
#[test]
fn a_revert_puts_back_modes_and_removes_what_was_added() {
    use std::os::unix::fs::PermissionsExt;
    let r = Repo::new();
    r.land(&[("run.sh", "echo hi\n"), ("gone", "g\n")], "Seed");
    run(&r.dir, &["workspace", "add", "w"], 0);
    let ws = r.dir.join(".braid/work/w");
    std::fs::set_permissions(ws.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_file(ws.join("gone")).unwrap();
    std::fs::write(ws.join("added"), "a\n").unwrap();
    run(&ws, &["commit", "-m", "Modes"], 0);
    let landed = run(&ws, &["land"], 0);
    let culprit = id(&landed["head"]);
    let shown = run(&r.dir, &["show", "main"], 0);
    assert_eq!(shown["changes"].as_array().unwrap().len(), 3);

    run(&r.dir, &["revert", &culprit], 0);
    let back = run(&r.dir, &["diff", &culprit, "main"], 0);
    let ops: Vec<(String, String)> = back["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (id(&c["path"]), id(&c["op"])))
        .collect();
    assert_eq!(
        ops,
        [
            ("added".to_owned(), "delete".to_owned()),
            ("gone".to_owned(), "file".to_owned()),
            ("run.sh".to_owned(), "file".to_owned()),
        ]
    );
}

#[test]
fn a_revert_the_branch_has_already_says_nothing_landed() {
    let r = Repo::new();
    r.land(&[("a", "1\n"), ("b", "b\n")], "Seed");
    let culprit = r.land(&[("a", "2\n")], "Change");
    let undone = r.land(&[("a", "1\n")], "Undo by hand");
    let out = run(&r.dir, &["revert", &culprit], 0);
    assert_eq!(out["land"]["outcome"], "already-applied");
    let text = braid(&r.dir, &["revert", &culprit]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("nothing landed"), "{text}");
    assert!(!text.contains("Reverted"), "{text}");
    assert_eq!(r.head().unwrap(), undone);
}
