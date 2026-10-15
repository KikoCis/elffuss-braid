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
