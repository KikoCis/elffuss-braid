// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! How objects are stored, through the public API: deltas against similar
//! objects, large blobs in chunks, streaming reads and writes, damage to
//! what other objects depend on, and the sizes stores take.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use braid_store::{CHUNKED_MIN, Codec, Error, Kind, ObjectId, Objects, Options, Problem, Store};

fn new_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path().join("store"), Options::default()).unwrap();
    (dir, store)
}

fn reopen(store: Store) -> Store {
    store.sync().unwrap();
    let root = store.root().to_path_buf();
    drop(store);
    Store::open(root, Options::default()).unwrap()
}

/// Bytes that do not compress, as model weights do not.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Source-like text: lines that compress only a little on their own.
fn text(seed: u64, lines: usize) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::new();
    for i in 0..lines {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(format!("fn item_{i}() -> u64 {{ 0x{state:016x} }}\n").as_bytes());
    }
    out
}

fn edited(payload: &[u8], at: usize, with: &[u8]) -> Vec<u8> {
    let mut out = payload.to_vec();
    out[at..at + with.len()].copy_from_slice(with);
    out
}

fn codec_count(store: &Store, codec: Codec) -> u64 {
    store
        .stats()
        .unwrap()
        .codecs
        .iter()
        .find(|c| c.codec == codec)
        .map_or(0, |c| c.objects)
}

fn segment_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(root.join("seg"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "seg"))
        .collect();
    files.sort();
    files
}

/// Where `needle` starts in the segment files, the first time.
fn find_in_segments(root: &Path, needle: &[u8]) -> (PathBuf, usize) {
    for path in segment_files(root) {
        let bytes = fs::read(&path).unwrap();
        if let Some(at) = bytes.windows(needle.len()).position(|w| w == needle) {
            return (path, at);
        }
    }
    panic!("not found in any segment");
}

fn flip_byte(path: &Path, offset: usize) {
    let mut bytes = fs::read(path).unwrap();
    bytes[offset] ^= 0x20;
    fs::write(path, bytes).unwrap();
}

/// Total bytes of every file in a store.
fn store_size(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let meta = entry.metadata().unwrap();
        total += if meta.is_dir() {
            store_size(&entry.path())
        } else {
            meta.len()
        };
    }
    total
}

#[test]
fn versions_of_a_file_are_stored_as_deltas() {
    let (_dir, store) = new_store();
    let mut versions = vec![text(1, 3000)];
    for n in 1..8usize {
        let previous = versions.last().unwrap();
        let mut next = edited(previous, n * 9000, format!("edit {n}").as_bytes());
        next.extend_from_slice(format!("fn added_{n}() {{}}\n").as_bytes());
        versions.push(next);
    }
    let mut ids = vec![store.put(Kind::Blob, &versions[0]).unwrap()];
    for version in &versions[1..] {
        let previous = *ids.last().unwrap();
        ids.push(store.put_similar(Kind::Blob, version, &previous).unwrap());
    }
    let alone: u64 = versions
        .iter()
        .map(|v| zstd::bulk::compress(v, 3).unwrap().len() as u64)
        .sum();
    let stats = store.stats().unwrap();
    assert_eq!(codec_count(&store, Codec::Delta), 7);
    // Every version on its own would take several times the room.
    assert!(stats.stored_bytes * 4 < alone, "{stats:?}");

    let store = reopen(store);
    for (id, version) in ids.iter().zip(&versions) {
        assert_eq!(*id, ObjectId::hash(Kind::Blob, version));
        assert_eq!(store.get(id).unwrap(), Some((Kind::Blob, version.clone())));
        assert_eq!(
            store.stat(id).unwrap(),
            Some((Kind::Blob, version.len() as u64))
        );
    }
    let report = store.fsck().unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(report.delta_records, 7);
    assert_eq!(report.objects, 8);
}

#[test]
fn a_damaged_base_fails_its_deltas_cleanly() {
    let (_dir, store) = new_store();
    let base = text(2, 2000);
    let near = edited(&base, 5000, b"a small change");
    let base_id = store.put(Kind::Tree, &base).unwrap();
    let delta_id = store.put_similar(Kind::Tree, &near, &base_id).unwrap();
    assert_eq!(codec_count(&store, Codec::Delta), 1);
    let root = store.root().to_path_buf();
    let store = reopen(store);
    assert!(store.fsck().unwrap().is_clean());
    drop(store);

    // The base is stored compressed: damage the middle of its record.
    let (path, at) = find_in_segments(&root, base_id.as_bytes());
    flip_byte(&path, at + 200);
    let store = Store::open(&root, Options::default()).unwrap();
    assert!(matches!(store.get(&base_id), Err(Error::Corrupt { .. })));
    let err = store.get(&delta_id).unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }), "{err}");
    assert!(err.to_string().contains("base"), "{err}");
    // The header is intact, so the size is still known.
    assert_eq!(
        store.stat(&delta_id).unwrap(),
        Some((Kind::Tree, near.len() as u64))
    );

    let report = store.fsck().unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| matches!(p, Problem::BadEntry { id, .. } if *id == base_id)),
        "{:#?}",
        report.problems
    );
    assert!(
        report.problems.iter().any(|p| matches!(
            p,
            Problem::BadRecord { detail, .. } if detail.contains(&delta_id.to_hex())
        )),
        "{:#?}",
        report.problems
    );
    assert_eq!(report.objects, 0);
}

