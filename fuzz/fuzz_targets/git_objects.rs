// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The parsers of raw git commits, tags and trees never panic on any input.
//!
//! - A commit's message is the end of its bytes, and a commit generated from
//!   what was read, when it can be written at all, reads back the same.
//! - A tree's entries, written back as they were read, give exactly its bytes.
//!
//! Each input is also tried behind a valid first header, and as the author
//! and committer lines of a commit, so the fuzzer spends its time on what
//! follows rather than on finding the header names.

#![no_main]

use braid_git::fuzzing::{
    EntryType, encode_commit, parse_commit, parse_signature, parse_tag, parse_tree,
};
use libfuzzer_sys::fuzz_target;

const TREE_HEADER: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n";
const OBJECT_HEADER: &[u8] = b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n";

fn commit(data: &[u8]) {
    let Ok(fields) = parse_commit(data) else {
        return;
    };
    assert!(data.ends_with(fields.message));
    let author = fields.author.map(parse_signature);
    let committer = fields.committer.map(parse_signature);
    if let (Some(author), Some(committer)) = (author, committer)
        && let Ok(bytes) = encode_commit(
            fields.tree,
            &fields.parents,
            &author,
            &committer,
            &[],
            fields.message,
        )
    {
        let again = parse_commit(&bytes).expect("a generated commit reads back");
        assert_eq!(again.tree, fields.tree);
        assert_eq!(again.parents, fields.parents);
        assert_eq!(again.author.map(parse_signature), Some(author));
        assert_eq!(again.committer.map(parse_signature), Some(committer));
        assert_eq!(again.message, fields.message);
    }
}

fn tree(data: &[u8]) {
    let Ok(entries) = parse_tree(data) else {
        return;
    };
    let mut written = Vec::with_capacity(data.len());
    for entry in &entries {
        let _ = EntryType::from_mode(entry.mode);
        written.extend_from_slice(entry.mode);
        written.push(b' ');
        written.extend_from_slice(entry.name);
        written.push(0);
        written.extend_from_slice(entry.oid.as_bytes());
    }
    assert_eq!(written, data);
}

fn framed(header: &[u8], data: &[u8]) -> Vec<u8> {
    let mut bytes = header.to_vec();
    bytes.extend_from_slice(data);
    bytes
}

/// A commit whose author and committer values are the first two lines of
/// `data`, followed by the rest of it.
fn with_signatures(data: &[u8]) -> Vec<u8> {
    let mut lines = data.splitn(3, |byte| *byte == b'\n');
    let author = lines.next().unwrap_or_default();
    let committer = lines.next().unwrap_or_default();
    let rest = lines.next().unwrap_or_default();
    let mut bytes = TREE_HEADER.to_vec();
    bytes.extend_from_slice(b"parent 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n");
    bytes.extend_from_slice(b"author ");
    bytes.extend_from_slice(author);
    bytes.extend_from_slice(b"\ncommitter ");
    bytes.extend_from_slice(committer);
    bytes.push(b'\n');
    bytes.extend_from_slice(rest);
    bytes
}

fuzz_target!(|data: &[u8]| {
    commit(data);
    commit(&framed(TREE_HEADER, data));
    commit(&with_signatures(data));
    let _ = parse_tag(data);
    assert!(parse_tag(&framed(OBJECT_HEADER, data)).is_ok());
    tree(data);
});
