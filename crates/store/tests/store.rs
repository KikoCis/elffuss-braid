// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The store through its public API: reopening, sealing, several handles,
//! torn tails, damage, and the op log.

use std::convert::Infallible;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use braid_store::{
    Durability, Error, Kind, ObjectId, Objects, Options, Problem, RefUpdate, Store, TransactError,
};

fn options(segment_size: u64, checkpoint_interval: u64) -> Options {
    Options {
        segment_size,
        checkpoint_interval,
        ..Options::default()
    }
}

fn new_store(options: Options) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path().join("store"), options).unwrap();
    (dir, store)
}

/// Files in `seg/` with this extension, sorted.
fn segment_files(store: &Store, extension: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(store.root().join("seg"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == extension))
        .collect();
    files.sort();
    files
}

fn flip_byte(path: &Path, offset: u64) {
    let mut bytes = fs::read(path).unwrap();
    bytes[offset as usize] ^= 0x20;
    fs::write(path, bytes).unwrap();
}

fn append_bytes(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

/// A mix of payloads: tiny, compressible, incompressible.
fn payloads(n: usize) -> Vec<(Kind, Vec<u8>)> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..n)
        .map(|i| {
            let kind = Kind::ALL[i % Kind::ALL.len()];
            let payload = match i % 3 {
                0 => format!("small {i}").into_bytes(),
                1 => format!("line {i} of a compressible object\n")
                    .repeat(40)
                    .into_bytes(),
                _ => (0..500 + i * 7).map(|_| next() as u8).collect(),
            };
            (kind, payload)
        })
        .collect()
}

fn assert_clean(store: &Store) -> braid_store::FsckReport {
    let report = store.fsck().unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    report
}

fn set(name: &str, old: Option<ObjectId>, new: Option<ObjectId>) -> Vec<RefUpdate> {
    vec![RefUpdate::new(name, old, new)]
}

#[test]
fn objects_survive_reopen() {
    let (dir, store) = new_store(Options::default());
    let root = store.root().to_path_buf();
    let objects = payloads(30);
    let ids: Vec<ObjectId> = objects
        .iter()
        .map(|(kind, payload)| store.put(*kind, payload).unwrap())
        .collect();
    store.sync().unwrap();
    for ((kind, payload), id) in objects.iter().zip(&ids) {
        assert_eq!(*id, ObjectId::hash(*kind, payload));
        assert_eq!(store.get(id).unwrap(), Some((*kind, payload.clone())));
    }
    drop(store);

    let store = Store::open(&root, Options::default()).unwrap();
    for ((kind, payload), id) in objects.iter().zip(&ids) {
        assert!(store.contains(id).unwrap());
        assert_eq!(store.get(id).unwrap(), Some((*kind, payload.clone())));
    }
    let absent = ObjectId::hash(Kind::Blob, b"never stored");
    assert_eq!(store.get(&absent).unwrap(), None);
    assert!(!store.contains(&absent).unwrap());
    let report = assert_clean(&store);
    assert_eq!(report.objects, 30);
    assert_eq!(report.records, 30);
    drop(dir);
}

#[test]
fn stat_reads_only_the_header() {
    let (_dir, a) = new_store(options(4096, 1000));
    let root = a.root().to_path_buf();
    let objects = payloads(40);
    for (kind, payload) in &objects {
        a.put(*kind, payload).unwrap();
    }
    a.sync().unwrap();
    // Raw, compressed, sealed and open records alike, from another handle.
    let b = Store::open(&root, options(4096, 1000)).unwrap();
    for (kind, payload) in &objects {
        let id = ObjectId::hash(*kind, payload);
        assert_eq!(b.stat(&id).unwrap(), Some((*kind, payload.len() as u64)));
    }
    let fresh = a.put(Kind::Tree, b"written after b opened").unwrap();
    assert_eq!(b.stat(&fresh).unwrap(), Some((Kind::Tree, 22)));
    let absent = ObjectId::hash(Kind::Blob, b"never stored");
    assert_eq!(b.stat(&absent).unwrap(), None);
    drop((a, b));

    // A store with a single record at the start of a single segment.
    let (_dir, store) = new_store(Options::default());
    let root = store.root().to_path_buf();
    let payload = b"payload damage does not reach stat";
    let id = store.put(Kind::Blob, payload).unwrap();
    store.sync().unwrap();
    let seg = segment_files(&store, "seg").remove(0);
    drop(store);

    // Damage to the payload breaks get, not stat: stat never reads it.
    flip_byte(&seg, 60);
    let store = Store::open(&root, Options::default()).unwrap();
    assert!(matches!(store.get(&id), Err(Error::Corrupt { .. })));
    assert_eq!(
        store.stat(&id).unwrap(),
        Some((Kind::Blob, payload.len() as u64))
    );
    drop(store);

    // Damage to the header is caught by comparing it with the entry.
    flip_byte(&seg, 12);
    let store = Store::open(&root, Options::default()).unwrap();
    assert!(matches!(store.stat(&id), Err(Error::Corrupt { .. })));
}

