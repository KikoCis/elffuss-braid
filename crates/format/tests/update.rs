// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Updating a directory one path at a time gives exactly the nodes of
//! building it again from scratch, and a small edit reads and writes only a
//! few nodes, however large the directory.
//!
//! `BRAID_TREE_CASES=<n>` runs `n` random cases instead of the default
//! number, for long runs.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};

use braid_format::tree::{MAX_FLAT_ENTRIES, MAX_NODE_ENTRIES};
use braid_format::{
    Entry, EntryEdit, EntryKind, GitId, Kind, Node, NodeCache, ObjectId, TreeError, Updated,
    build_directory, find_entry, read_directory, update_directory, validate_name,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

/// Cases of the random test; `BRAID_TREE_CASES` sets another number.
const DEFAULT_CASES: u32 = 24;

fn cases() -> u32 {
    match std::env::var("BRAID_TREE_CASES") {
        Ok(value) => value
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("BRAID_TREE_CASES must be a number, not {value:?}")),
        Err(_) => DEFAULT_CASES,
    }
}

type Entries = BTreeMap<Vec<u8>, EntryKind>;

/// Nodes by id, as a store holds them.
type Store = HashMap<ObjectId, Vec<u8>>;

fn kind(salt: u64, variant: u64) -> EntryKind {
    let blob = ObjectId::hash(Kind::Blob, &salt.to_le_bytes());
    match variant % 11 {
        0 => EntryKind::Executable { blob, size: salt },
        1 => EntryKind::Symlink { blob, size: salt },
        2 => EntryKind::Tree { tree: blob },
        3 => EntryKind::Gitlink {
            commit: GitId::Sha1([salt as u8; 20]),
        },
        _ => EntryKind::File { blob, size: salt },
    }
}

/// A position among the entries as they are when it is used, as a fraction
/// of their number.
#[derive(Clone, Copy, Debug)]
struct Pos(u64);

impl Pos {
    fn of(self, len: usize) -> usize {
        ((len as u128 * u128::from(self.0)) >> 64) as usize
    }
}

fn pos() -> impl Strategy<Value = Pos> {
    any::<u64>().prop_map(Pos)
}

/// A small, deterministic generator, so that a case is described by a few
/// numbers that proptest can shrink.
fn mix(seed: u64, i: u64) -> u64 {
    let mut x = seed ^ i.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 31;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 29;
    x
}

/// The `i`th name of a directory in one of several styles: numbered like the
/// files of a generated directory, random-looking, sharing long prefixes, or
/// of mixed lengths and bytes, Unicode included.
fn name(style: u8, seed: u64, i: u64) -> Vec<u8> {
    let r = mix(seed, i);
    match style % 4 {
        0 => format!("f{i:06}.txt").into_bytes(),
        1 => format!("{r:016x}").into_bytes(),
        2 => format!("component-{}-{i:07}", r % 5).into_bytes(),
        _ => {
            const BYTES: &[u8] = b"abcXYZ019._- \xc3\xa9";
            let len = 1 + (r % 13) as usize;
            let mut name: Vec<u8> = (0..len)
                .map(|k| BYTES[(mix(r, k as u64) % BYTES.len() as u64) as usize])
                .collect();
            name.extend_from_slice(format!("{i}").as_bytes());
            // Keep it valid UTF-8 when it holds half of a two-byte letter.
            String::from_utf8_lossy(&name).into_owned().into_bytes()
        }
    }
}

/// A directory of about `count` distinct valid names.
fn directory(count: usize, style: u8, seed: u64) -> Entries {
    let mut entries = Entries::new();
    let mut i = 0u64;
    while entries.len() < count {
        let name = name(style, seed, i);
        if validate_name(&name).is_ok() {
            entries.insert(name, kind(mix(seed, i), mix(seed, i + 1)));
        }
        i += 1;
    }
    entries
}

