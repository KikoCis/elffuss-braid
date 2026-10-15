// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Property tests of the history queries on random histories: the ancestry
//! of [`History`], with generation numbers and skip pointers, answers what
//! the walking functions answer, on histories with octopus merges and
//! criss-cross merges; skip pointers find every first-parent ancestor; and
//! derived records are the same whatever order they are computed in.

use std::collections::BTreeSet;

use braid_engine::{
    DerivedIndex, History, MemoryIndex, commit, first_parent_chain_contains, is_ancestor,
    merge_base, merge_bases, read_changeset, skip_height,
};
use braid_format::{Change, ChangeOp, Kind, MemoryObjects, ObjectId, Objects, Signature};
use proptest::prelude::*;
use proptest::sample::Index;

/// How the next changesets of a random history are made.
#[derive(Clone, Debug)]
enum Step {
    /// A changeset without parents.
    Root,
    /// A changeset on one parent.
    Child(Index),
    /// A merge of two to five parents, which may repeat.
    Merge(Vec<Index>),
    /// Two merges of the same two parents, in both orders.
    CrissCross(Index, Index),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        1 => Just(Step::Root),
        6 => any::<Index>().prop_map(Step::Child),
        3 => proptest::collection::vec(any::<Index>(), 2..=5).prop_map(Step::Merge),
        2 => (any::<Index>(), any::<Index>()).prop_map(|(a, b)| Step::CrissCross(a, b)),
    ]
}

fn signature() -> Signature {
    Signature {
        name: b"Ada".to_vec(),
        email: b"ada@example.org".to_vec(),
        seconds: 1_700_000_000,
        offset_minutes: 0,
    }
}

/// A changeset with these parents that adds a file of its own, and changes
/// a shared one every third time.
fn node(objects: &MemoryObjects, parents: Vec<ObjectId>, n: usize) -> ObjectId {
    let mut changes = Vec::new();
    let mut set = |path: String, content: String| {
        let blob = objects.put(Kind::Blob, content.as_bytes()).unwrap();
        changes.push(Change {
            path: path.into_bytes(),
            op: ChangeOp::File {
                blob,
                size: content.len() as u64,
            },
        });
    };
    set(format!("n/{n}"), format!("{n}\n"));
    if n.is_multiple_of(3) {
        set("shared".to_owned(), format!("{n}\n"));
    }
    commit(
        objects,
        parents,
        changes,
        signature(),
        signature(),
        format!("{n}\n").into_bytes(),
        Vec::new(),
    )
    .unwrap()
}

/// Builds a history from `steps`; returns every changeset, oldest first.
fn build(objects: &MemoryObjects, steps: &[Step]) -> Vec<ObjectId> {
    let mut nodes: Vec<ObjectId> = Vec::new();
    for step in steps {
        let pick = |index: &Index| nodes[index.index(nodes.len())];
        let made = match step {
            _ if nodes.is_empty() => vec![node(objects, Vec::new(), 0)],
            Step::Root => vec![node(objects, Vec::new(), nodes.len())],
            Step::Child(parent) => vec![node(objects, vec![pick(parent)], nodes.len())],
            Step::Merge(parents) => {
                vec![node(
                    objects,
                    parents.iter().map(pick).collect(),
                    nodes.len(),
                )]
            }
            Step::CrissCross(a, b) => {
                let (a, b) = (pick(a), pick(b));
                let first = node(objects, vec![a, b], nodes.len());
                let second = node(objects, vec![b, a], nodes.len() + 1);
                vec![first, second]
            }
        };
        nodes.extend(made);
    }
    nodes
}

/// `head` and all its ancestors, by walking.
fn all_ancestors_of(objects: &MemoryObjects, head: ObjectId) -> BTreeSet<ObjectId> {
    let mut seen = BTreeSet::from([head]);
    let mut stack = vec![head];
    while let Some(id) = stack.pop() {
        for parent in read_changeset(objects, id).unwrap().parents {
            if seen.insert(parent) {
                stack.push(parent);
            }
        }
    }
    seen
}

/// The first-parent ancestor `steps` below `id`, by walking.
fn walk_first_parents(objects: &MemoryObjects, id: ObjectId, steps: u64) -> Option<ObjectId> {
    let mut at = id;
    for _ in 0..steps {
        at = *read_changeset(objects, at).unwrap().parents.first()?;
    }
    Some(at)
}