#[test]
fn storing_twice_writes_once() {
    let (_dir, store) = new_store(Options::default());
    let first = store.put(Kind::Blob, b"same").unwrap();
    let second = store.put(Kind::Blob, b"same").unwrap();
    assert_eq!(first, second);
    // Same bytes, other kind: another object.
    let tree = store.put(Kind::Tree, b"same").unwrap();
    assert_ne!(first, tree);
    store.sync().unwrap();
    let report = assert_clean(&store);
    assert_eq!(report.records, 2);
}

#[test]
fn opening_needs_a_store() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        Store::open(dir.path(), Options::default()),
        Err(Error::NotAStore { .. })
    ));
    let store = Store::init(dir.path(), Options::default()).unwrap();
    drop(store);
    // init is idempotent.
    Store::init(dir.path(), Options::default()).unwrap();
    fs::write(dir.path().join("format"), "object-format=sha1\n").unwrap();
    assert!(matches!(
        Store::open(dir.path(), Options::default()),
        Err(Error::UnsupportedFormat { .. })
    ));
}

#[test]
fn full_segments_are_sealed() {
    let (_dir, store) = new_store(options(4096, 1000));
    let root = store.root().to_path_buf();
    let objects = payloads(120);
    for (kind, payload) in &objects {
        store.put(*kind, payload).unwrap();
    }
    store.sync().unwrap();
    let sealed = segment_files(&store, "idx");
    assert!(sealed.len() >= 3, "{} sealed segments", sealed.len());
    // A sealed segment has no journal any more.
    for idx in &sealed {
        assert!(!idx.with_extension("jrn").exists());
    }
    let report = assert_clean(&store);
    assert_eq!(report.sealed_segments, sealed.len());
    assert_eq!(report.objects, 120);
    drop(store);

    let store = Store::open(&root, options(4096, 1000)).unwrap();
    for (kind, payload) in &objects {
        let id = ObjectId::hash(*kind, payload);
        assert_eq!(store.get(&id).unwrap(), Some((*kind, payload.clone())));
    }
}

#[test]
fn handles_see_each_other() {
    let (_dir, a) = new_store(Options::default());
    let b = Store::open(a.root(), Options::default()).unwrap();
    let from_a = a.put(Kind::Blob, b"written by a").unwrap();
    let from_b = b.put(Kind::Blob, b"written by b").unwrap();
    // Each handle holds its own segment: the locks exclude each other even
    // inside one process.
    assert_eq!(segment_files(&a, "seg").len(), 2);
    // A miss rescans the directory.
    assert_eq!(
        b.get(&from_a).unwrap(),
        Some((Kind::Blob, b"written by a".to_vec()))
    );
    assert_eq!(
        a.get(&from_b).unwrap(),
        Some((Kind::Blob, b"written by b".to_vec()))
    );
    a.sync().unwrap();
    b.sync().unwrap();
    assert_clean(&a);
}

#[test]
fn a_copy_in_another_writers_segment_is_reused() {
    let (_dir, a) = new_store(Options::default());
    let b = Store::open(a.root(), Options::default()).unwrap();
    let id = a.put(Kind::Blob, b"shared content").unwrap();
    b.rescan().unwrap();
    // b relies on a's copy, which a has not flushed: b's sync flushes it.
    assert_eq!(b.put(Kind::Blob, b"shared content").unwrap(), id);
    b.sync().unwrap();
    let report = assert_clean(&b);
    assert_eq!(report.records, 1);
}

#[test]
fn a_damaged_copy_in_an_open_segment_is_written_again() {
    let (_dir, a) = new_store(Options::default());
    let root = a.root().to_path_buf();
    let id = a.put(Kind::Blob, b"content that will be damaged").unwrap();
    a.sync().unwrap();
    drop(a);
    let seg = Store::open(&root, Options::default()).unwrap();
    let path = segment_files(&seg, "seg").remove(0);
    drop(seg);
    flip_byte(&path, 60);

    let b = Store::open(&root, Options::default()).unwrap();
    assert!(matches!(b.get(&id), Err(Error::Corrupt { .. })));
    assert_eq!(
        b.put(Kind::Blob, b"content that will be damaged").unwrap(),
        id
    );
    assert_eq!(
        b.get(&id).unwrap(),
        Some((Kind::Blob, b"content that will be damaged".to_vec()))
    );
    b.sync().unwrap();
    // The damaged record is still reported.
    let report = b.fsck().unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| matches!(p, Problem::BadEntry { id: bad, .. } if *bad == id)),
        "{:#?}",
        report.problems
    );
}