#[test]
fn a_missing_base_fails_its_delta_cleanly() {
    let (_dir, store) = new_store();
    let base = text(3, 1500);
    let base_id = store.put(Kind::Blob, &base).unwrap();
    let near = edited(&base, 100, b"x");
    let delta_id = store.put_similar(Kind::Blob, &near, &base_id).unwrap();
    store.sync().unwrap();
    let root = store.root().to_path_buf();
    drop(store);
    // Drop the base's journal entry: its record is no longer reachable.
    let journal = fs::read_dir(root.join("seg"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jrn"))
        .unwrap();
    let entries = fs::read(&journal).unwrap();
    fs::write(&journal, [vec![0u8; 64], entries[64..].to_vec()].concat()).unwrap();

    let store = Store::open(&root, Options::default()).unwrap();
    assert_eq!(store.get(&base_id).unwrap(), None);
    let err = store.get(&delta_id).unwrap_err();
    assert!(err.to_string().contains("missing"), "{err}");
    let report = store.fsck().unwrap();
    assert!(
        report.problems.iter().any(|p| matches!(
            p,
            Problem::BadRecord { detail, .. } if detail.contains("missing")
        )),
        "{:#?}",
        report.problems
    );
}

#[test]
fn a_small_edit_in_a_large_file_adds_few_chunks() {
    let (_dir, store) = new_store();
    let first = noise(1, 20 << 20);
    let mut second = first.clone();
    for (i, byte) in second[10 << 20..(10 << 20) + 16].iter_mut().enumerate() {
        *byte = i as u8;
    }
    let first_id = store.put(Kind::Blob, &first).unwrap();
    let after_first = store.stats().unwrap();
    let second_id = store.put(Kind::Blob, &second).unwrap();
    let after_second = store.stats().unwrap();
    // The second version adds its chunk list and one or two chunks.
    let added = after_second.objects - after_first.objects;
    assert!((2..=4).contains(&added), "{added} objects added");
    assert!(after_first.objects > 100, "{after_first:?}");
    let chunked = after_second
        .codecs
        .iter()
        .find(|c| c.codec == Codec::Chunked)
        .unwrap();
    assert_eq!(chunked.objects, 2);
    assert_eq!(chunked.raw_bytes, 40 << 20);
    // Random bytes stay raw; content shared by both counts once.
    assert!(after_second.raw_bytes < (21 << 20), "{after_second:?}");

    let store = reopen(store);
    assert_eq!(
        store.stat(&second_id).unwrap(),
        Some((Kind::Blob, 20 << 20))
    );
    assert_eq!(store.get(&first_id).unwrap(), Some((Kind::Blob, first)));
    assert_eq!(store.get(&second_id).unwrap(), Some((Kind::Blob, second)));
    let report = store.fsck().unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(report.chunked_records, 2);
}

#[test]
fn streaming_gives_the_same_bytes_and_ids() {
    let (_dir, store) = new_store();
    let sizes = [
        0,
        100,
        CHUNKED_MIN as usize - 1,
        CHUNKED_MIN as usize,
        (6 << 20) + 12_345,
    ];
    for (n, size) in sizes.into_iter().enumerate() {
        let payload = noise(n as u64 + 10, size);
        let id = ObjectId::hash(Kind::Blob, &payload);
        // Read in small, uneven pieces.
        let reader = Trickle {
            data: &payload,
            step: 7777,
        };
        assert_eq!(
            store.put_blob_from(reader, Some(size as u64)).unwrap(),
            (id, size as u64)
        );
        let mut out = Vec::new();
        assert_eq!(
            store.read_to(&id, &mut out).unwrap(),
            Some((Kind::Blob, size as u64))
        );
        assert!(out == payload, "size {size}");
        assert_eq!(store.get(&id).unwrap(), Some((Kind::Blob, payload.clone())));
        // Whole or streamed, a blob is stored the same way.
        assert_eq!(store.put(Kind::Blob, &payload).unwrap(), id);
    }
    // A size hint that is wrong only changes how it is cut.
    let payload = noise(99, 5 << 20);
    let id = ObjectId::hash(Kind::Blob, &payload);
    let (streamed, size) = store.put_blob_from(&payload[..], Some(1 << 30)).unwrap();
    assert_eq!((streamed, size), (id, 5 << 20));
    let store = reopen(store);
    assert_eq!(store.get(&id).unwrap(), Some((Kind::Blob, payload)));
    let mut out = Vec::new();
    assert!(
        store
            .read_to(&ObjectId::hash(Kind::Blob, b"absent"), &mut out)
            .unwrap()
            .is_none()
    );
    assert!(out.is_empty());
    let report = store.fsck().unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(report.chunked_records, 3);
}

/// A reader that hands out at most `step` bytes at a time.
struct Trickle<'a> {
    data: &'a [u8],
    step: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(self.step).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

#[test]
fn a_damaged_chunk_fails_its_blob_cleanly() {
    let (_dir, store) = new_store();
    let big = noise(5, 5 << 20);
    let big_id = store.put(Kind::Blob, &big).unwrap();
    let root = store.root().to_path_buf();
    let store = reopen(store);
    assert!(store.fsck().unwrap().is_clean());
    drop(store);

    // Chunks of random bytes are stored raw: damage one in the middle.
    let (path, at) = find_in_segments(&root, &big[3 << 20..(3 << 20) + 64]);
    flip_byte(&path, at + 10);
    let store = Store::open(&root, Options::default()).unwrap();
    let err = store.get(&big_id).unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }), "{err}");
    assert!(err.to_string().contains("chunk"), "{err}");
    let mut out = Vec::new();
    assert!(store.read_to(&big_id, &mut out).is_err());
    assert_eq!(store.stat(&big_id).unwrap(), Some((Kind::Blob, 5 << 20)));

    let report = store.fsck().unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| matches!(p, Problem::BadEntry { .. })),
        "{:#?}",
        report.problems
    );
    assert!(
        report.problems.iter().any(|p| matches!(
            p,
            Problem::BadRecord { detail, .. } if detail.contains(&big_id.to_hex())
        )),
        "{:#?}",
        report.problems
    );
}

