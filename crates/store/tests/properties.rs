// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Random sequences of operations, compared with simple reference models:
//! objects with `MemoryObjects`, the op log with a map and a list.

use std::collections::{BTreeMap, HashSet};
use std::convert::Infallible;

use braid_format::MemoryObjects;
use braid_store::{Error, Kind, ObjectId, Objects, Options, RefUpdate, Store};
use proptest::prelude::*;

/// Cases per property: a few by default, many with `BRAID_STRESS=1`. Every
/// case flushes to disk, so the default stays small.
fn cases() -> u32 {
    if std::env::var_os("BRAID_STRESS").is_some_and(|value| value != "0") {
        256
    } else {
        12
    }
}

#[derive(Clone, Debug)]
enum ObjectOp {
    Put(Kind, Vec<u8>),
    /// Reads one of the objects stored so far, picked by this number.
    Get(usize),
    /// Reads an object that may not exist.
    Probe(Kind, Vec<u8>),
    Sync,
    Reopen,
}

fn kind() -> impl Strategy<Value = Kind> {
    prop::sample::select(Kind::ALL.to_vec())
}

fn payload() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // Small: stored raw.
        prop::collection::vec(any::<u8>(), 0..80),
        // Repetitive: compressed.
        (prop::collection::vec(any::<u8>(), 1..12), 8usize..400)
            .prop_map(|(unit, times)| unit.repeat(times)),
        // Noise: stored raw because it does not compress.
        prop::collection::vec(any::<u8>(), 65..1500),
    ]
}