#[test]
fn threads_share_a_handle() {
    let (_dir, store) = new_store(options(16 << 10, 1000));
    let objects = payloads(200);
    std::thread::scope(|scope| {
        for chunk in objects.chunks(25) {
            let store = &store;
            scope.spawn(move || {
                for (kind, payload) in chunk {
                    let id = store.put(*kind, payload).unwrap();
                    assert_eq!(store.get(&id).unwrap(), Some((*kind, payload.clone())));
                }
                store.sync().unwrap();
            });
        }
    });
    let report = assert_clean(&store);
    assert_eq!(report.objects, 200);
}

#[test]
fn an_abandoned_full_segment_is_sealed_by_the_next_writer() {
    let (_dir, a) = new_store(Options::default());
    let root = a.root().to_path_buf();
    for (kind, payload) in payloads(20) {
        a.put(kind, &payload).unwrap();
    }
    a.sync().unwrap();
    drop(a);
    assert!(segment_files(&Store::open(&root, Options::default()).unwrap(), "idx").is_empty());

    let b = Store::open(&root, options(1024, 1000)).unwrap();
    b.put(Kind::Blob, b"one more").unwrap();
    b.sync().unwrap();
    assert_eq!(segment_files(&b, "idx").len(), 1);
    assert_eq!(segment_files(&b, "seg").len(), 2);
    let report = assert_clean(&b);
    assert_eq!(report.objects, 21);
}

#[test]
fn a_torn_segment_tail_is_skipped() {
    let (_dir, store) = new_store(Options::default());
    let root = store.root().to_path_buf();
    let first = payloads(10);
    for (kind, payload) in &first {
        store.put(*kind, payload).unwrap();
    }
    store.sync().unwrap();
    let seg = segment_files(&store, "seg").remove(0);
    drop(store);
    // Half of a record, as a crash in the middle of an append leaves it.
    let mut torn = fs::read(&seg).unwrap();
    torn.truncate(41);
    append_bytes(&seg, &torn);

    let store = Store::open(&root, Options::default()).unwrap();
    let second = payloads(25);
    for (kind, payload) in &second {
        store.put(*kind, payload).unwrap();
    }
    store.sync().unwrap();
    drop(store);

    let store = Store::open(&root, Options::default()).unwrap();
    for (kind, payload) in first.iter().chain(&second) {
        let id = ObjectId::hash(*kind, payload);
        assert_eq!(store.get(&id).unwrap(), Some((*kind, payload.clone())));
    }
    let report = assert_clean(&store);
    assert!(report.torn_segment_bytes >= 41, "{report:?}");
    assert_eq!(report.objects, 25);
}

#[test]
fn a_torn_journal_tail_is_skipped_and_overwritten() {
    let (_dir, store) = new_store(Options::default());
    let root = store.root().to_path_buf();
    let objects = payloads(12);
    for (kind, payload) in &objects[..6] {
        store.put(*kind, payload).unwrap();
    }
    store.sync().unwrap();
    let jrn = segment_files(&store, "jrn").remove(0);
    drop(store);
    append_bytes(&jrn, &[0xab; 30]);

    let store = Store::open(&root, Options::default()).unwrap();
    for (kind, payload) in &objects[6..] {
        store.put(*kind, payload).unwrap();
    }
    store.sync().unwrap();
    assert_eq!(fs::metadata(&jrn).unwrap().len(), 12 * 64);
    drop(store);

    let store = Store::open(&root, Options::default()).unwrap();
    for (kind, payload) in &objects {
        let id = ObjectId::hash(*kind, payload);
        assert_eq!(store.get(&id).unwrap(), Some((*kind, payload.clone())));
    }
    let report = assert_clean(&store);
    assert_eq!(report.torn_journal_entries, 0);
}

#[test]
fn a_damaged_record_is_an_error_and_fsck_reports_it() {
    let (_dir, store) = new_store(Options::default());
    let root = store.root().to_path_buf();
    let id = store
        .put(Kind::Blob, b"a record that will lose a bit")
        .unwrap();
    store.sync().unwrap();
    let seg = segment_files(&store, "seg").remove(0);
    drop(store);
    flip_byte(&seg, 60);

    let store = Store::open(&root, Options::default()).unwrap();
    let err = store.get(&id).unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }), "{err}");
    let report = store.fsck().unwrap();
    assert!(!report.is_clean());
    assert!(
        report
            .problems
            .iter()
            .any(|p| matches!(p, Problem::BadEntry { id: bad, .. } if *bad == id)),
        "{:#?}",
        report.problems
    );
}

