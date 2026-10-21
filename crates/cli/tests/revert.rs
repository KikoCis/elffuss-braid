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
