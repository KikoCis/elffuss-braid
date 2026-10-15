// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! End-to-end tests of the `braid` binary.

use std::io::Write;
use std::process::{Command, Stdio};

use braid_format::{Kind, ObjectId};

fn braid() -> Command {
    Command::new(env!("CARGO_BIN_EXE_braid"))
}

#[test]
fn version_prints_name_and_version() {
    let out = braid().arg("--version").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.trim(), format!("braid {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn hash_object_matches_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hello.txt");
    std::fs::write(&path, b"hello\n").unwrap();

    let out = braid().arg("hash-object").arg(&path).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let expected = ObjectId::hash(Kind::Blob, b"hello\n");
    assert_eq!(text, format!("{expected}  {}\n", path.display()));
}

#[test]
fn hash_object_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.bin");
    let payload: Vec<u8> = (0..1_000_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(&path, &payload).unwrap();

    let out = braid()
        .args(["hash-object", "--json"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let item = &value[0];
    assert_eq!(item["kind"], "blob");
    assert_eq!(item["size"], payload.len() as u64);
    assert_eq!(
        item["id"],
        ObjectId::hash(Kind::Blob, &payload).to_hex().as_str()
    );
}

#[test]
fn hash_object_reads_standard_input() {
    let mut child = braid()
        .args(["hash-object", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let expected = ObjectId::hash(Kind::Blob, b"hello\n");
    assert_eq!(text, format!("{expected}  -\n"));
}

#[test]
fn missing_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = braid()
        .arg("hash-object")
        .arg(dir.path().join("does-not-exist"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.starts_with("braid: error: cannot open"), "{err}");
}

fn braid_in(dir: &std::path::Path) -> Command {
    let mut command = braid();
    command.arg("--repo").arg(dir).env("BRAID_AGENT", "tester");
    command
}

fn run_json(command: &mut Command) -> serde_json::Value {
    let out = command.arg("--json").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn repository_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let init = braid().arg("init").arg(dir.path()).output().unwrap();
    assert!(init.status.success());
    // A second init in the same place is refused.
    assert!(
        !braid()
            .arg("init")
            .arg(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    let file = dir.path().join("note.txt");
    std::fs::write(&file, b"hello\n").unwrap();
    let written = run_json(braid_in(dir.path()).args(["hash-object", "-w"]).arg(&file));
    let id = written[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(id, ObjectId::hash(Kind::Blob, b"hello\n").to_hex());

    let cat = braid_in(dir.path())
        .args(["cat-object", &id])
        .output()
        .unwrap();
    assert_eq!(cat.stdout, b"hello\n");

    // Refs: create with a compare-and-swap, refuse a stale expectation.
    let set = run_json(braid_in(dir.path()).args(["ref", "set", "main", &id, "--expect", "none"]));
    assert_eq!(set["agent"], "tester");
    assert_eq!(set["updates"][0]["new"], id.as_str());
    let stale = braid_in(dir.path())
        .args(["ref", "set", "main", &id, "--expect", "none"])
        .output()
        .unwrap();
    assert!(!stale.status.success());

    let refs = run_json(braid_in(dir.path()).arg("refs"));
    assert_eq!(refs["refs"]["main"], id.as_str());
    let by_name = braid_in(dir.path())
        .args(["cat-object", "main"])
        .output()
        .unwrap();
    assert_eq!(by_name.stdout, b"hello\n");

    // The op log shows it, undo reverts it, and the store stays clean.
    let log = run_json(braid_in(dir.path()).args(["op", "log"]));
    assert_eq!(log.as_array().unwrap().len(), 1);
    let undone = run_json(braid_in(dir.path()).arg("undo"));
    assert_eq!(undone["undoes"], 1);
    let refs = run_json(braid_in(dir.path()).arg("refs"));
    assert!(refs["refs"].as_object().unwrap().is_empty());
    let nothing = braid_in(dir.path()).arg("undo").output().unwrap();
    assert!(!nothing.status.success());

    let fsck = run_json(braid_in(dir.path()).arg("fsck"));
    assert_eq!(fsck["clean"], true);
    assert_eq!(fsck["objects"], 1);
}

/// Two blob contents whose ids start with the same six hex digits or more.
fn blob_twins() -> (Vec<u8>, Vec<u8>) {
    let mut seen = std::collections::HashMap::new();
    for n in 0u64.. {
        let content = format!("twin {n}\n").into_bytes();
        let id = ObjectId::hash(Kind::Blob, &content);
        let start = id.to_hex()[..6].to_owned();
        if let Some(other) = seen.insert(start, content.clone()) {
            return (other, content);
        }
    }
    unreachable!("the search ends")
}

#[test]
fn short_ids_and_an_ambiguous_one() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        braid()
            .arg("init")
            .arg(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    let (one, two) = blob_twins();
    let files = [dir.path().join("one"), dir.path().join("two")];
    std::fs::write(&files[0], &one).unwrap();
    std::fs::write(&files[1], &two).unwrap();
    let written = run_json(
        braid_in(dir.path())
            .args(["hash-object", "-w"])
            .args(&files),
    );
    let ids: Vec<String> = (0..2)
        .map(|i| written[i]["id"].as_str().unwrap().to_owned())
        .collect();
    let shared = ids[0]
        .bytes()
        .zip(ids[1].bytes())
        .take_while(|(a, b)| a == b)
        .count();
    assert!(shared >= 6);

    // Six digits name both: an error that lists them, and nothing printed.
    let out = braid_in(dir.path())
        .args(["cat-object", &ids[0][..6]])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("ambiguous"), "{err}");
    for id in &ids {
        assert!(err.contains(&id[..12]), "{err}");
    }

    // One digit more names one of them, wherever a revision is taken.
    let unique: Vec<&str> = ids.iter().map(|id| &id[..shared + 1]).collect();
    let cat = braid_in(dir.path())
        .args(["cat-object", unique[1]])
        .output()
        .unwrap();
    assert_eq!(cat.stdout, two);
    let cat = run_json(braid_in(dir.path()).args(["cat-object", unique[0]]));
    assert_eq!(cat["id"], ids[0].as_str());
    let set = run_json(braid_in(dir.path()).args(["ref", "set", "pinned", unique[0]]));
    assert_eq!(set["updates"][0]["new"], ids[0].as_str());
    // A ref spelled like a short id wins over it.
    run_json(braid_in(dir.path()).args(["ref", "set", &ids[1][..6], unique[1]]));
    let cat = braid_in(dir.path())
        .args(["cat-object", &ids[1][..6]])
        .output()
        .unwrap();
    assert_eq!(cat.stdout, two);
    // A short id is no changeset: show says what it is.
    let out = braid_in(dir.path())
        .args(["show", unique[0]])
        .output()
        .unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("is a blob, not a changeset"), "{err}");
}

#[test]
fn commands_outside_a_repository_fail_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let out = braid()
        .arg("--repo")
        .arg(dir.path())
        .arg("refs")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("not inside a braid repository"), "{err}");
}

/// Runs git with a fixed identity and dates, ignoring the user's config.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.org")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.org")
        .env("GIT_AUTHOR_DATE", "1700000000 +0100")
        .env("GIT_COMMITTER_DATE", "1700000000 +0100")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn git_import_log_show_export() {
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("README.md"), "hello\n").unwrap();
    git(&src, &["add", "README.md"]);
    git(&src, &["commit", "-q", "-m", "First"]);
    std::fs::create_dir(src.join("src")).unwrap();
    std::fs::write(src.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&src, &["add", "src/lib.rs"]);
    git(&src, &["commit", "-q", "-m", "Second\n\nWith a body."]);
    git(&src, &["tag", "-a", "v1", "-m", "Version 1"]);

    let repo = work.path().join("braid");
    assert!(
        braid()
            .arg("init")
            .arg(&repo)
            .output()
            .unwrap()
            .status
            .success()
    );
    let import = run_json(braid_in(&repo).args(["git", "import"]).arg(&src));
    assert_eq!(import["commits"], 2);
    assert_eq!(import["tags"], 1);
    // Importing again finds nothing new and moves no ref.
    let again = run_json(braid_in(&repo).args(["git", "import"]).arg(&src));
    assert_eq!(again["commits"], 0);
    assert!(again["operation"].is_null());

    let log = run_json(braid_in(&repo).args(["log", "main"]));
    let log = log.as_array().unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0]["message"], "Second\n\nWith a body.\n");
    assert_eq!(log[0]["author"]["email"], "ada@example.org");
    assert_eq!(log[0]["author"]["offset_minutes"], 60);

    let show = run_json(braid_in(&repo).args(["show", "main"]));
    assert_eq!(show["changes"][0]["path"], "src/lib.rs");
    assert_eq!(show["changes"][0]["op"], "file");
    assert!(show["git_origin"].is_string());

    // Export into an empty repository: the same commit and tag ids.
    let mirror = work.path().join("mirror.git");
    std::fs::create_dir(&mirror).unwrap();
    git(&mirror, &["init", "-q", "--bare"]);
    run_json(braid_in(&repo).args(["git", "export"]).arg(&mirror));
    for name in ["refs/heads/main", "refs/tags/v1"] {
        assert_eq!(
            git(&src, &["rev-parse", name]),
            git(&mirror, &["rev-parse", name]),
            "{name}"
        );
    }

    // The import is one operation: undo takes every ref back.
    run_json(braid_in(&repo).arg("undo"));
    let refs = run_json(braid_in(&repo).arg("refs"));
    assert!(refs["refs"].as_object().unwrap().is_empty());
    assert_eq!(run_json(braid_in(&repo).arg("fsck"))["clean"], true);
}