#[test]
fn content_that_does_not_match_its_id_is_an_error() {
    let (_dir, store) = new_store(Options::default());
    let root = store.root().to_path_buf();
    let id = store.put(Kind::Blob, b"original content").unwrap();
    store.sync().unwrap();
    let seg = segment_files(&store, "seg").remove(0);
    drop(store);
    // Change the payload and fix the record's CRC, as a buggy writer might.
    let mut bytes = fs::read(&seg).unwrap();
    bytes[52] = b'O';
    let crc = crc32c(&bytes[..52 + 16]);
    bytes[68..72].copy_from_slice(&crc.to_le_bytes());
    fs::write(&seg, bytes).unwrap();

    let store = Store::open(&root, Options::default()).unwrap();
    assert!(matches!(store.get(&id), Err(Error::Corrupt { .. })));
    let report = store.fsck().unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| matches!(p, Problem::BadRecord { .. })),
        "{:#?}",
        report.problems
    );
}

/// CRC-32C, bit by bit, to forge a valid record in a test.
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
fn a_damaged_index_is_reported_and_worked_around() {
    let (_dir, store) = new_store(options(1024, 1000));
    let root = store.root().to_path_buf();
    let objects = payloads(20);
    for (kind, payload) in &objects {
        store.put(*kind, payload).unwrap();
    }
    store.sync().unwrap();
    let idx = segment_files(&store, "idx").remove(0);
    drop(store);
    flip_byte(&idx, 1100);

    let store = Store::open(&root, options(1024, 1000)).unwrap();
    for (kind, payload) in &objects {
        let id = ObjectId::hash(*kind, payload);
        assert_eq!(store.get(&id).unwrap(), Some((*kind, payload.clone())));
    }
    let report = store.fsck().unwrap();
    assert_eq!(report.objects, 20);
    assert!(
        matches!(report.problems.as_slice(), [Problem::BadIndex { .. }]),
        "{:#?}",
        report.problems
    );
}

#[test]
fn transactions_compare_and_swap() {
    let (_dir, store) = new_store(Options::default());
    let a = store.put(Kind::Changeset, b"a").unwrap();
    let b = store.put(Kind::Changeset, b"b").unwrap();
    store.sync().unwrap();
    assert!(store.refs().unwrap().is_empty());

    let op = store
        .transact("agent-1", "create main", |refs| {
            assert_eq!(refs.seq(), 0);
            Ok::<_, Infallible>(set("main", None, Some(a)))
        })
        .unwrap();
    assert_eq!(op.seq, 1);
    assert_eq!(op.agent, "agent-1");
    assert_eq!(store.refs().unwrap().get("main"), Some(a));

    // A stale expectation is a conflict, and nothing is recorded.
    let err = store
        .transact("agent-2", "stale", |_| {
            Ok::<_, Infallible>(set("main", None, Some(b)))
        })
        .unwrap_err();
    assert!(err.is_conflict(), "{err}");
    assert_eq!(store.refs().unwrap().seq(), 1);

    store
        .transact("agent-2", "move main", |refs| {
            Ok::<_, Infallible>(set("main", refs.get("main"), Some(b)))
        })
        .unwrap();

    // Refused before anything is written.
    let missing = ObjectId::hash(Kind::Changeset, b"not stored");
    let err = store
        .transact("agent-1", "dangling", |_| {
            Ok::<_, Infallible>(set("other", None, Some(missing)))
        })
        .unwrap_err();
    assert!(matches!(
        err,
        TransactError::Store(Error::MissingObject { .. })
    ));
    let err = store
        .transact("agent-1", "twice", |_| {
            Ok::<_, Infallible>(vec![
                RefUpdate::new("x", None, Some(a)),
                RefUpdate::new("x", None, Some(b)),
            ])
        })
        .unwrap_err();
    assert!(matches!(
        err,
        TransactError::Store(Error::DuplicateRef { .. })
    ));
    let err = store
        .transact("agent-1", "bad name", |_| {
            Ok::<_, Infallible>(set("", None, Some(a)))
        })
        .unwrap_err();
    assert!(matches!(
        err,
        TransactError::Store(Error::InvalidRefName { .. })
    ));
    let err = store
        .transact("agent-1", "aborted", |_| {
            Err::<Vec<RefUpdate>, _>("changed my mind")
        })
        .unwrap_err();
    assert!(matches!(err, TransactError::Aborted("changed my mind")));

    // Delete, and several refs at once.
    store
        .transact("agent-3", "branch and delete", |refs| {
            Ok::<_, Infallible>(vec![
                RefUpdate::new("main", refs.get("main"), None),
                RefUpdate::new("feature", None, Some(a)),
            ])
        })
        .unwrap();
    let refs = store.refs().unwrap();
    assert_eq!(refs.seq(), 3);
    assert_eq!(refs.get("main"), None);
    assert_eq!(refs.get("feature"), Some(a));
    assert_eq!(refs.len(), 1);
    assert_clean(&store);
}