#[test]
fn stats_count_objects_by_kind_and_codec() {
    let (_dir, store) = new_store();
    let base = text(6, 1000);
    let base_id = store.put(Kind::Blob, &base).unwrap();
    store
        .put_similar(Kind::Blob, &edited(&base, 10, b"changed"), &base_id)
        .unwrap();
    store.put(Kind::Changeset, b"small").unwrap();
    store
        .put(Kind::Blob, &noise(7, CHUNKED_MIN as usize))
        .unwrap();
    store.sync().unwrap();
    let stats = store.stats().unwrap();
    let kind = |kind: Kind| stats.kinds.iter().find(|k| k.kind == kind).unwrap();
    let codec = |codec: Codec| stats.codecs.iter().find(|c| c.codec == codec).unwrap();
    assert_eq!(kind(Kind::Changeset).objects, 1);
    assert_eq!(kind(Kind::Tree).objects, 0);
    assert_eq!(codec(Codec::Delta).objects, 1);
    assert_eq!(codec(Codec::Zstd).objects, 1);
    assert_eq!(codec(Codec::Chunked).objects, 1);
    assert_eq!(codec(Codec::Chunked).raw_bytes, CHUNKED_MIN);
    assert_eq!(stats.objects, kind(Kind::Blob).objects + 1);
    assert_eq!(
        stats.objects,
        stats.codecs.iter().map(|c| c.objects).sum::<u64>()
    );
    assert!(stats.record_bytes > stats.stored_bytes);
    assert!(stats.segment_bytes >= stats.record_bytes);
    assert!(stats.index_bytes >= 64 * stats.objects);
    assert_eq!(
        Codec::ALL.map(Codec::name),
        ["raw", "zstd", "dictionary", "delta", "chunked"]
    );
}

/// Ten versions of a large binary file, each with a few bytes changed:
/// the store keeps little more than one version.
#[test]
fn ten_versions_of_a_large_binary() {
    let (dir, store) = new_store();
    let mut version = noise(8, 20 << 20);
    let mut ids = Vec::new();
    for n in 0..10usize {
        if n > 0 {
            let at = (n * 1_900_000) % version.len();
            for byte in &mut version[at..at + 32] {
                *byte = byte.wrapping_add(n as u8);
            }
        }
        ids.push(
            store
                .put_blob_from(&version[..], Some(version.len() as u64))
                .unwrap()
                .0,
        );
    }
    let store = reopen(store);
    let ratio = store_size(dir.path()) as f64 / version.len() as f64;
    eprintln!("store / one version, ten versions of a large binary: {ratio:.3}");
    assert!(ratio < 1.15, "{ratio}");
    let mut out = Vec::new();
    store.read_to(&ids[9], &mut out).unwrap();
    assert!(out == version);
    assert_eq!(store.fsck().unwrap().chunked_records, 10);
}
