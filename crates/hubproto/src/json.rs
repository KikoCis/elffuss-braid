// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Changes and conflicts as `braid land --json` prints them, so that a
//! landing on the hub reports exactly what a local landing does. The CLI's
//! own printer and these must agree.

use braid_engine::{Conflict, ConflictKind, HunkSide};
use braid_format::{Change, ChangeOp, EntryKind};
use serde_json::{Value, json};

fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

/// A change: `{"path", "op", …}` with `op` one of `delete`, `file`,
/// `executable`, `symlink` (with `blob` and `size`) or `gitlink` (with
/// `commit`).
pub fn change(change: &Change) -> Value {
    let path = show(&change.path);
    match &change.op {
        ChangeOp::Delete => json!({ "path": path, "op": "delete" }),
        ChangeOp::File { blob, size } => {
            json!({ "path": path, "op": "file", "blob": blob.to_hex(), "size": size })
        }
        ChangeOp::Executable { blob, size } => {
            json!({ "path": path, "op": "executable", "blob": blob.to_hex(), "size": size })
        }
        ChangeOp::Symlink { blob, size } => {
            json!({ "path": path, "op": "symlink", "blob": blob.to_hex(), "size": size })
        }
        ChangeOp::Gitlink { commit } => json!({
            "path": path,
            "op": "gitlink",
            "commit": commit.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>(),
        }),
    }
}

/// One side of a conflict: `null`, `{"kind": "tree", "tree"}`, or the entry
/// as a change without its path, `op` renamed `kind`.
pub fn entry(entry: &Option<EntryKind>) -> Value {
    match entry {
        None => Value::Null,
        Some(EntryKind::Tree { tree }) => json!({ "kind": "tree", "tree": tree.to_hex() }),
        Some(kind) => match ChangeOp::from_entry(kind) {
            Some(op) => {
                let mut value = change(&Change {
                    path: Vec::new(),
                    op,
                });
                if let Some(object) = value.as_object_mut() {
                    object.remove("path");
                    if let Some(op) = object.remove("op") {
                        object.insert("kind".to_owned(), op);
                    }
                }
                value
            }
            None => Value::Null,
        },
    }
}

fn side(side: &HunkSide) -> Value {
    json!({
        "start": side.lines.start,
        "end": side.lines.end,
        "text": String::from_utf8_lossy(&side.text),
    })
}

/// A conflict as data, its sides named from the lander's point of view:
/// `base` (the common ancestor), `branch` (what the branch has now) and
/// `workspace` (the change being landed), with the hunks of a content
/// conflict.
pub fn conflict(conflict: &Conflict) -> Value {
    let mut value = json!({
        "path": show(&conflict.path),
        "kind": conflict.kind.code(),
        "base": entry(&conflict.base),
        "branch": entry(&conflict.ours),
        "workspace": entry(&conflict.theirs),
        "hunks": [],
    });
    match &conflict.kind {
        ConflictKind::Content { hunks } => {
            value["hunks"] = hunks
                .iter()
                .map(|hunk| {
                    json!({
                        "base": side(&hunk.base),
                        "branch": side(&hunk.ours),
                        "workspace": side(&hunk.theirs),
                    })
                })
                .collect();
        }
        ConflictKind::DirectoryRemoved { directory } => {
            value["directory"] = Value::String(show(directory));
        }
        ConflictKind::Case { existing } => {
            value["existing"] = Value::String(show(existing));
        }
        _ => {}
    }
    value
}