/// Every pair of the history's changesets, or about 120 of them spread over
/// all of it.
fn pairs(nodes: &[ObjectId]) -> Vec<(ObjectId, ObjectId)> {
    let every = (nodes.len() * nodes.len()).div_ceil(120);
    let mut pairs = Vec::new();
    for (i, a) in nodes.iter().enumerate() {
        for (j, b) in nodes.iter().enumerate() {
            if (i * nodes.len() + j) % every == (i + j) % every {
                pairs.push((*a, *b));
            }
        }
    }
    pairs
}

fn check_ancestry(
    objects: &MemoryObjects,
    history: &mut History<'_, MemoryObjects, &MemoryIndex>,
    nodes: &[ObjectId],
) -> Result<(), TestCaseError> {
    for (a, b) in pairs(nodes) {
        prop_assert_eq!(
            history.merge_bases(a, b).unwrap(),
            merge_bases(objects, a, b).unwrap(),
            "merge bases of {} and {}",
            a.short(),
            b.short()
        );
        prop_assert_eq!(
            history.merge_base(a, b).unwrap(),
            merge_base(objects, a, b).unwrap()
        );
        prop_assert_eq!(
            history.is_ancestor(a, b).unwrap(),
            is_ancestor(objects, a, b).unwrap(),
            "is {} an ancestor of {}",
            a.short(),
            b.short()
        );
        for limit in [0, 1, 3, usize::MAX] {
            prop_assert_eq!(
                history.first_parent_chain_contains(a, b, limit).unwrap(),
                first_parent_chain_contains(objects, a, b, limit).unwrap(),
                "is {} on the chain of {} within {}",
                b.short(),
                a.short(),
                limit
            );
        }
    }
    // Several at once, against every ancestor found by walking.
    for (i, head) in nodes.iter().enumerate().step_by(3) {
        let ancestors = all_ancestors_of(objects, *head);
        for wanted in [
            nodes
                .iter()
                .skip(i % 3)
                .step_by(4)
                .copied()
                .collect::<Vec<_>>(),
            nodes.iter().take(i).step_by(5).copied().collect(),
        ] {
            let expected = wanted.iter().all(|id| ancestors.contains(id));
            prop_assert_eq!(history.all_ancestors(&wanted, *head).unwrap(), expected);
        }
    }
    // Ids that are not changesets are nobody's ancestors.
    let absent = ObjectId::hash(Kind::Changeset, b"absent");
    let head = *nodes.last().unwrap();
    prop_assert!(!history.is_ancestor(absent, head).unwrap());
    prop_assert!(
        !history
            .first_parent_chain_contains(head, absent, usize::MAX)
            .unwrap()
    );
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn ancestry_with_derived_data_equals_the_walks(
        steps in proptest::collection::vec(step(), 1..24),
    ) {
        let objects = MemoryObjects::new();
        let nodes = build(&objects, &steps);
        let index = MemoryIndex::new();

        // With everything computed in this history.
        let mut history = History::new(&objects, &index);
        check_ancestry(&objects, &mut history, &nodes)?;
        history.flush().unwrap();

        // With everything read back from records.
        let mut again = History::new(&objects, &index);
        check_ancestry(&objects, &mut again, &nodes)?;
        prop_assert_eq!(again.computed(), 0);
    }

    #[test]
    fn skip_pointers_reach_every_first_parent_ancestor(
        steps in proptest::collection::vec(step(), 1..60),
    ) {
        let objects = MemoryObjects::new();
        let nodes = build(&objects, &steps);
        let index = MemoryIndex::new();
        let mut history = History::new(&objects, &index);
        for id in &nodes {
            let node = history.graph(*id).unwrap();
            for steps in 0..=node.height + 1 {
                prop_assert_eq!(
                    history.first_parent_ancestor(*id, steps).unwrap(),
                    walk_first_parents(&objects, *id, steps).filter(|_| steps <= node.height)
                );
            }
            if let Some(skip) = node.skip {
                prop_assert_eq!(history.graph(skip).unwrap().height, skip_height(node.height));
            }
            let parents = read_changeset(&objects, *id).unwrap().parents;
            let mut generation = 0;
            for parent in &parents {
                generation = generation.max(history.generation(*parent).unwrap());
            }
            prop_assert_eq!(node.generation, generation + 1);
            prop_assert_eq!(&node.parents, &parents);
        }
    }

    #[test]
    fn derived_records_do_not_depend_on_the_order_they_are_made_in(
        steps in proptest::collection::vec(step(), 1..30),
        order in any::<u64>(),
    ) {
        let objects = MemoryObjects::new();
        let nodes = build(&objects, &steps);

        // Oldest first, newest first, and in a shuffled order.
        let mut shuffled = nodes.clone();
        let mut state = order | 1;
        for i in (1..shuffled.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            shuffled.swap(i, (state % (i as u64 + 1)) as usize);
        }
        let mut orders = vec![nodes.clone(), nodes.iter().rev().copied().collect(), shuffled];
        let mut results = Vec::new();
        for order in orders.drain(..) {
            let index = MemoryIndex::new();
            let mut history = History::new(&objects, &index);
            for id in &order {
                history.graph(*id).unwrap();
            }
            history.flush().unwrap();
            results.push(index.entries());
        }
        prop_assert_eq!(&results[0], &results[1]);
        prop_assert_eq!(&results[0], &results[2]);
        // One record per changeset.
        let changesets: BTreeSet<ObjectId> = nodes.iter().copied().collect();
        prop_assert_eq!(results[0].len(), changesets.len());
    }
}

