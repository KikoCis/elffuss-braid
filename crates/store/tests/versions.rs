// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Storage versions: stores that need a newer version of braid are refused,
//! and what such a version wrote is never written over; a store of an
//! older version is raised only when it gets records that need it.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use braid_store::{Error, Kind, ObjectId, Objects, Options, Problem, STORAGE_VERSION, Store};

const VERSION_1: &str = "object-format=blake3-dk-v1\n";
const VERSION_2: &str = "object-format=blake3-dk-v1\nstorage=2\n";

fn new_store(options: Options) -> (tempfile::TempDir, PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let store = Store::init(&root, options).unwrap();
    (dir, root, store)
}

fn format_of(root: &Path) -> String {
    fs::read_to_string(root.join("format")).unwrap()
}

/// Every file of the segments, with its bytes.
fn segment_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<(PathBuf, Vec<u8>)> = fs::read_dir(root.join("seg"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    files.sort();
    files
}

fn file_with_extension(root: &Path, extension: &str) -> PathBuf {
    segment_files(root)
        .into_iter()
        .map(|(path, _)| path)
        .find(|path| path.extension().is_some_and(|ext| ext == extension))
        .unwrap()
}

/// Source-like text, and the same with a small edit: a delta.
fn versions() -> (Vec<u8>, Vec<u8>) {
    let first: Vec<u8> = (0..2000u64)
        .flat_map(|i| {
            format!("line {i}: {:016x}\n", i.wrapping_mul(0x9e37_79b9_7f4a_7c15)).into_bytes()
        })
        .collect();
    let mut second = first.clone();
    second[1000..1010].copy_from_slice(b"0123456789");
    (first, second)
}

/// CRC-32C, bit by bit, to forge entries in a test.
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[test]
fn new_stores_are_at_the_current_version() {
    let (_dir, root, store) = new_store(Options::default());
    assert_eq!(STORAGE_VERSION, 2);
    assert_eq!(format_of(&root), VERSION_2);
    drop(store);
    // init does not touch an existing store's format file.
    Store::init(&root, Options::default()).unwrap();
    assert_eq!(format_of(&root), VERSION_2);
}

#[test]
fn a_newer_storage_version_is_refused() {
    let (_dir, root, store) = new_store(Options::default());
    store.put(Kind::Blob, b"written at version 2").unwrap();
    store.sync().unwrap();
    drop(store);
    let newer = "object-format=blake3-dk-v1\nstorage=3\n";
    fs::write(root.join("format"), newer).unwrap();
    let before = segment_files(&root);

    for result in [
        Store::open(&root, Options::default()),
        Store::init(&root, Options::default()),
    ] {
        let err = result.unwrap_err();
        assert!(matches!(err, Error::NewerStore { .. }), "{err}");
        let text = err.to_string();
        assert!(text.contains("needs a newer version of braid"), "{text}");
        assert!(text.contains("storage version 3"), "{text}");
    }
    assert_eq!(format_of(&root), newer);
    assert_eq!(segment_files(&root), before);
}

#[test]
fn an_older_store_is_raised_only_by_records_that_need_it() {
    let (_dir, root, store) = new_store(Options::default());
    drop(store);
    // As a version of braid from before storage versions left it.
    fs::write(root.join("format"), VERSION_1).unwrap();

    let store = Store::open(&root, Options::default()).unwrap();
    let (first, second) = versions();
    let first_id = store.put(Kind::Blob, &first).unwrap();
    store.put(Kind::Tree, b"stored whole").unwrap();
    store.sync().unwrap();
    // Records stored whole are what every version reads.
    assert_eq!(format_of(&root), VERSION_1);
    assert!(store.fsck().unwrap().is_clean());

    // The first delta raises the store, before it is written.
    let second_id = store.put_similar(Kind::Blob, &second, &first_id).unwrap();
    assert_eq!(format_of(&root), VERSION_2);
    store.sync().unwrap();
    let report = store.fsck().unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(report.delta_records, 1);
    assert_eq!(
        store.get(&second_id).unwrap(),
        Some((Kind::Blob, second.clone()))
    );

    // A large blob does the same on another older store.
    let (_dir, root, store) = new_store(Options::default());
    drop(store);
    fs::write(root.join("format"), VERSION_1).unwrap();
    let store = Store::open(&root, Options::default()).unwrap();
    store
        .put(Kind::Blob, &vec![1u8; braid_store::CHUNKED_MIN as usize])
        .unwrap();
    assert_eq!(format_of(&root), VERSION_2);
}

#[test]
fn records_that_need_a_later_version_raise_the_store_when_it_is_opened() {
    let (_dir, root, store) = new_store(Options::default());
    let (first, second) = versions();
    let first_id = store.put(Kind::Blob, &first).unwrap();
    store.put_similar(Kind::Blob, &second, &first_id).unwrap();
    store.sync().unwrap();
    // A format file that says less than the store holds, as a writer from
    // before storage versions could leave behind.
    fs::write(root.join("format"), VERSION_1).unwrap();
    let report = store.fsck().unwrap();
    assert!(
        matches!(report.problems.as_slice(), [Problem::Format { detail }] if detail.contains("version 2")),
        "{:#?}",
        report.problems
    );
    drop(store);
    let store = Store::open(&root, Options::default()).unwrap();
    assert_eq!(format_of(&root), VERSION_2);
    assert!(store.fsck().unwrap().is_clean());
}

#[test]
fn journal_entries_of_a_newer_version_are_refused_not_overwritten() {
    let (_dir, root, store) = new_store(Options::default());
    for n in 0..5u8 {
        store.put(Kind::Blob, &[n; 100]).unwrap();
    }
    store.sync().unwrap();
    drop(store);
    // A handle that opened before a newer version wrote.
    let older = Store::open(&root, Options::default()).unwrap();

    // A newer version appends a record and its entry: the record is beyond
    // what this version parses, the entry whole, with a codec it does not
    // know.
    let journal = file_with_extension(&root, "jrn");
    let segment = file_with_extension(&root, "seg");
    let offset = fs::metadata(&segment).unwrap().len().next_multiple_of(8);
    OpenOptions::new()
        .append(true)
        .open(&segment)
        .unwrap()
        .write_all(&[0xab; 128])
        .unwrap();
    let mut entry = [0u8; 64];
    entry[..32].copy_from_slice(ObjectId::hash(Kind::Blob, b"from the future").as_bytes());
    entry[32..40].copy_from_slice(&offset.to_le_bytes());
    entry[40..44].copy_from_slice(&64u32.to_le_bytes());
    entry[44] = Kind::Blob.code();
    entry[45] = 9;
    entry[48..56].copy_from_slice(&1000u64.to_le_bytes());
    let crc = crc32c(&entry[..60]);
    entry[60..64].copy_from_slice(&crc.to_le_bytes());
    OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(&entry)
        .unwrap();
    let before = segment_files(&root);

    // Opening refuses.
    let err = Store::open(&root, Options::default()).unwrap_err();
    assert!(matches!(err, Error::NewerStore { .. }), "{err}");
    assert!(err.to_string().contains(".jrn"), "{err}");
    // The handle that opened before cannot take the segment over and write
    // after an entry it does not understand.
    let err = older.put(Kind::Blob, b"one more").unwrap_err();
    assert!(matches!(err, Error::NewerStore { .. }), "{err}");
    assert!(older.rescan().is_err());
    drop(older);
    assert_eq!(segment_files(&root), before);
}

#[test]
fn an_index_of_a_newer_version_is_refused_not_scanned() {
    let options = Options {
        segment_size: 1024,
        ..Options::default()
    };
    let (_dir, root, store) = new_store(options.clone());
    for n in 0..40u8 {
        store.put(Kind::Blob, &[n; 300]).unwrap();
    }
    store.sync().unwrap();
    drop(store);
    // An index whose hash holds, of an index version this one does not know.
    let index = file_with_extension(&root, "idx");
    let mut bytes = fs::read(&index).unwrap();
    bytes[8] = 2;
    let body = bytes.len() - 32;
    let hash = *blake3::hash(&bytes[..body]).as_bytes();
    bytes[body..].copy_from_slice(&hash);
    fs::write(&index, &bytes).unwrap();
    let before = segment_files(&root);

    let err = Store::open(&root, options).unwrap_err();
    assert!(matches!(err, Error::NewerStore { .. }), "{err}");
    assert!(err.to_string().contains(".idx"), "{err}");
    assert_eq!(segment_files(&root), before);
}
