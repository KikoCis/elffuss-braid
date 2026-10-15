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
