// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Property tests: every encoding is canonical, and a directory's nodes depend
//! only on its set of entries.

use std::collections::{BTreeMap, HashMap};

use braid_format::codec::{Reader, put_sv, put_uv};
use braid_format::tree::{MAX_FLAT_ENTRIES, MAX_NODE_ENTRIES};
use braid_format::{
    Change, ChangeOp, Changeset, ChangesetParts, Entry, EntryKind, GitId, Kind, Node, ObjectId,
    Signature, build_directory, read_directory, validate_name,
};
use proptest::prelude::*;

fn object_id() -> impl Strategy<Value = ObjectId> {
    any::<[u8; 32]>().prop_map(ObjectId::from_bytes)
}

fn git_id() -> impl Strategy<Value = GitId> {
    prop_oneof![
        any::<[u8; 20]>().prop_map(GitId::Sha1),
        any::<[u8; 32]>().prop_map(GitId::Sha256),
    ]
}

/// Names from a small alphabet, so that sets collide and sort interestingly,
/// plus arbitrary Unicode; anything invalid is filtered out.
fn name() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        "[a-c.][a-c0-2.]{0,6}".prop_map(String::into_bytes),
        "\\PC{1,6}".prop_map(String::into_bytes),
    ]
    .prop_filter("valid name", |n| validate_name(n).is_ok())
}

fn entry_kind() -> impl Strategy<Value = EntryKind> {
    prop_oneof![
        (object_id(), any::<u64>()).prop_map(|(blob, size)| EntryKind::File { blob, size }),
        (object_id(), any::<u64>()).prop_map(|(blob, size)| EntryKind::Executable { blob, size }),
        object_id().prop_map(|tree| EntryKind::Tree { tree }),
        git_id().prop_map(|commit| EntryKind::Gitlink { commit }),
    ]
}

fn entries(max: usize) -> impl Strategy<Value = Vec<Entry>> {
    prop::collection::btree_map(name(), entry_kind(), 0..max).prop_map(|map: BTreeMap<_, _>| {
        map.into_iter()
            .map(|(name, kind)| Entry { name, kind })
            .collect()
    })
}

/// Many distinct names, to exercise split directories.
fn wide_entries() -> impl Strategy<Value = Vec<Entry>> {
    (MAX_FLAT_ENTRIES - 10..6000usize, any::<u64>()).prop_map(|(n, seed)| {
        (0..n)
            .map(|i| Entry {
                name: format!("n{:x}-{i}", seed.wrapping_mul(i as u64 + 1) % 9973).into_bytes(),
                kind: EntryKind::File {
                    blob: ObjectId::hash(Kind::Blob, &i.to_le_bytes()),
                    size: i as u64,
                },
            })
            .collect()
    })
}

fn signature() -> impl Strategy<Value = Signature> {
    (
        prop::collection::vec(any::<u8>(), 0..20),
        prop::collection::vec(any::<u8>(), 0..20),
        any::<i64>(),
        any::<i64>(),
    )
        .prop_map(|(name, email, seconds, offset_minutes)| Signature {
            name,
            email,
            seconds,
            offset_minutes,
        })
}

fn change_op() -> impl Strategy<Value = ChangeOp> {
    prop_oneof![
        Just(ChangeOp::Delete),
        (object_id(), any::<u64>()).prop_map(|(blob, size)| ChangeOp::File { blob, size }),
        (object_id(), any::<u64>()).prop_map(|(blob, size)| ChangeOp::Symlink { blob, size }),
        git_id().prop_map(|commit| ChangeOp::Gitlink { commit }),
    ]
}

fn changeset_parts() -> impl Strategy<Value = ChangesetParts> {
    (
        prop::collection::vec(object_id(), 0..4),
        object_id(),
        prop::collection::btree_map(
            prop::collection::vec(name(), 1..4).prop_map(|parts| parts.join(&b'/')),
            change_op(),
            0..20,
        ),
        signature(),
        signature(),
        prop::collection::vec(any::<u8>(), 0..64),
        prop::collection::vec(
            (
                prop::collection::vec(any::<u8>(), 0..8),
                prop::collection::vec(any::<u8>(), 0..8),
            ),
            0..4,
        ),
        prop::option::of(object_id()),
    )
        .prop_map(
            |(parents, root, changes, author, committer, message, extras, git_origin)| {
                ChangesetParts {
                    parents,
                    root,
                    changes: changes
                        .into_iter()
                        .map(|(path, op)| Change { path, op })
                        .collect(),
                    author,
                    committer,
                    message,
                    extras,
                    git_origin,
                }
            },
        )
}

fn node_sizes(nodes: &[(ObjectId, Vec<u8>)]) -> Vec<usize> {
    nodes
        .iter()
        .map(|(_, bytes)| match Node::decode(bytes).unwrap() {
            Node::Flat(e) | Node::Leaf(e) => e.len(),
            Node::Internal { children, .. } => children.len(),
        })
        .collect()
}

