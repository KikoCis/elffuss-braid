// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Indexes through the public API: sealed segments looked up in their
//! indexes, indexes merged as they pile up, and damage to merged indexes.

use std::fs;
use std::path::{Path, PathBuf};

use braid_store::{Kind, ObjectId, Objects, Options, Problem, Store};

fn options() -> Options {
    Options {
        segment_records: 10,
        merge_threshold: 4,
        ..Options::default()
    }
}

fn payload(n: u32) -> Vec<u8> {
    format!("object {n} of the index tests\n")
        .repeat(1 + (n % 4) as usize)
        .into_bytes()
}

/// A store of `n` objects, its root, and their ids.
fn store_of(n: u32) -> (tempfile::TempDir, PathBuf, Vec<ObjectId>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let unmerged = Options {
        merge_threshold: 0,
        ..options()
    };
    let store = Store::init(&root, unmerged).unwrap();
    let ids: Vec<ObjectId> = (0..n)
        .map(|i| store.put(Kind::Blob, &payload(i)).unwrap())
        .collect();
    store.sync().unwrap();
    assert!(store.merge_indexes().unwrap());
    (dir, root, ids)
}

fn files_with(root: &Path, extension: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(root.join("seg"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == extension))
        .collect();
    files.sort();
    files
}

fn flip_byte(path: &Path, offset: usize) {
    let mut bytes = fs::read(path).unwrap();
    bytes[offset] ^= 0x20;
    fs::write(path, bytes).unwrap();
}

fn assert_all_readable(store: &Store, ids: &[ObjectId]) {
    for (n, id) in (0u32..).zip(ids) {
        assert_eq!(
            store.get(id).unwrap(),
            Some((Kind::Blob, payload(n))),
            "{n}"
        );
        assert_eq!(
            store.stat(id).unwrap(),
            Some((Kind::Blob, payload(n).len() as u64))
        );
    }
}

#[test]
fn sealed_segments_are_found_through_one_merged_index() {
    let (_dir, root, ids) = store_of(250);
    assert_eq!(files_with(&root, "midx").len(), 1);
    assert!(files_with(&root, "idx").is_empty());
    // Merged segments have neither a journal nor an index of their own.
    let journals = files_with(&root, "jrn");
    assert!(journals.len() <= 1, "{journals:?}");
    let store = Store::open(&root, options()).unwrap();
    assert_all_readable(&store, &ids);
    let report = store.fsck().unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(report.objects, 250);
    assert!(report.sealed_segments >= 24, "{report:?}");
    let stats = store.stats().unwrap();
    assert_eq!(stats.objects, 250);
    assert!(stats.index_bytes >= 64 * stats.objects, "{stats:?}");
}

#[test]
fn a_damaged_merged_index_is_worked_around_and_reported() {
    let (_dir, root, ids) = store_of(250);
    let merged = files_with(&root, "midx").remove(0);
    // An entry in the middle of the index.
    let len = fs::metadata(&merged).unwrap().len() as usize;
    flip_byte(&merged, len - 64 * 100 + 10);

    let store = Store::open(&root, options()).unwrap();
    assert_all_readable(&store, &ids);
    let report = store.fsck().unwrap();
    assert!(
        matches!(report.problems.as_slice(), [Problem::BadMergedIndex { .. }]),
        "{:#?}",
        report.problems
    );
    assert_eq!(report.objects, 250);
    // Writing on works around it too.
    let more = store.put(Kind::Blob, b"written past the damage").unwrap();
    store.sync().unwrap();
    assert!(
        Store::open(&root, options())
            .unwrap()
            .contains(&more)
            .unwrap()
    );
}

#[test]
fn a_merged_index_whose_head_is_damaged_leaves_its_segments_readable() {
    let (_dir, root, ids) = store_of(120);
    let merged = files_with(&root, "midx").remove(0);
    // The segment table: no reader can tell which segments it covers.
    flip_byte(&merged, 40);

    let store = Store::open(&root, options()).unwrap();
    assert_all_readable(&store, &ids);
    let report = store.fsck().unwrap();
    assert!(!report.is_clean());
    assert!(
        report.problems.iter().all(|problem| matches!(
            problem,
            Problem::BadMergedIndex { .. } | Problem::BadIndex { .. }
        )),
        "{:#?}",
        report.problems
    );
    assert_eq!(report.objects, 120);
}
