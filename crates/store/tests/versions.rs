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
const VERSION_3: &str = "object-format=blake3-dk-v1\nstorage=3\n";

fn new_store(options: Options) -> (tempfile::TempDir, PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let store = Store::init(&root, options).unwrap();
    (dir, root, store)
}

fn format_of(root: &Path) -> String {
    fs::read_to_string(root.join("format")).unwrap()
}

/// Every file of the segments and their indexes, with its bytes.
fn segment_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<(PathBuf, Vec<u8>)> = fs::read_dir(root.join("seg"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        // The lock merges take holds nothing.
        .filter(|path| !path.ends_with("merge.lock"))
        .map(|path| {
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
    assert_eq!(STORAGE_VERSION, 3);
    assert_eq!(format_of(&root), VERSION_3);
    drop(store);
    // init does not touch an existing store's format file.
    Store::init(&root, Options::default()).unwrap();
    assert_eq!(format_of(&root), VERSION_3);
}

#[test]
fn a_newer_storage_version_is_refused() {
    let (_dir, root, store) = new_store(Options::default());
    store.put(Kind::Blob, b"written at version 3").unwrap();
    store.sync().unwrap();
    drop(store);
    let newer = "object-format=blake3-dk-v1\nstorage=4\n";
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
        assert!(text.contains("storage version 4"), "{text}");
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
        merge_threshold: 0,
        ..Options::default()
    };
    let (_dir, root, store) = new_store(options.clone());
    let ids: Vec<ObjectId> = (0..40u8)
        .map(|n| store.put(Kind::Blob, &[n; 300]).unwrap())
        .collect();
    store.sync().unwrap();
    drop(store);
    // An index whose header checks out, of an index version this one does
    // not know.
    let index = file_with_extension(&root, "idx");
    let mut bytes = fs::read(&index).unwrap();
    bytes[8] = 3;
    let crc = crc32c(&bytes[..28]);
    bytes[28..32].copy_from_slice(&crc.to_le_bytes());
    fs::write(&index, &bytes).unwrap();
    let before = segment_files(&root);

    // Indexes are read as lookups need them: a lookup that reads this one
    // fails, rather than scanning the segment or missing what it holds.
    let store = Store::open(&root, options).unwrap();
    let mut refused = 0;
    for (n, id) in (0..40u8).zip(&ids) {
        match store.get(id) {
            Ok(found) => assert_eq!(found, Some((Kind::Blob, vec![n; 300]))),
            Err(err) => {
                assert!(matches!(err, Error::NewerStore { .. }), "{err}");
                assert!(err.to_string().contains(".idx"), "{err}");
                refused += 1;
            }
        }
    }
    assert!(refused > 0);
    // Merging needs every index whole: it refuses too, and changes nothing.
    assert!(matches!(
        store.merge_indexes(),
        Err(Error::NewerStore { .. })
    ));
    let report = store.fsck().unwrap();
    assert!(
        report.problems.iter().any(
            |problem| matches!(problem, Problem::Newer { path, .. } if path.ends_with(".idx"))
        ),
        "{:#?}",
        report.problems
    );
    drop(store);
    assert_eq!(segment_files(&root), before);
}

/// Rewrites an index of version 2 that covers one segment as version 1, as
/// storage version 2 wrote them.
fn as_version_1(bytes: &[u8]) -> Vec<u8> {
    let count = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
    assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 1);
    let segment_len = &bytes[48..56];
    let fanout = &bytes[56..56 + 1024];
    let entries = &bytes[56 + 1024 + 4..];
    assert_eq!(entries.len(), count * 64);
    let mut out = b"braididx".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(count as u64).to_le_bytes());
    out.extend_from_slice(segment_len);
    out.extend_from_slice(fanout);
    for entry in entries.chunks_exact(64) {
        out.extend_from_slice(&entry[..32]);
    }
    for entry in entries.chunks_exact(64) {
        out.extend_from_slice(&entry[32..60]);
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    let hash = *blake3::hash(&out).as_bytes();
    out.extend_from_slice(&hash);
    out
}

#[test]
fn a_store_of_version_2_is_read_and_raised_when_it_writes_an_index() {
    let options = Options {
        segment_records: 10,
        merge_threshold: 0,
        ..Options::default()
    };
    let (_dir, root, store) = new_store(options.clone());
    let (first, second) = versions();
    let mut objects: Vec<(Kind, Vec<u8>)> =
        vec![(Kind::Blob, first.clone()), (Kind::Blob, second.clone())];
    let first_id = store.put(Kind::Blob, &first).unwrap();
    store.put_similar(Kind::Blob, &second, &first_id).unwrap();
    for n in 0..58u32 {
        let payload = format!("object {n}").repeat(10).into_bytes();
        store.put(Kind::Tree, &payload).unwrap();
        objects.push((Kind::Tree, payload));
    }
    store.sync().unwrap();
    drop(store);
    // As storage version 2 left it: indexes of version 1.
    let indexes: Vec<PathBuf> = segment_files(&root)
        .into_iter()
        .map(|(path, _)| path)
        .filter(|path| path.extension().is_some_and(|ext| ext == "idx"))
        .collect();
    assert_eq!(indexes.len(), 6);
    for index in &indexes {
        let bytes = fs::read(index).unwrap();
        fs::write(index, as_version_1(&bytes)).unwrap();
    }
    fs::write(root.join("format"), VERSION_2).unwrap();

    // Read as they are, and not raised by reading.
    let store = Store::open(&root, options.clone()).unwrap();
    for (kind, payload) in &objects {
        let id = ObjectId::hash(*kind, payload);
        assert_eq!(store.get(&id).unwrap(), Some((*kind, payload.clone())));
    }
    let report = store.fsck().unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(report.delta_records, 1);
    assert_eq!(format_of(&root), VERSION_2);

    // The first index of version 2 raises it, before it is written.
    for n in 0..10u32 {
        store.put(Kind::Blob, &n.to_le_bytes()).unwrap();
    }
    assert_eq!(format_of(&root), VERSION_3);
    // Merged with the others.
    assert!(store.merge_indexes().unwrap());
    let store = Store::open(&root, options).unwrap();
    for (kind, payload) in &objects {
        let id = ObjectId::hash(*kind, payload);
        assert_eq!(store.get(&id).unwrap(), Some((*kind, payload.clone())));
    }
    let report = store.fsck().unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(report.objects, 70);
}
