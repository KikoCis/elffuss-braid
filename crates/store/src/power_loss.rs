// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Power lost between flushes, simulated.
//!
//! A test watches every batch of flushes the store makes on its thread, and
//! keeps a model of what a power loss could leave on the disk at any point:
//! a file holds what it held when a batch last flushed it; a name created,
//! renamed or removed in a directory stays that way once the directory is
//! flushed; and the op log, the one file whose newest bytes could do harm,
//! holds everything written to it so far, flushed or not. Right before every
//! batch, the moment a crash between two flushes would cut the work short,
//! the model is written out as a store of its own, and that store must be
//! consistent: every ref points at an object it can read whole, with what
//! that object needs, and fsck finds no ref to a missing object.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use braid_format::{Kind, ObjectId, Objects};

use crate::fsutil::observe::{self, When};
use crate::{Durability, Error, Options, Problem, RefUpdate, Store};

/// What a power loss could leave of each file, by path.
#[derive(Clone, Debug, Default)]
struct Disk {
    files: BTreeMap<PathBuf, Vec<u8>>,
}

/// The model, and the stores it says a crash could leave.
#[derive(Debug, Default)]
struct Model {
    disk: Disk,
    /// What the disk could hold right before each batch.
    crashes: Vec<Disk>,
}

impl Model {
    fn before(&mut self, root: &Path) {
        let mut disk = self.disk.clone();
        // The log may hold its newest record although nobody flushed it.
        let log = root.join("oplog").join("log");
        if let Ok(bytes) = fs::read(&log) {
            disk.files.insert(log, bytes);
        }
        self.crashes.push(disk);
    }

    fn after(&mut self, paths: &[PathBuf]) {
        for path in paths {
            if path.is_dir() {
                self.flush_dir(path);
            } else if let Ok(bytes) = fs::read(path) {
                self.disk.files.insert(path.clone(), bytes);
            }
        }
    }

    /// A flushed directory keeps the names it has now: files removed from
    /// it are gone, and files renamed into it are there with what they
    /// hold, which was flushed before the rename. Other new names are there
    /// with what their own flushes made durable, nothing so far.
    fn flush_dir(&mut self, dir: &Path) {
        self.disk
            .files
            .retain(|path, _| path.parent() != Some(dir) || path.exists());
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() || self.disk.files.contains_key(&path) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let published = name == "format"
                || name.starts_with("ckpt.")
                || name.ends_with(".idx")
                || name.ends_with(".midx");
            let bytes = if published {
                fs::read(&path).unwrap_or_default()
            } else {
                Vec::new()
            };
            self.disk.files.insert(path, bytes);
        }
    }
}

/// Writes `disk`, whose paths are under `root`, as a store under `to`.
fn materialize(disk: &Disk, root: &Path, to: &Path) {
    for dir in ["seg", "oplog"] {
        fs::create_dir_all(to.join(dir)).unwrap();
    }
    for (path, bytes) in &disk.files {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        if relative
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("tmp-"))
        {
            continue;
        }
        let target = to.join(relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, bytes).unwrap();
    }
}

/// Text that compresses, with a few bytes that differ from one version to
/// the next, so that versions become deltas.
fn text(version: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for line in 0..120u64 {
        let value = line.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (line * version / 7);
        out.extend_from_slice(format!("line {line}: {value:016x}\n").as_bytes());
    }
    out
}

fn run(durability: Durability) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let options = Options {
        durability,
        segment_size: 4096,
        segment_records: 6,
        merge_threshold: 3,
        checkpoint_interval: 4,
    };
    let model = Rc::new(RefCell::new(Model::default()));
    let hook_model = Rc::clone(&model);
    let hook_root = root.clone();
    let watching = observe::watch(move |when, paths| {
        let mut model = hook_model.borrow_mut();
        match when {
            When::Before => model.before(&hook_root),
            When::After => model.after(paths),
        }
    });

    let store = Store::init(&root, options.clone()).unwrap();
    // A second writer, whose copies the first relies on without them being
    // flushed by their writer.
    let other = Store::open(&root, options.clone()).unwrap();
    let mut previous: Option<ObjectId> = None;
    for round in 0..24u64 {
        // Written before the transaction, flushed before it takes the lock.
        let before = store
            .put(Kind::Changeset, format!("round {round}").as_bytes())
            .unwrap();
        // Written by another writer that never flushes: the transaction
        // finds it in that writer's open segment, and must flush that.
        let shared = format!("written by the other writer in round {round}").repeat(3);
        let theirs = other.put(Kind::Tree, shared.as_bytes()).unwrap();
        if round % 2 == 1 {
            store.rescan().unwrap();
        }
        store
            .transact_with("agent", |refs| {
                // Written inside, as a landing writes what it rebases: only
                // the transaction flushes it, before its record.
                let payload = text(round);
                let tip = match previous {
                    Some(similar) if round % 3 != 0 => {
                        store.put_similar(Kind::Blob, &payload, &similar)
                    }
                    _ => store.put(Kind::Blob, &payload),
                }
                .unwrap();
                previous = Some(tip);
                Ok::<_, Infallible>((
                    vec![
                        RefUpdate::new("main", refs.get("main"), Some(tip)),
                        RefUpdate::new("before", refs.get("before"), Some(before)),
                        RefUpdate::new("theirs", refs.get("theirs"), Some(theirs)),
                    ],
                    "advance".to_owned(),
                ))
            })
            .unwrap();
    }
    drop(watching);
    let report = store.fsck().unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    // The workload went through every kind of flush.
    let names: Vec<String> = fs::read_dir(root.join("seg"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|name| name.ends_with(".midx")),
        "{names:?}"
    );

    let model = model.borrow();
    assert!(model.crashes.len() > 50, "{}", model.crashes.len());
    let mut moved = HashMap::new();
    for (at, disk) in model.crashes.iter().enumerate() {
        let crashed = tempfile::tempdir().unwrap();
        let copy = crashed.path().join("store");
        materialize(disk, &root, &copy);
        let store = match Store::open(&copy, options.clone()) {
            Ok(store) => store,
            // Before the store was created: nothing to keep.
            Err(Error::NotAStore { .. }) => continue,
            Err(err) => panic!("{durability:?}, crash before batch {at}: {err}"),
        };
        let refs = store.refs().unwrap();
        for (name, id) in refs.iter() {
            let found = store.get(&id);
            assert!(
                matches!(found, Ok(Some(_))),
                "{durability:?}, crash before batch {at}: ref {name} points at {id}, which reads {found:?}"
            );
        }
        let report = store.fsck().unwrap();
        assert!(
            !report
                .problems
                .iter()
                .any(|problem| matches!(problem, Problem::MissingObject { .. })),
            "{durability:?}, crash before batch {at}: {:#?}",
            report.problems
        );
        moved.insert(refs.seq(), at);
    }
    // Crashes cut the history at many points.
    assert!(moved.len() > 10, "{moved:?}");
}

#[test]
fn a_crash_between_flushes_never_leaves_a_ref_to_a_lost_object() {
    run(Durability::Ordered);
    run(Durability::Full);
}