/// One step of a batch of edits, applied to the entries as they are by then.
#[derive(Clone, Debug)]
enum Op {
    /// New content for the entry at this position.
    Modify(Pos),
    /// Remove the entry at this position.
    Delete(Pos),
    /// A new name right after the entry at this position.
    AddAfter(Pos),
    /// Remove a run of neighbours, from this position.
    DeleteRun(Pos, usize),
    /// New content for a run of neighbours.
    ModifyRun(Pos, usize),
    /// New names, one after the other, right after this position.
    AddRun(Pos, usize),
    /// The content of one entry moves to a new name next to another.
    Rename(Pos, Pos),
    /// Remove, or change, the first or last entries.
    DeleteFirst(usize),
    DeleteLast(usize),
    ModifyFirst(usize),
    ModifyLast(usize),
    /// New names before every other, or after.
    AddFirst(usize),
    AddLast(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => pos().prop_map(Op::Modify),
        3 => pos().prop_map(Op::Delete),
        3 => pos().prop_map(Op::AddAfter),
        1 => (pos(), 1usize..700).prop_map(|(i, n)| Op::DeleteRun(i, n)),
        1 => (pos(), 1usize..700).prop_map(|(i, n)| Op::ModifyRun(i, n)),
        1 => (pos(), 1usize..700).prop_map(|(i, n)| Op::AddRun(i, n)),
        2 => (pos(), pos()).prop_map(|(a, b)| Op::Rename(a, b)),
        1 => (1usize..40).prop_map(Op::DeleteFirst),
        1 => (1usize..40).prop_map(Op::DeleteLast),
        1 => (1usize..40).prop_map(Op::ModifyFirst),
        1 => (1usize..40).prop_map(Op::ModifyLast),
        1 => (1usize..40).prop_map(Op::AddFirst),
        1 => (1usize..40).prop_map(Op::AddLast),
    ]
}

/// The name at `index` among `entries`, which must not be empty.
fn nth(entries: &Entries, index: &Pos) -> Vec<u8> {
    entries
        .keys()
        .nth(index.of(entries.len()))
        .cloned()
        .expect("not empty")
}

/// A name that sorts right after `name`, before any longer one that starts
/// with it.
fn after(name: &[u8], tag: u64) -> Vec<u8> {
    let mut next = name.to_vec();
    next.push(b'\x01');
    next.extend_from_slice(format!("{tag:05}").as_bytes());
    next
}

/// Applies `ops` to a copy of `entries`.
fn edited(entries: &Entries, ops: &[Op], seed: u64) -> Entries {
    let mut new = entries.clone();
    let mut tag = 0u64;
    let fresh = |tag: &mut u64| {
        *tag += 1;
        kind(mix(seed ^ 0x5eed, *tag), mix(seed, *tag))
    };
    for op in ops {
        if new.is_empty() {
            new.insert(b"seed".to_vec(), fresh(&mut tag));
            continue;
        }
        let names: Vec<Vec<u8>> = match op {
            Op::Modify(i) | Op::Delete(i) | Op::AddAfter(i) | Op::Rename(i, _) => {
                vec![nth(&new, i)]
            }
            Op::DeleteRun(i, n) | Op::ModifyRun(i, n) | Op::AddRun(i, n) => {
                let start = i.of(new.len());
                new.keys().skip(start).take(*n).cloned().collect()
            }
            Op::DeleteFirst(n) | Op::ModifyFirst(n) | Op::AddFirst(n) => {
                new.keys().take(*n).cloned().collect()
            }
            Op::DeleteLast(n) | Op::ModifyLast(n) | Op::AddLast(n) => {
                new.keys().rev().take(*n).cloned().collect()
            }
        };
        match op {
            Op::Modify(_) | Op::ModifyRun(..) | Op::ModifyFirst(_) | Op::ModifyLast(_) => {
                for name in names {
                    new.insert(name, fresh(&mut tag));
                }
            }
            Op::Delete(_) | Op::DeleteRun(..) | Op::DeleteFirst(_) | Op::DeleteLast(_) => {
                for name in names {
                    new.remove(&name);
                }
            }
            Op::AddAfter(_) => {
                let name = after(&names[0], tag);
                new.insert(name, fresh(&mut tag));
            }
            Op::AddRun(_, n) => {
                for _ in 0..*n {
                    let name = after(&names[0], tag);
                    new.insert(name, fresh(&mut tag));
                }
            }
            Op::Rename(_, to) => {
                if let Some(content) = new.remove(&names[0])
                    && !new.is_empty()
                {
                    let next = after(&nth(&new, to), tag);
                    tag += 1;
                    new.insert(next, content);
                }
            }
            Op::AddFirst(n) => {
                for k in 0..*n {
                    new.insert(format!("\u{1}first{k:03}").into_bytes(), fresh(&mut tag));
                }
            }
            Op::AddLast(n) => {
                for k in 0..*n {
                    new.insert(format!("\u{10ffff}{k:03}").into_bytes(), fresh(&mut tag));
                }
            }
        }
    }
    new
}