#[test]
fn a_transaction_can_say_what_it_found() {
    let (_dir, store) = new_store(Options::default());
    let a = store.put(Kind::Changeset, b"a").unwrap();
    store.sync().unwrap();
    let op = store
        .transact_with("agent-1", |refs| {
            let found = match refs.get("main") {
                Some(_) => "there",
                None => "absent",
            };
            Ok::<_, Infallible>((set("main", None, Some(a)), format!("main was {found}")))
        })
        .unwrap();
    assert_eq!(op.description, "main was absent");
    assert_eq!(store.log(1).unwrap()[0].description, "main was absent");

    // A description over the limit is refused, and nothing is recorded.
    let err = store
        .transact_with("agent-1", |refs| {
            let long = "x".repeat((64 << 10) + 1);
            Ok::<_, Infallible>((set("main", refs.get("main"), None), long))
        })
        .unwrap_err();
    assert!(matches!(err, TransactError::Store(Error::TooLong { .. })));
    assert_eq!(store.refs().unwrap().get("main"), Some(a));
    assert_eq!(store.log(10).unwrap().len(), 1);
}

#[test]
fn short_ids_find_objects_by_hex_prefix() {
    let (_dir, a) = new_store(options(4096, 1000));
    // Objects in sealed segments and in the open one.
    let ids: Vec<ObjectId> = payloads(60)
        .iter()
        .map(|(kind, payload)| a.put(*kind, payload).unwrap())
        .collect();
    a.sync().unwrap();
    assert!(!segment_files(&a, "idx").is_empty());
    for id in &ids {
        let hex = id.to_hex();
        for len in [6, 7, 12, 64] {
            assert_eq!(a.ids_with_prefix(&hex[..len], 10).unwrap(), [*id], "{len}");
        }
    }
    // Sorted, and cut at the limit.
    let mut all = ids.clone();
    all.sort();
    assert_eq!(a.ids_with_prefix("", usize::MAX).unwrap(), all);
    assert_eq!(a.ids_with_prefix("", 3).unwrap(), all[..3]);
    let digit = &all[0].to_hex()[..1];
    let starting: Vec<ObjectId> = all
        .iter()
        .filter(|id| id.to_hex().starts_with(digit))
        .copied()
        .collect();
    assert_eq!(a.ids_with_prefix(digit, usize::MAX).unwrap(), starting);
    // Anything but lowercase hex digits matches nothing.
    for bad in ["ABCDEF", "xyz123", " 01234", &"0".repeat(65)] {
        assert!(a.ids_with_prefix(bad, 10).unwrap().is_empty(), "{bad:?}");
    }

    // What another handle wrote after this one opened is found on a miss.
    let b = Store::open(a.root(), options(4096, 1000)).unwrap();
    let fresh = a.put(Kind::Blob, b"written after b opened").unwrap();
    a.sync().unwrap();
    assert_eq!(
        b.ids_with_prefix(&fresh.to_hex()[..8], 10).unwrap(),
        [fresh]
    );
}

#[test]
fn the_op_log_cannot_be_used_inside_a_transaction() {
    let (_dir, store) = new_store(Options::default());
    let err = store
        .transact("agent", "nested", |_| {
            assert!(matches!(store.refs(), Err(Error::Reentrant)));
            assert!(matches!(store.log(1), Err(Error::Reentrant)));
            Err::<Vec<RefUpdate>, _>(())
        })
        .unwrap_err();
    assert!(matches!(err, TransactError::Aborted(())));
    // Objects can be written from inside, and are flushed before the record.
    let op = store
        .transact("agent", "write inside", |refs| {
            let id = store.put(Kind::Blob, b"written inside").unwrap();
            Ok::<_, Infallible>(set("inside", refs.get("inside"), Some(id)))
        })
        .unwrap();
    assert_eq!(op.seq, 1);
    assert_clean(&store);
}