fn object_op() -> impl Strategy<Value = ObjectOp> {
    prop_oneof![
        8 => (kind(), payload()).prop_map(|(kind, payload)| ObjectOp::Put(kind, payload)),
        4 => any::<usize>().prop_map(ObjectOp::Get),
        1 => (kind(), payload()).prop_map(|(kind, payload)| ObjectOp::Probe(kind, payload)),
        1 => Just(ObjectOp::Sync),
        1 => Just(ObjectOp::Reopen),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Objects behave as in memory, across reopening and sealing.
    #[test]
    fn objects_behave_like_memory(
        ops in prop::collection::vec(object_op(), 1..80),
        segment_size in 64u64..6000,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let options = Options { segment_size, ..Options::default() };
        let mut store = Store::init(dir.path(), options.clone()).unwrap();
        let model = MemoryObjects::new();
        let mut stored = Vec::new();
        for op in ops {
            match op {
                ObjectOp::Put(kind, payload) => {
                    let id = store.put(kind, &payload).unwrap();
                    prop_assert_eq!(id, model.put(kind, &payload).unwrap());
                    stored.push(id);
                }
                ObjectOp::Get(pick) => {
                    if !stored.is_empty() {
                        let id = stored[pick % stored.len()];
                        prop_assert_eq!(store.get(&id).unwrap(), model.get(&id).unwrap());
                        prop_assert_eq!(store.stat(&id).unwrap(), model.stat(&id).unwrap());
                    }
                }
                ObjectOp::Probe(kind, payload) => {
                    let id = ObjectId::hash(kind, &payload);
                    prop_assert_eq!(store.get(&id).unwrap(), model.get(&id).unwrap());
                    prop_assert_eq!(store.contains(&id).unwrap(), model.contains(&id).unwrap());
                    prop_assert_eq!(store.stat(&id).unwrap(), model.stat(&id).unwrap());
                }
                ObjectOp::Sync => store.sync().unwrap(),
                ObjectOp::Reopen => {
                    drop(store);
                    store = Store::open(dir.path(), options.clone()).unwrap();
                }
            }
        }
        store.sync().unwrap();
        drop(store);
        let store = Store::open(dir.path(), options).unwrap();
        for id in model.ids() {
            prop_assert_eq!(store.get(&id).unwrap(), model.get(&id).unwrap());
            prop_assert_eq!(store.stat(&id).unwrap(), model.stat(&id).unwrap());
        }
        let report = store.fsck().unwrap();
        prop_assert!(report.is_clean(), "{:#?}", report.problems);
        prop_assert_eq!(report.objects, model.len() as u64);
        // One handle at a time never writes an object twice.
        prop_assert_eq!(report.records, model.len() as u64);
    }
}

#[derive(Clone, Debug)]
enum LogOp {
    /// Points ref `name` at value `value`, expecting what the model holds,
    /// or a value it cannot hold if `stale`.
    Set {
        name: u8,
        value: u8,
        stale: bool,
    },
    Delete {
        name: u8,
    },
    Undo,
    Reopen,
}

fn log_op() -> impl Strategy<Value = LogOp> {
    prop_oneof![
        6 => (0u8..4, 0u8..6, prop::bool::weighted(0.15))
            .prop_map(|(name, value, stale)| LogOp::Set { name, value, stale }),
        2 => (0u8..4).prop_map(|name| LogOp::Delete { name }),
        2 => Just(LogOp::Undo),
        1 => Just(LogOp::Reopen),
    ]
}

/// The op log as a map of refs and a list of operations.
#[derive(Default)]
struct Model {
    refs: BTreeMap<String, ObjectId>,
    /// Index `i` holds operation `i + 1`: what it undid, and its updates.
    ops: Vec<(Option<u64>, Vec<RefUpdate>)>,
}

impl Model {
    fn apply(&mut self, undoes: Option<u64>, updates: Vec<RefUpdate>) {
        for update in &updates {
            match update.new {
                Some(id) => self.refs.insert(update.name.clone(), id),
                None => self.refs.remove(&update.name),
            };
        }
        self.ops.push((undoes, updates));
    }

    /// The operation an undo reverts: the newest that is neither an undo
    /// nor undone.
    fn undo_target(&self) -> Option<u64> {
        let mut undone = HashSet::new();
        for (index, (undoes, _)) in self.ops.iter().enumerate().rev() {
            let seq = index as u64 + 1;
            if let Some(target) = undoes {
                undone.insert(*target);
            } else if !undone.contains(&seq) {
                return Some(seq);
            }
        }
        None
    }
}

fn value(n: u8) -> ObjectId {
    ObjectId::hash(Kind::Changeset, &[n])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// The op log behaves like the model, across reopening and checkpoints.
    #[test]
    fn op_log_behaves_like_a_model(
        ops in prop::collection::vec(log_op(), 1..40),
        checkpoint_interval in 0u64..5,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let options = Options { checkpoint_interval, ..Options::default() };
        let mut store = Store::init(dir.path(), options.clone()).unwrap();
        for n in 0..6 {
            store.put(Kind::Changeset, &[n]).unwrap();
        }
        store.sync().unwrap();
        let mut model = Model::default();

        for op in ops {
            match op {
                LogOp::Set { name, value: n, stale } => {
                    let name = format!("ref/{name}");
                    let current = model.refs.get(&name).copied();
                    let old = if stale { Some(value(200)) } else { current };
                    let update = RefUpdate::new(name, old, Some(value(n)));
                    let result = store.transact("prop", "set", |_| {
                        Ok::<_, Infallible>(vec![update.clone()])
                    });
                    if stale {
                        prop_assert!(result.unwrap_err().is_conflict());
                    } else {
                        let op = result.unwrap();
                        prop_assert_eq!(op.seq, model.ops.len() as u64 + 1);
                        model.apply(None, vec![update]);
                    }
                }
                LogOp::Delete { name } => {
                    let name = format!("ref/{name}");
                    let update = RefUpdate::new(name.clone(), model.refs.get(&name).copied(), None);
                    store
                        .transact("prop", "delete", |_| Ok::<_, Infallible>(vec![update.clone()]))
                        .unwrap();
                    model.apply(None, vec![update]);
                }
                LogOp::Undo => match model.undo_target() {
                    None => prop_assert!(matches!(store.undo_last("prop"), Err(Error::NothingToUndo))),
                    Some(target) => {
                        let op = store.undo_last("prop").unwrap();
                        prop_assert_eq!(op.undoes, Some(target));
                        let reverted: Vec<RefUpdate> = model.ops[target as usize - 1]
                            .1
                            .iter()
                            .map(|update| RefUpdate::new(update.name.clone(), update.new, update.old))
                            .collect();
                        prop_assert_eq!(&op.updates, &reverted);
                        model.apply(Some(target), reverted);
                    }
                },
                LogOp::Reopen => {
                    drop(store);
                    store = Store::open(dir.path(), options.clone()).unwrap();
                }
            }
            let refs = store.refs().unwrap();
            prop_assert_eq!(refs.as_map(), &model.refs);
            prop_assert_eq!(refs.seq(), model.ops.len() as u64);
        }

        drop(store);
        let store = Store::open(dir.path(), options).unwrap();
        let log = store.log(usize::MAX).unwrap();
        prop_assert_eq!(log.len(), model.ops.len());
        for (op, seq) in log.iter().zip((1..=model.ops.len() as u64).rev()) {
            prop_assert_eq!(op.seq, seq);
            let (undoes, updates) = &model.ops[seq as usize - 1];
            prop_assert_eq!(&op.undoes, undoes);
            prop_assert_eq!(&op.updates, updates);
        }
        let report = store.fsck().unwrap();
        prop_assert!(report.is_clean(), "{:#?}", report.problems);
        prop_assert_eq!(report.operations, model.ops.len() as u64);
    }
}