#[test]
fn skip_heights_follow_the_rule() {
    let expected = [0, 0, 0, 1, 0, 1, 4, 1, 0, 1, 8, 1, 8, 1, 12, 9, 0, 1];
    for (height, skip) in expected.iter().enumerate() {
        assert_eq!(skip_height(height as u64), *skip, "height {height}");
    }
    for height in 1..10_000u64 {
        assert!(skip_height(height) < height);
    }
    // Large heights land far away: following pointers takes few steps.
    assert_eq!(skip_height(1 << 40), 0);
    assert_eq!(skip_height((1 << 40) + 2), 1 << 40);
}

#[test]
fn a_long_chain_is_walked_in_few_records() {
    // A chain of 3000 changesets: once its graph data exists, reaching the
    // root reads a few dozen records, not thousands.
    let objects = MemoryObjects::new();
    let edit = |parents: Vec<ObjectId>, n: usize| {
        let content = format!("{n}\n");
        let blob = objects.put(Kind::Blob, content.as_bytes()).unwrap();
        let change = Change {
            path: b"f".to_vec(),
            op: ChangeOp::File {
                blob,
                size: content.len() as u64,
            },
        };
        let message = content.clone().into_bytes();
        commit(
            &objects,
            parents,
            vec![change],
            signature(),
            signature(),
            message,
            Vec::new(),
        )
        .unwrap()
    };
    let mut at = edit(Vec::new(), 0);
    let root = at;
    for n in 1..3000 {
        at = edit(vec![at], n);
    }
    let index = MemoryIndex::new();
    let mut history = History::new(&objects, &index);
    history.graph(at).unwrap();
    history.flush().unwrap();

    let counting = Counting {
        inner: &index,
        lookups: std::cell::Cell::new(0),
    };
    let mut fresh = History::new(&objects, &counting);
    assert!(fresh.is_ancestor(root, at).unwrap());
    assert!(
        fresh
            .first_parent_chain_contains(at, root, usize::MAX)
            .unwrap()
    );
    assert_eq!(fresh.merge_bases(at, root).unwrap(), [root]);
    assert_eq!(fresh.first_parent_ancestor(at, 2999).unwrap(), Some(root));
    assert!(
        counting.lookups.get() < 100,
        "{} lookups",
        counting.lookups.get()
    );
}

/// An index that counts lookups.
#[derive(Debug)]
struct Counting<'i> {
    inner: &'i MemoryIndex,
    lookups: std::cell::Cell<usize>,
}

impl DerivedIndex for Counting<'_> {
    type Error = std::convert::Infallible;

    fn find(
        &self,
        key: &braid_engine::DerivedKey,
    ) -> Result<Option<ObjectId>, std::convert::Infallible> {
        self.lookups.set(self.lookups.get() + 1);
        self.inner.find(key)
    }

    fn record(
        &self,
        entries: &[(braid_engine::DerivedKey, ObjectId)],
    ) -> Result<(), std::convert::Infallible> {
        self.inner.record(entries)
    }
}