#[test]
fn fsck_inside_a_transaction_is_refused() {
    let (_dir, store) = new_store(Options::default());
    let err = store
        .transact("agent", "nested", |_| {
            assert!(matches!(store.fsck(), Err(Error::Reentrant)));
            assert!(matches!(store.undo_last("x"), Err(Error::Reentrant)));
            Err::<Vec<RefUpdate>, _>(())
        })
        .unwrap_err();
    assert!(matches!(err, TransactError::Aborted(())));
}

#[test]
fn many_threads_can_create_the_same_store() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("a").join("store");
    std::thread::scope(|scope| {
        for n in 0..8u8 {
            let root = &root;
            scope.spawn(move || {
                let store = Store::init(root, Options::default()).unwrap();
                store.put(Kind::Blob, &[n]).unwrap();
                store.sync().unwrap();
            });
        }
    });
    let store = Store::open(&root, Options::default()).unwrap();
    for n in 0..8u8 {
        assert!(store.contains(&ObjectId::hash(Kind::Blob, &[n])).unwrap());
    }
    assert_eq!(
        fs::read_to_string(root.join("format")).unwrap(),
        "object-format=blake3-dk-v1\nstorage=2\n"
    );
    assert_clean(&store);
}

#[test]
fn the_log_is_newest_first_and_survives_reopen() {
    let (_dir, store) = new_store(Options::default());
    let root = store.root().to_path_buf();
    let ids: Vec<ObjectId> = (0..5u8)
        .map(|n| store.put(Kind::Changeset, &[n]).unwrap())
        .collect();
    for (n, id) in ids.iter().enumerate() {
        store
            .transact(&format!("agent-{n}"), &format!("step {n}"), |refs| {
                Ok::<_, Infallible>(set("main", refs.get("main"), Some(*id)))
            })
            .unwrap();
    }
    drop(store);
    let store = Store::open(&root, Options::default()).unwrap();
    let log = store.log(3).unwrap();
    assert_eq!(
        log.iter().map(|op| op.seq).collect::<Vec<_>>(),
        vec![5, 4, 3]
    );
    assert_eq!(log[0].description, "step 4");
    assert_eq!(log[0].updates, set("main", Some(ids[3]), Some(ids[4])));
    assert_eq!(store.log(100).unwrap().len(), 5);
    assert!(store.log(0).unwrap().is_empty());
    assert_eq!(store.refs().unwrap().get("main"), Some(ids[4]));
}

#[test]
fn undo_steps_back_one_operation_at_a_time() {
    let (_dir, store) = new_store(Options::default());
    let ids: Vec<ObjectId> = (0..3u8)
        .map(|n| store.put(Kind::Changeset, &[n]).unwrap())
        .collect();
    for id in &ids {
        store
            .transact("agent", "advance", |refs| {
                Ok::<_, Infallible>(set("main", refs.get("main"), Some(*id)))
            })
            .unwrap();
    }
    let undo = store.undo_last("fixer").unwrap();
    assert_eq!(undo.seq, 4);
    assert_eq!(undo.undoes, Some(3));
    assert_eq!(undo.agent, "fixer");
    assert_eq!(store.refs().unwrap().get("main"), Some(ids[1]));
    assert_eq!(store.undo_last("fixer").unwrap().undoes, Some(2));
    assert_eq!(store.refs().unwrap().get("main"), Some(ids[0]));
    assert_eq!(store.undo_last("fixer").unwrap().undoes, Some(1));
    assert_eq!(store.refs().unwrap().get("main"), None);
    assert!(matches!(
        store.undo_last("fixer"),
        Err(Error::NothingToUndo)
    ));

    // A new operation can be undone; one whose ref moved since cannot.
    store
        .transact("agent", "again", |refs| {
            Ok::<_, Infallible>(set("main", refs.get("main"), Some(ids[2])))
        })
        .unwrap();
    store
        .transact("agent", "other", |refs| {
            Ok::<_, Infallible>(set("other", refs.get("other"), Some(ids[0])))
        })
        .unwrap();
    assert_eq!(store.undo_last("fixer").unwrap().undoes, Some(8));
    assert_eq!(store.refs().unwrap().get("other"), None);
    store
        .transact("agent", "move main by hand", |refs| {
            Ok::<_, Infallible>(set("main", refs.get("main"), Some(ids[1])))
        })
        .unwrap();
    // Undoing "move main by hand" works; undoing "again" after it does too.
    assert_eq!(store.undo_last("fixer").unwrap().undoes, Some(10));
    assert_eq!(store.undo_last("fixer").unwrap().undoes, Some(7));
    assert_eq!(store.refs().unwrap().get("main"), None);
    assert_clean(&store);
}

