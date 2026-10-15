// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Ancestry: first-parent chains and merge bases.
//!
//! These walk history one changeset at a time, so they cost time proportional
//! to the part of history they cover. Generation numbers and skip pointers,
//! stored as derived data, will make them logarithmic.

use std::collections::{HashMap, HashSet, VecDeque};

use braid_format::{ObjectId, Objects};

use crate::access::read_changeset;
use crate::error::Error;

/// Whether `base` is `head` or one of its first-parent ancestors no more than
/// `limit` steps away. Walking stops at the limit, so `false` also means "not
/// within `limit` steps".
pub fn first_parent_chain_contains<O: Objects>(
    objects: &O,
    head: ObjectId,
    base: ObjectId,
    limit: usize,
) -> Result<bool, Error> {
    let mut current = head;
    let mut steps = 0;
    loop {
        if current == base {
            return Ok(true);
        }
        if steps == limit {
            return Ok(false);
        }
        match read_changeset(objects, current)?.parents.first() {
            Some(parent) => current = *parent,
            None => return Ok(false),
        }
        steps += 1;
    }
}

/// The best common ancestors of `a` and `b`: the common ancestors (a
/// changeset counts as its own ancestor) that are not ancestors of another
/// common ancestor. There are several after criss-cross merges, and none when
/// the histories are unrelated. Sorted by id.
pub fn merge_bases<O: Objects>(
    objects: &O,
    a: ObjectId,
    b: ObjectId,
) -> Result<Vec<ObjectId>, Error> {
    let mut graph = Graph {
        objects,
        parents: HashMap::new(),
    };
    let from_a = graph.ancestors(vec![a])?;

    // Walk back from `b`, stopping on each path at the first common ancestor.
    // Every common ancestor is one of these or an ancestor of one of them.
    let mut candidates = Vec::new();
    let mut seen = HashSet::from([b]);
    let mut queue = VecDeque::from([b]);
    while let Some(id) = queue.pop_front() {
        if from_a.contains(&id) {
            candidates.push(id);
            continue;
        }
        for parent in graph.parents(id)? {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }

    // Drop the candidates that are ancestors of other candidates.
    if candidates.len() > 1 {
        let mut starts = Vec::new();
        for candidate in &candidates {
            starts.extend(graph.parents(*candidate)?);
        }
        let below = graph.ancestors(starts)?;
        candidates.retain(|candidate| !below.contains(candidate));
    }
    candidates.sort();
    Ok(candidates)
}

/// One best common ancestor of `a` and `b`, or `None` if their histories are
/// unrelated. When there are several (see [`merge_bases`]), the one with the
/// smallest id, so the answer does not depend on how history was walked.
pub fn merge_base<O: Objects>(
    objects: &O,
    a: ObjectId,
    b: ObjectId,
) -> Result<Option<ObjectId>, Error> {
    Ok(merge_bases(objects, a, b)?.into_iter().next())
}

/// Parents of changesets, read once each.
struct Graph<'o, O> {
    objects: &'o O,
    parents: HashMap<ObjectId, Vec<ObjectId>>,
}

impl<O: Objects> Graph<'_, O> {
    fn parents(&mut self, id: ObjectId) -> Result<Vec<ObjectId>, Error> {
        if let Some(parents) = self.parents.get(&id) {
            return Ok(parents.clone());
        }
        let parents = read_changeset(self.objects, id)?.parents;
        self.parents.insert(id, parents.clone());
        Ok(parents)
    }

    /// `starts` and all their ancestors.
    fn ancestors(&mut self, starts: Vec<ObjectId>) -> Result<HashSet<ObjectId>, Error> {
        let mut seen: HashSet<ObjectId> = starts.iter().copied().collect();
        let mut queue: VecDeque<ObjectId> = starts.into();
        while let Some(id) = queue.pop_front() {
            for parent in self.parents(id)? {
                if seen.insert(parent) {
                    queue.push_back(parent);
                }
            }
        }
        Ok(seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Repo;

    /// A changeset with these parents, distinguished by a file named `name`.
    fn node(repo: &Repo, parents: &[ObjectId], name: &str) -> ObjectId {
        repo.commit(parents, vec![repo.set(&format!("{name}.txt"), name)])
    }

    #[test]
    fn first_parent_chain() {
        let repo = Repo::new();
        let root = node(&repo, &[], "root");
        let a = node(&repo, &[root], "a");
        let side = node(&repo, &[root], "side");
        let merge = node(&repo, &[a, side], "merge");
        let head = node(&repo, &[merge], "head");
        let objects = &repo.objects;
        for ancestor in [head, merge, a, root] {
            assert!(first_parent_chain_contains(objects, head, ancestor, usize::MAX).unwrap());
        }
        // Reachable, but only through a second parent.
        assert!(!first_parent_chain_contains(objects, head, side, usize::MAX).unwrap());
        // Not an ancestor at all.
        assert!(!first_parent_chain_contains(objects, root, head, usize::MAX).unwrap());
        // The limit counts steps.
        assert!(first_parent_chain_contains(objects, head, a, 2).unwrap());
        assert!(!first_parent_chain_contains(objects, head, a, 1).unwrap());
        assert!(first_parent_chain_contains(objects, head, head, 0).unwrap());
    }

    #[test]
    fn merge_base_of_a_fork() {
        let repo = Repo::new();
        let root = node(&repo, &[], "root");
        let base = node(&repo, &[root], "base");
        let left = node(&repo, &[base], "left");
        let left2 = node(&repo, &[left], "left2");
        let right = node(&repo, &[base], "right");
        let objects = &repo.objects;
        assert_eq!(merge_base(objects, left2, right).unwrap(), Some(base));
        assert_eq!(merge_base(objects, right, left2).unwrap(), Some(base));
        // An ancestor is its own merge base with a descendant.
        assert_eq!(merge_base(objects, left2, base).unwrap(), Some(base));
        assert_eq!(merge_base(objects, left, left).unwrap(), Some(left));
    }

    #[test]
    fn merge_base_skips_ancestors_of_better_candidates() {
        // `b` reaches `root` directly and `mid` through `d`; `mid` is better.
        let repo = Repo::new();
        let root = node(&repo, &[], "root");
        let mid = node(&repo, &[root], "mid");
        let a = node(&repo, &[mid], "a");
        let d = node(&repo, &[mid], "d");
        let b = node(&repo, &[root, d], "b");
        assert_eq!(merge_bases(&repo.objects, a, b).unwrap(), [mid]);
    }

    #[test]
    fn criss_cross_has_two_merge_bases() {
        let repo = Repo::new();
        let root = node(&repo, &[], "root");
        let x = node(&repo, &[root], "x");
        let y = node(&repo, &[root], "y");
        let m1 = node(&repo, &[x, y], "m1");
        let m2 = node(&repo, &[y, x], "m2");
        let mut expected = vec![x, y];
        expected.sort();
        assert_eq!(merge_bases(&repo.objects, m1, m2).unwrap(), expected);
        assert_eq!(
            merge_base(&repo.objects, m1, m2).unwrap(),
            Some(expected[0])
        );
    }

    #[test]
    fn unrelated_histories_have_no_merge_base() {
        let repo = Repo::new();
        let one = node(&repo, &[], "one");
        let two = node(&repo, &[], "two");
        assert_eq!(merge_base(&repo.objects, one, two).unwrap(), None);
    }
}
