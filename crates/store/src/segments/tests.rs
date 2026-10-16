// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! What a handle reads of the indexes, and merges cut short.

use std::fs;
use std::path::Path;

use braid_format::{Kind, ObjectId, Objects};

use super::merge::failpoints;
use super::sources::SourceKind;
use super::*;
use crate::{STORAGE_VERSION, Store};

fn options(segment_records: u64, merge_threshold: usize) -> Options {
    Options {
        segment_records,
        merge_threshold,
        ..Options::default()
    }
}

fn payload(n: u32) -> Vec<u8> {
    format!("object {n}, a little longer than an id\n")
        .repeat(1 + (n % 5) as usize)
        .into_bytes()
}

/// Writes `n` objects, syncing now and then, and returns their ids.
fn fill(store: &Store, n: u32) -> Vec<ObjectId> {
    let ids = (0..n)
        .map(|i| {
            let id = store.put(Kind::Blob, &payload(i)).unwrap();
            if i % 97 == 96 {
                store.sync().unwrap();
            }
            id
        })
        .collect();
    store.sync().unwrap();
    ids
}

fn files_with(dir: &Path, extension: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir.join("seg"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(&format!(".{extension}")))
        .collect();
    names.sort();
    names
}

fn active(segments: &Segments) -> Vec<Arc<Source>> {
    segments
        .read_catalog()
        .active
        .iter()
        .map(|active| Arc::clone(&active.source))
        .collect()
}

#[test]
fn opening_reads_no_index_and_a_lookup_one_bucket_of_each() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let store = Store::init(&root, options(50, 6)).unwrap();
    let ids = fill(&store, 1500);
    drop(store);
    assert!(!files_with(&root, "midx").is_empty());
    assert!(!files_with(&root, "idx").is_empty());

    let segments = Segments::open(&root, &options(50, 6), STORAGE_VERSION).unwrap();
    let sources = active(&segments);
    assert!(sources.len() >= 2, "{sources:?}");
    for source in &sources {
        // The head of a merged index says what it covers; nothing else is
        // read until a lookup needs it.
        let expected = (source.kind == SourceKind::Merged, 0);
        assert_eq!(source.read_so_far(), expected, "{}", source.file);
    }
    // The open segment's objects come from its journal.
    assert!(segments.read_catalog().open.len() < 50);

    let id = ids[700];
    assert!(segments.locate(&id, ANY).unwrap().is_some());
    let mut buckets = 0;
    for source in &sources {
        let (_, read) = source.read_so_far();
        assert!(read <= 1, "{}: {read} buckets", source.file);
        buckets += read;
    }
    assert!(buckets >= 1 && buckets <= sources.len());
    // A miss goes through every index, still a bucket each.
    let absent = ObjectId::hash(Kind::Blob, b"not in the store");
    assert_eq!(
        segments.locate(&absent, ANY).unwrap().map(|l| l.entry),
        None
    );
    for source in &sources {
        assert!(source.read_so_far().1 <= 2, "{}", source.file);
    }
    for id in &ids {
        assert!(segments.locate(id, ANY).unwrap().is_some(), "{id}");
    }
}

#[test]
fn merges_take_the_indexes_that_pile_up_and_keep_every_object() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let store = Store::init(&root, options(20, 4)).unwrap();
    let ids = fill(&store, 2000);
    let indexes = files_with(&root, "idx");
    let merged = files_with(&root, "midx");
    assert!(indexes.len() < 4, "{indexes:?}");
    // About a hundred segments: few merged indexes, each larger than twice
    // the next.
    assert!(!merged.is_empty() && merged.len() <= 6, "{merged:?}");
    let mut sizes: Vec<u64> = merged
        .iter()
        .map(|name| fs::metadata(root.join("seg").join(name)).unwrap().len())
        .collect();
    sizes.sort_unstable();
    for pair in sizes.windows(2) {
        assert!(pair[1] > pair[0], "{sizes:?}");
    }
    drop(store);

    let store = Store::open(&root, options(20, 4)).unwrap();
    for (n, id) in (0u32..).zip(&ids) {
        assert_eq!(store.get(id).unwrap(), Some((Kind::Blob, payload(n))));
    }
    let report = store.fsck().unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(report.objects, 2000);
    assert_eq!(report.unindexed_records, 0);
    assert_eq!(store.stats().unwrap().objects, 2000);
    // Short ids are found in every kind of index.
    for id in ids.iter().step_by(37) {
        let hex = id.to_hex();
        assert_eq!(store.ids_with_prefix(&hex[..10], 10).unwrap(), [*id]);
    }
    let mut all = ids.clone();
    all.sort();
    assert_eq!(store.ids_with_prefix("", usize::MAX).unwrap(), all);
    // Everything into one.
    assert!(store.merge_indexes().unwrap());
    assert_eq!(files_with(&root, "midx").len(), 1);
    assert!(files_with(&root, "idx").is_empty());
    assert!(!store.merge_indexes().unwrap());
    assert!(store.fsck().unwrap().is_clean());
}