#[test]
fn undo_reverts_every_ref_an_operation_changed() {
    let (_dir, store) = new_store(Options::default());
    let a = store.put(Kind::Changeset, b"a").unwrap();
    let b = store.put(Kind::Changeset, b"b").unwrap();
    store
        .transact("agent", "start", |_| {
            Ok::<_, Infallible>(vec![
                RefUpdate::new("main", None, Some(a)),
                RefUpdate::new("old", None, Some(b)),
            ])
        })
        .unwrap();
    store
        .transact("agent", "reshape", |refs| {
            Ok::<_, Infallible>(vec![
                RefUpdate::new("main", refs.get("main"), Some(b)),
                RefUpdate::new("old", refs.get("old"), None),
                RefUpdate::new("new", None, Some(a)),
            ])
        })
        .unwrap();
    let undo = store.undo_last("fixer").unwrap();
    assert_eq!(undo.undoes, Some(2));
    assert_eq!(undo.description, "undo operation 2");
    assert_eq!(
        undo.updates,
        vec![
            RefUpdate::new("main", Some(b), Some(a)),
            RefUpdate::new("old", None, Some(b)),
            RefUpdate::new("new", Some(a), None),
        ]
    );
    let refs = store.refs().unwrap();
    assert_eq!(refs.get("main"), Some(a));
    assert_eq!(refs.get("old"), Some(b));
    assert_eq!(refs.get("new"), None);

    // An operation that changed nothing is still an operation.
    store
        .transact("agent", "nothing", |_| Ok::<_, Infallible>(Vec::new()))
        .unwrap();
    assert_eq!(store.undo_last("fixer").unwrap().undoes, Some(4));
    assert_eq!(store.undo_last("fixer").unwrap().undoes, Some(1));
    assert!(store.refs().unwrap().is_empty());
    assert!(matches!(
        store.undo_last("fixer"),
        Err(Error::NothingToUndo)
    ));
    let log = store.log(usize::MAX).unwrap();
    assert_eq!(
        log.iter().map(|op| (op.seq, op.undoes)).collect::<Vec<_>>(),
        vec![
            (6, Some(1)),
            (5, Some(4)),
            (4, None),
            (3, Some(2)),
            (2, None),
            (1, None)
        ]
    );
    assert_clean(&store);
}

#[test]
fn checkpoints_are_written_and_survive_damage() {
    let (_dir, store) = new_store(options(256 << 20, 3));
    let root = store.root().to_path_buf();
    let ids: Vec<ObjectId> = (0..10u8)
        .map(|n| store.put(Kind::Changeset, &[n]).unwrap())
        .collect();
    for (n, id) in ids.iter().enumerate() {
        store
            .transact("agent", "step", |refs| {
                Ok::<_, Infallible>(vec![
                    RefUpdate::new("main", refs.get("main"), Some(*id)),
                    RefUpdate::new(format!("tag/{n}"), None, Some(*id)),
                ])
            })
            .unwrap();
    }
    let mut checkpoints: Vec<String> = fs::read_dir(root.join("oplog"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("ckpt."))
        .collect();
    checkpoints.sort();
    assert_eq!(checkpoints, vec!["ckpt.6", "ckpt.9"]);
    let report = assert_clean(&store);
    assert_eq!(report.checkpoints, 2);
    drop(store);

    // A fresh handle starts from the newest checkpoint.
    let store = Store::open(&root, options(256 << 20, 3)).unwrap();
    let refs = store.refs().unwrap();
    assert_eq!(refs.seq(), 10);
    assert_eq!(refs.get("main"), Some(ids[9]));
    assert_eq!(refs.len(), 11);
    drop(store);

    // A damaged checkpoint is skipped, and reported.
    flip_byte(&root.join("oplog/ckpt.9"), 40);
    let store = Store::open(&root, options(256 << 20, 3)).unwrap();
    let refs = store.refs().unwrap();
    assert_eq!(refs.seq(), 10);
    assert_eq!(refs.get("tag/0"), Some(ids[0]));
    let report = store.fsck().unwrap();
    assert!(
        matches!(report.problems.as_slice(), [Problem::BadCheckpoint { .. }]),
        "{:#?}",
        report.problems
    );
}

#[test]
fn a_torn_log_tail_is_ignored_then_cut_off() {
    let (_dir, store) = new_store(Options::default());
    let root = store.root().to_path_buf();
    let a = store.put(Kind::Changeset, b"a").unwrap();
    let b = store.put(Kind::Changeset, b"b").unwrap();
    store
        .transact("agent", "set a", |_| {
            Ok::<_, Infallible>(set("main", None, Some(a)))
        })
        .unwrap();
    let log = root.join("oplog/log");
    let good_len = fs::metadata(&log).unwrap().len();
    // The first half of a second record, as a crash mid-append leaves it.
    store
        .transact("agent", "set b", |_| {
            Ok::<_, Infallible>(set("main", Some(a), Some(b)))
        })
        .unwrap();
    drop(store);
    let bytes = fs::read(&log).unwrap();
    let cut = good_len as usize + (bytes.len() - good_len as usize) / 2;
    fs::write(&log, &bytes[..cut]).unwrap();

    let store = Store::open(&root, Options::default()).unwrap();
    assert_eq!(store.refs().unwrap().get("main"), Some(a));
    let report = assert_clean(&store);
    assert_eq!(report.torn_log_bytes, (cut as u64) - good_len);
    assert_eq!(report.operations, 1);
    // The next writer cuts the torn bytes off and appends in their place.
    store
        .transact("agent", "set b again", |refs| {
            Ok::<_, Infallible>(set("main", refs.get("main"), Some(b)))
        })
        .unwrap();
    let report = assert_clean(&store);
    assert_eq!(report.torn_log_bytes, 0);
    assert_eq!(report.operations, 2);
    assert_eq!(store.log(10).unwrap()[0].seq, 2);
}

#[test]
fn damage_in_the_middle_of_the_log_is_never_cut_off() {
    let (_dir, store) = new_store(Options::default());
    let root = store.root().to_path_buf();
    let ids: Vec<ObjectId> = (0..3u8)
        .map(|n| store.put(Kind::Changeset, &[n]).unwrap())
        .collect();
    for id in &ids {
        store
            .transact("agent", "advance", |refs| {
                Ok::<_, Infallible>(set("main", refs.get("main"), Some(*id)))
            })
            .unwrap();
    }
    drop(store);
    let log = root.join("oplog/log");
    flip_byte(&log, 20);
    let before = fs::read(&log).unwrap();

    let store = Store::open(&root, options(256 << 20, 0)).unwrap();
    assert!(matches!(store.refs(), Err(Error::Corrupt { .. })));
    let err = store
        .transact("agent", "advance", |_| Ok::<_, Infallible>(Vec::new()))
        .unwrap_err();
    assert!(
        matches!(err, TransactError::Store(Error::Corrupt { .. })),
        "{err}"
    );
    assert_eq!(fs::read(&log).unwrap(), before);
    let report = store.fsck().unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| matches!(p, Problem::BadOperation { offset: 0, .. })),
        "{:#?}",
        report.problems
    );
}