/// The edits that turn `old` into `new`, with a few that change nothing:
/// names set to the entry they have, and removals of names that are absent.
fn edits(old: &Entries, new: &Entries, idle: &[Pos]) -> Vec<EntryEdit> {
    let mut edits: BTreeMap<Vec<u8>, Option<EntryKind>> = BTreeMap::new();
    for (name, kind) in new {
        if old.get(name) != Some(kind) {
            edits.insert(name.clone(), Some(kind.clone()));
        }
    }
    for name in old.keys() {
        if !new.contains_key(name) {
            edits.insert(name.clone(), None);
        }
    }
    for (k, index) in idle.iter().enumerate() {
        if new.is_empty() {
            break;
        }
        let name = nth(new, index);
        if k % 2 == 0 {
            edits
                .entry(name.clone())
                .or_insert_with(|| Some(new[&name].clone()));
        } else {
            edits.entry(after(&name, 99_999)).or_insert(None);
        }
    }
    edits
        .into_iter()
        .map(|(name, kind)| EntryEdit { name, kind })
        .collect()
}

fn as_entries(entries: &Entries) -> Vec<Entry> {
    entries
        .iter()
        .map(|(name, kind)| Entry {
            name: name.clone(),
            kind: kind.clone(),
        })
        .collect()
}

/// Builds `entries` from scratch, and returns the root and the store of its
/// nodes.
fn stored(entries: &Entries) -> (ObjectId, Store) {
    let directory = build_directory(as_entries(entries)).unwrap();
    (directory.root(), directory.nodes.into_iter().collect())
}

/// Updates the directory `root` of `store` with `edits`, and counts the
/// nodes it fetches.
fn update(store: &Store, root: ObjectId, edits: &[EntryEdit]) -> (Updated, usize) {
    let fetched = Cell::new(0);
    let mut cache = NodeCache::new();
    let updated = update_directory(&mut cache, root, edits, |id| {
        fetched.set(fetched.get() + 1);
        store.get(id).cloned()
    })
    .unwrap();
    (updated, fetched.get())
}

/// Checks an update against building the new entries from scratch: the same
/// root, the same nodes byte for byte, none missing and none extra, every
/// node written after its children, and hints that name old nodes.
fn check_update(old_store: &Store, updated: &Updated, new: &Entries) -> Result<(), TestCaseError> {
    let expected = build_directory(as_entries(new)).unwrap();
    prop_assert_eq!(updated.root, expected.root());
    prop_assert_eq!(updated.entries, new.len() as u64);

    let expected_nodes: Store = expected.nodes.iter().cloned().collect();
    let mut written = HashSet::new();
    for node in &updated.nodes {
        prop_assert_eq!(
            expected_nodes.get(&node.id),
            Some(&node.bytes),
            "a node written is not one of the new directory"
        );
        prop_assert!(
            !old_store.contains_key(&node.id),
            "an old node is written again"
        );
        prop_assert!(written.insert(node.id), "a node is written twice");
        if let Node::Internal { children, .. } = Node::decode(&node.bytes).unwrap() {
            for child in children {
                prop_assert!(
                    written.contains(&child.node) || old_store.contains_key(&child.node),
                    "a node is written before its children"
                );
            }
        }
        if let Some(similar) = node.similar {
            prop_assert!(old_store.contains_key(&similar), "a hint names no old node");
            prop_assert_ne!(similar, node.id);
        }
    }
    for (id, bytes) in &expected.nodes {
        prop_assert!(
            written.contains(id) || old_store.get(id) == Some(bytes),
            "a node of the new directory is neither written nor kept"
        );
    }

    // Read back through the old nodes and the new ones.
    let mut store = old_store.clone();
    store.extend(updated.nodes.iter().map(|n| (n.id, n.bytes.clone())));
    prop_assert_eq!(
        read_directory(updated.root, |id| store.get(id).cloned()).unwrap(),
        as_entries(new)
    );
    Ok(())
}