#[test]
fn a_merge_cut_short_reads_the_same_and_the_next_cleans_up() {
    for point in ["published", "journals removed"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let store = Store::init(&root, options(10, 0)).unwrap();
        let ids = fill(&store, 300);
        let indexes = files_with(&root, "idx");
        assert!(indexes.len() > 10, "{indexes:?}");
        // A sealer that died before removing its journal left this one.
        let stale = indexes[0].replace(".idx", ".jrn");
        fs::write(root.join("seg").join(&stale), b"").unwrap();

        failpoints::arm(Some(point));
        let cut = store.merge_indexes();
        failpoints::arm(None);
        assert!(cut.is_err(), "{point}");
        assert_eq!(files_with(&root, "midx").len(), 1, "{point}");
        assert_eq!(files_with(&root, "idx"), indexes, "{point}");
        let journal_left = root.join("seg").join(&stale).exists();
        assert_eq!(journal_left, point == "published", "{point}");

        // Readers see the same objects, through either index.
        let fresh = Store::open(&root, options(10, 0)).unwrap();
        for (n, id) in (0u32..).zip(&ids) {
            assert_eq!(fresh.get(id).unwrap(), Some((Kind::Blob, payload(n))));
        }
        let report = fresh.fsck().unwrap();
        assert!(report.is_clean(), "{point}: {:#?}", report.problems);
        assert_eq!(report.objects, 300);
        // A writer never takes a merged segment for an open one.
        let sizes = |names: &[String]| -> Vec<u64> {
            names
                .iter()
                .map(|name| {
                    let seg = root.join("seg").join(name.replace(".idx", ".seg"));
                    fs::metadata(seg).unwrap().len()
                })
                .collect()
        };
        let before = sizes(&indexes);
        let more = fresh.put(Kind::Blob, b"written after the cut").unwrap();
        fresh.sync().unwrap();
        assert_eq!(sizes(&indexes), before, "{point}");

        // The next merge removes what this one left: there is nothing left
        // to merge.
        assert!(!fresh.merge_indexes().unwrap(), "{point}");
        assert!(files_with(&root, "idx").is_empty(), "{point}");
        assert!(!root.join("seg").join(&stale).exists(), "{point}");
        assert_eq!(files_with(&root, "midx").len(), 1, "{point}");
        let reopened = Store::open(&root, options(10, 0)).unwrap();
        assert!(reopened.contains(&more).unwrap());
        for id in &ids {
            assert!(reopened.contains(id).unwrap());
        }
        let report = reopened.fsck().unwrap();
        assert!(report.is_clean(), "{point}: {:#?}", report.problems);
    }
}

#[test]
fn a_handle_that_has_not_read_a_merged_away_index_finds_its_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let writer = Store::init(&root, options(10, 0)).unwrap();
    let ids = fill(&writer, 200);
    // Registered, not read.
    let reader = Store::open(&root, options(10, 0)).unwrap();
    assert!(writer.merge_indexes().unwrap());
    assert!(files_with(&root, "idx").is_empty());
    for (n, id) in (0u32..).zip(&ids) {
        assert_eq!(reader.get(id).unwrap(), Some((Kind::Blob, payload(n))));
    }
    let hex = ids[3].to_hex();
    assert_eq!(reader.ids_with_prefix(&hex[..12], 5).unwrap(), [ids[3]]);
}