#[test]
fn fsck_reports_refs_to_missing_objects() {
    let (_dir, store) = new_store(Options::default());
    let root = store.root().to_path_buf();
    let id = store.put(Kind::Changeset, b"soon gone").unwrap();
    store
        .transact("agent", "set", |_| {
            Ok::<_, Infallible>(set("main", None, Some(id)))
        })
        .unwrap();
    drop(store);
    for entry in fs::read_dir(root.join("seg")).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    let store = Store::open(&root, Options::default()).unwrap();
    let report = store.fsck().unwrap();
    assert_eq!(
        report.problems,
        vec![Problem::MissingObject {
            name: "main".to_owned(),
            id
        }]
    );
}

#[test]
fn full_durability_works_the_same() {
    let options = Options {
        durability: Durability::Full,
        segment_size: 2048,
        checkpoint_interval: 2,
    };
    let (_dir, store) = new_store(options.clone());
    let root = store.root().to_path_buf();
    let objects = payloads(10);
    let mut last = None;
    for (kind, payload) in &objects {
        let id = store.put(*kind, payload).unwrap();
        store.sync().unwrap();
        store
            .transact("agent", "advance", |refs| {
                Ok::<_, Infallible>(set("main", refs.get("main"), Some(id)))
            })
            .unwrap();
        last = Some(id);
    }
    drop(store);
    let store = Store::open(&root, options).unwrap();
    assert_eq!(store.refs().unwrap().get("main"), last);
    let report = assert_clean(&store);
    assert_eq!(report.operations, 10);
}

#[test]
fn limits_are_enforced() {
    let (_dir, store) = new_store(Options::default());
    let err = store
        .transact(&"a".repeat(2000), "x", |_| Ok::<_, Infallible>(Vec::new()))
        .unwrap_err();
    assert!(matches!(err, TransactError::Store(Error::TooLong { .. })));
    let err = store
        .transact(
            "a",
            &"d".repeat(70_000),
            |_| Ok::<_, Infallible>(Vec::new()),
        )
        .unwrap_err();
    assert!(matches!(err, TransactError::Store(Error::TooLong { .. })));
    assert_eq!(store.refs().unwrap().seq(), 0);
}