/// Directories of 1,000 to 20,000 entries, sometimes just around the limit
/// of a flat node.
fn shape() -> impl Strategy<Value = (usize, u8, u64)> {
    (
        prop_oneof![
            3 => 1_000usize..20_000,
            1 => (MAX_FLAT_ENTRIES - 24)..(MAX_FLAT_ENTRIES + 40),
        ],
        any::<u8>(),
        any::<u64>(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn updates_build_what_building_from_scratch_builds(
        (count, style, seed) in shape(),
        ops in prop::collection::vec(op(), 1..12),
        idle in prop::collection::vec(pos(), 0..3),
    ) {
        let old = directory(count, style, seed);
        let new = edited(&old, &ops, seed);
        let edits = edits(&old, &new, &idle);
        let (root, store) = stored(&old);
        let (updated, _) = update(&store, root, &edits);
        check_update(&store, &updated, &new)?;
    }

    #[test]
    fn lookups_find_what_the_directory_holds(
        (count, style, seed) in shape(),
        probes in prop::collection::vec(pos(), 1..40),
    ) {
        let entries = directory(count, style, seed);
        let (root, store) = stored(&entries);
        let mut cache = NodeCache::new();
        let mut fetch = |id: &ObjectId| store.get(id).cloned();
        for probe in &probes {
            let name = nth(&entries, probe);
            let found = find_entry(&mut cache, root, &name, &mut fetch).unwrap();
            prop_assert_eq!(found.as_ref(), entries.get(&name));
            let absent = after(&name, 99_999);
            prop_assert_eq!(find_entry(&mut cache, root, &absent, &mut fetch).unwrap(), None);
        }
        for name in [&b"\x01"[..], "\u{10ffff}".as_bytes()] {
            prop_assert_eq!(find_entry(&mut cache, root, name, &mut fetch).unwrap(), None);
        }
    }
}

/// Edits across the limit of a flat node, both ways, and down to nothing.
#[test]
fn crossing_the_flat_limit_both_ways() {
    let old = directory(MAX_FLAT_ENTRIES, 0, 7);
    let (root, store) = stored(&old);
    let one_more = edited(&old, &[Op::AddLast(1)], 7);
    let (grown, _) = update(&store, root, &edits(&old, &one_more, &[]));
    check_update(&store, &grown, &one_more).unwrap();
    assert!(grown.nodes.len() >= 3, "split into leaves under a root");

    // The split directory alone, so that its old nodes are all a store has.
    let (split_root, split_store) = stored(&one_more);
    assert_eq!(split_root, grown.root);
    let (shrunk, _) = update(&split_store, split_root, &edits(&one_more, &old, &[]));
    check_update(&split_store, &shrunk, &old).unwrap();
    assert_eq!(shrunk.root, root, "flat again, as it was");

    let big = directory(5_000, 1, 3);
    let (root, store) = stored(&big);
    let (gone, _) = update(&store, root, &edits(&big, &Entries::new(), &[]));
    check_update(&store, &gone, &Entries::new()).unwrap();
    assert_eq!(gone.entries, 0);
}

/// How many levels of nodes a directory of `entries` has: 1 when it is flat.
fn levels(store: &Store, root: ObjectId) -> usize {
    match Node::decode(&store[&root]).unwrap() {
        Node::Internal { level, .. } => usize::from(level) + 1,
        _ => 1,
    }
}

/// Edits of one entry each: a new name next to one at a position, the
/// removal of that one, or new content for it; and the same at both ends.
fn single_edits(entries: &Entries) -> Vec<EntryEdit> {
    let names: Vec<&Vec<u8>> = entries.keys().collect();
    let set = |name: Vec<u8>, salt: u64| EntryEdit {
        name,
        kind: Some(kind(salt, 7)),
    };
    let mut edits = Vec::new();
    for k in 0..40u64 {
        let name = names[Pos(mix(5, k)).of(names.len())];
        edits.push(set(after(name, k), k));
        edits.push(EntryEdit {
            name: name.clone(),
            kind: None,
        });
        edits.push(set(name.clone(), k + 1_000));
    }
    let (first, last) = (names[0], names[names.len() - 1]);
    edits.push(set(b"\x01first".to_vec(), 1));
    edits.push(set("\u{10ffff}".as_bytes().to_vec(), 2));
    for name in [first, last] {
        edits.push(EntryEdit {
            name: name.clone(),
            kind: None,
        });
    }
    edits
}

/// A one-entry edit of a directory reads the nodes on its path and a few
/// neighbours, and writes about as many, whatever the size of the directory:
/// the bound depends only on the number of levels, which grows with the
/// logarithm of the size.
#[test]
fn one_entry_edits_read_and_write_a_few_nodes() {
    // Per level: the node on the path and, while the boundaries move, the
    // next ones. Measured here, one edit reads one node more than there are
    // levels at most, and writes as many as it reads.
    const READS_PER_LEVEL: usize = 2;
    const WRITES_PER_LEVEL: usize = 2;
    // Building a large directory from scratch to compare is slow in a debug
    // build: every edit is counted, one in this many is also compared.
    const COMPARED: usize = 31;
    let mut report = Vec::new();
    for count in [2_000usize, 20_000, 100_000] {
        let old = directory(count, 0, 11);
        let (root, store) = stored(&old);
        let levels = levels(&store, root);
        let (mut max_read, mut max_written, mut reads, mut writes) = (0, 0, 0, 0);
        let edits = single_edits(&old);
        for (k, edit) in edits.iter().enumerate() {
            let (updated, read) = update(&store, root, std::slice::from_ref(edit));
            if k % COMPARED == 0 || k + 4 >= edits.len() {
                let mut new = old.clone();
                match &edit.kind {
                    Some(kind) => new.insert(edit.name.clone(), kind.clone()),
                    None => new.remove(&edit.name),
                };
                check_update(&store, &updated, &new).unwrap();
            }
            max_read = max_read.max(read);
            max_written = max_written.max(updated.nodes.len());
            reads += read;
            writes += updated.nodes.len();
        }
        assert!(
            max_read <= READS_PER_LEVEL * levels,
            "{count} entries, {levels} levels: an edit read {max_read} of {} nodes",
            store.len()
        );
        assert!(
            max_written <= WRITES_PER_LEVEL * levels,
            "{count} entries, {levels} levels: an edit wrote {max_written} nodes"
        );
        report.push(Work {
            entries: count,
            levels,
            nodes: store.len(),
            most_read: max_read,
            most_written: max_written,
            reads,
            writes,
            edits: edits.len(),
        });
    }
    // Beyond the levels, the size of the directory makes no difference: the
    // average edit of the largest directory reads no more per level than one
    // of the smallest, give or take one node.
    let per_level = |work: &Work| work.reads as f64 / (work.edits * work.levels) as f64;
    let (small, large) = (per_level(&report[0]), per_level(&report[2]));
    assert!(large <= small + 1.0, "{report:#?}");
    println!("{report:#?}");
}

/// What the edits of one directory read and wrote.
#[derive(Debug)]
#[allow(dead_code)] // Printed.
struct Work {
    entries: usize,
    levels: usize,
    nodes: usize,
    most_read: usize,
    most_written: usize,
    reads: usize,
    writes: usize,
    edits: usize,
}

#[test]
fn invalid_edits_are_refused_before_anything_is_read() {
    let entries = directory(3_000, 0, 1);
    let (root, store) = stored(&entries);
    let file = kind(1, 5);
    // Edits, and what the error must be.
    type Case = (Vec<EntryEdit>, fn(&TreeError) -> bool);
    let cases: [Case; 3] = [
        (
            vec![
                EntryEdit {
                    name: b"b".to_vec(),
                    kind: Some(file.clone()),
                },
                EntryEdit {
                    name: b"a".to_vec(),
                    kind: None,
                },
            ],
            |error| matches!(error, TreeError::Unsorted { index: 1 }),
        ),
        (
            vec![EntryEdit {
                name: b".git".to_vec(),
                kind: Some(file.clone()),
            }],
            |error| matches!(error, TreeError::Name { .. }),
        ),
        (
            vec![EntryEdit {
                name: b".gitmodules".to_vec(),
                kind: Some(EntryKind::Symlink {
                    blob: ObjectId::hash(Kind::Blob, b"x"),
                    size: 1,
                }),
            }],
            |error| matches!(error, TreeError::SymlinkName(_)),
        ),
    ];
    for (edits, expected) in cases {
        let fetched = Cell::new(0);
        let result = update_directory(&mut NodeCache::new(), root, &edits, |id| {
            fetched.set(fetched.get() + 1);
            store.get(id).cloned()
        });
        let error = result.unwrap_err();
        assert!(expected(&error), "{error:?}");
        assert_eq!(fetched.get(), 0);
    }
}

/// The nodes on the way to an edit are checked against the references that
/// lead to them, and the boundaries the edit reads against the splitting
/// rule.
#[test]
fn damaged_directories_are_refused() {
    let entries = directory(3_000, 0, 2);
    let (root, store) = stored(&entries);
    let edit = |name: &[u8]| {
        vec![EntryEdit {
            name: name.to_vec(),
            kind: Some(kind(9, 9)),
        }]
    };
    let first = entries.keys().next().unwrap().clone();

    // A leaf that is missing.
    let Node::Internal { children, level } = Node::decode(&store[&root]).unwrap() else {
        panic!("a split directory");
    };
    assert_eq!(level, 1);
    let mut missing = store.clone();
    missing.remove(&children[0].node);
    let result = update_directory(&mut NodeCache::new(), root, &edit(&first), |id| {
        missing.get(id).cloned()
    });
    assert_eq!(result, Err(TreeError::MissingNode(children[0].node)));

    // A root whose first reference gives a wrong count of entries.
    let mut wrong = children.clone();
    wrong[0].entry_count += 1;
    let forged = Node::Internal {
        level,
        children: wrong,
    };
    let mut forged_store = store.clone();
    forged_store.insert(forged.id(), forged.encode());
    let result = update_directory(&mut NodeCache::new(), forged.id(), &edit(&first), |id| {
        forged_store.get(id).cloned()
    });
    assert_eq!(result, Err(TreeError::NotCanonical));
    let result = find_entry(&mut NodeCache::new(), forged.id(), &first, |id| {
        forged_store.get(id).cloned()
    });
    assert_eq!(result, Err(TreeError::NotCanonical));

    // Two leaves cut at the wrong place: the first one ends early.
    let Node::Leaf(left) = Node::decode(&store[&children[0].node]).unwrap() else {
        panic!("a leaf");
    };
    let Node::Leaf(right) = Node::decode(&store[&children[1].node]).unwrap() else {
        panic!("a leaf");
    };
    let cut = left.len() - 1;
    let early = Node::Leaf(left[..cut].to_vec());
    let mut moved = vec![left[cut].clone()];
    moved.extend(right.iter().cloned());
    assert!(moved.len() <= MAX_NODE_ENTRIES);
    let late = Node::Leaf(moved);
    let mut recut = children.clone();
    recut[0] = braid_format::tree::Child {
        last_name: left[cut - 1].name.clone(),
        node: early.id(),
        entry_count: cut as u64,
    };
    recut[1] = braid_format::tree::Child {
        last_name: children[1].last_name.clone(),
        node: late.id(),
        entry_count: children[1].entry_count + 1,
    };
    let bad_root = Node::Internal {
        level,
        children: recut,
    };
    let mut bad_store = store.clone();
    for node in [&early, &late, &bad_root] {
        bad_store.insert(node.id(), node.encode());
    }
    let result = update_directory(&mut NodeCache::new(), bad_root.id(), &edit(&first), |id| {
        bad_store.get(id).cloned()
    });
    assert_eq!(result, Err(TreeError::NotCanonical));
}