proptest! {
    #[test]
    fn integers_round_trip(u in any::<u64>(), s in any::<i64>()) {
        let mut out = Vec::new();
        put_uv(&mut out, u);
        put_sv(&mut out, s);
        let mut reader = Reader::new(&out);
        prop_assert_eq!(reader.uv().unwrap(), u);
        prop_assert_eq!(reader.sv().unwrap(), s);
        prop_assert!(reader.finish().is_ok());
    }

    #[test]
    fn small_directories_round_trip(entries in entries(40)) {
        let dir = build_directory(entries.clone()).unwrap();
        prop_assert_eq!(dir.nodes.len(), 1);
        let (id, bytes) = &dir.nodes[0];
        let node = Node::decode(bytes).unwrap();
        prop_assert_eq!(&node.encode(), bytes);
        prop_assert_eq!(node.id(), *id);
        prop_assert_eq!(node, Node::Flat(entries));
    }

    #[test]
    fn directory_ignores_input_order(mut entries in entries(40), seed in any::<u64>()) {
        let sorted = build_directory(entries.clone()).unwrap();
        let n = entries.len().max(1);
        entries.rotate_left((seed as usize) % n);
        entries.reverse();
        prop_assert_eq!(build_directory(entries).unwrap(), sorted);
    }

    #[test]
    fn decoders_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        // Whatever the bytes, decoding either fails or round-trips exactly.
        if let Ok(node) = Node::decode(&bytes) {
            prop_assert_eq!(node.encode(), bytes.clone());
        }
        if let Ok(changeset) = Changeset::decode(&bytes) {
            prop_assert_eq!(changeset.encode(), bytes);
        }
    }

    #[test]
    fn changesets_round_trip(parts in changeset_parts()) {
        let assembled = parts.assemble().unwrap();
        let decoded = Changeset::decode(&assembled.encoded).unwrap();
        prop_assert_eq!(&decoded, &assembled.changeset);
        prop_assert_eq!(decoded.encode(), assembled.encoded);
        prop_assert_eq!(decoded.id(), assembled.id);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn split_directories_are_canonical(entries in wide_entries(), seed in any::<u64>()) {
        let mut sorted = entries.clone();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        sorted.dedup_by(|a, b| a.name == b.name);
        let dir = build_directory(sorted.clone()).unwrap();

        // Every node decodes and re-encodes to itself.
        for (id, bytes) in &dir.nodes {
            let node = Node::decode(bytes).unwrap();
            prop_assert_eq!(&node.encode(), bytes);
            prop_assert_eq!(node.id(), *id);
        }

        // Size limits: nothing above the maximum, and a split directory is
        // never stored flat.
        let sizes = node_sizes(&dir.nodes);
        prop_assert!(sizes.iter().all(|s| *s <= MAX_NODE_ENTRIES.max(MAX_FLAT_ENTRIES)));
        if sorted.len() > MAX_FLAT_ENTRIES {
            prop_assert!(dir.nodes.len() >= 3);
            prop_assert!(sizes.iter().all(|s| *s <= MAX_NODE_ENTRIES));
        }

        // Reading the nodes back gives the entries and passes the canonical check.
        let store: HashMap<ObjectId, Vec<u8>> = dir.nodes.iter().cloned().collect();
        prop_assert_eq!(read_directory(dir.root(), |id| store.get(id).cloned()).unwrap(), sorted.clone());

        // Boundaries depend on names only: new contents, same node sizes.
        let recontent: Vec<Entry> = sorted
            .iter()
            .map(|e| Entry {
                name: e.name.clone(),
                kind: EntryKind::File {
                    blob: ObjectId::hash(Kind::Blob, &seed.to_le_bytes()),
                    size: seed,
                },
            })
            .collect();
        prop_assert_eq!(node_sizes(&build_directory(recontent).unwrap().nodes), sizes.clone());

        // Removing one entry keeps every leaf that ends before it: the scan up
        // to there is unchanged, so an edit never rewrites earlier leaves.
        if sorted.len() > MAX_FLAT_ENTRIES + 1 {
            let victim = (seed as usize) % sorted.len();
            let mut smaller = sorted.clone();
            let removed = smaller.remove(victim);
            let after_leaves = leaves(&build_directory(smaller).unwrap().nodes);
            for (last, id) in leaves(&dir.nodes) {
                if last < removed.name {
                    prop_assert!(after_leaves.iter().any(|(_, a)| *a == id));
                }
            }
        }
    }
}

fn leaves(nodes: &[(ObjectId, Vec<u8>)]) -> Vec<(Vec<u8>, ObjectId)> {
    nodes
        .iter()
        .filter_map(|(id, bytes)| match Node::decode(bytes).unwrap() {
            Node::Leaf(e) => Some((e.last().unwrap().name.clone(), *id)),
            _ => None,
        })
        .collect()
}
