// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Ancestry: first-parent chains and merge bases.
//!
//! The free functions walk history one changeset at a time, reading only
//! objects, so they cost time proportional to the part of history they
//! cover. The methods of [`History`] of the same names give the same answers
//! with commit graph data, derived data stored per changeset:
//!
//! - the **generation**, 1 for a changeset without parents and otherwise 1
//!   more than the largest generation of its parents, so that an ancestor
//!   always has a smaller generation than its descendants and walks stop as
//!   soon as they go below the one they look for;
//! - the **height**, the number of first parents down to a changeset
//!   without parents, and a **skip pointer** to the first-parent ancestor at
//!   height [`skip_height`]`(height)`, the rule of Bitcoin's block index:
//!   following skip pointers reaches any first-parent ancestor in a number of
//!   steps that grows with the logarithm of the distance.
//!
//! The derived function `graph`, version 1, of a changeset is:
//!
//! ```text
//! uv   n, n × [32] parents, in order
//! uv   generation
//! uv   height
//! [32] skip pointer, when the height is not 0
//! ```
//!
//! It depends on the parents' values, so computing it for a changeset
//! computes it for every ancestor that lacks it, once; afterwards a query
//! reads only the records it needs.

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::rc::Rc;

use braid_format::codec::{Reader, put_id, put_uv};
use braid_format::{DecodeError, Kind, ObjectId, Objects};

use crate::access::read_changeset;
use crate::derived::{DerivedIndex, DerivedKey};
use crate::error::{Error, store_error};
use crate::history::History;

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

/// Whether `ancestor` is `descendant` or one of its ancestors, through any
/// parents. The walk goes back from `descendant` and stops when it finds
/// `ancestor`, so a "no" covers the whole history behind `descendant`.
pub fn is_ancestor<O: Objects>(
    objects: &O,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool, Error> {
    all_ancestors(objects, &[ancestor], descendant)
}

/// Whether each of `ids` is `head` or one of its ancestors: one walk back
/// from `head`, which stops as soon as all of them are found.
pub(crate) fn all_ancestors<O: Objects>(
    objects: &O,
    ids: &[ObjectId],
    head: ObjectId,
) -> Result<bool, Error> {
    let mut wanted: HashSet<ObjectId> = ids.iter().copied().collect();
    if wanted.is_empty() {
        return Ok(true);
    }
    let mut graph = Graph {
        objects,
        parents: HashMap::new(),
    };
    let mut seen = HashSet::from([head]);
    let mut queue = VecDeque::from([head]);
    while let Some(id) = queue.pop_front() {
        if wanted.remove(&id) && wanted.is_empty() {
            return Ok(true);
        }
        for parent in graph.parents(id)? {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
    }
    Ok(false)
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

/// The derived function of commit graph data.
const GRAPH: &str = "graph";

/// The version of its encoding.
const GRAPH_VERSION: u64 = 1;

/// What the commit graph records of a changeset: the value of the derived
/// function `graph`, version 1 (section 8 of the format spec).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphNode {
    /// The parents, in order.
    pub parents: Vec<ObjectId>,
    /// 1 without parents, otherwise 1 more than the largest generation of
    /// the parents.
    pub generation: u64,
    /// The number of first parents down to a changeset without parents.
    pub height: u64,
    /// The first-parent ancestor at height [`skip_height`]`(height)`; `None`
    /// exactly when the height is 0.
    pub skip: Option<ObjectId>,
}

impl GraphNode {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + 32 * (self.parents.len() + 1));
        put_uv(&mut out, self.parents.len() as u64);
        for parent in &self.parents {
            put_id(&mut out, parent);
        }
        put_uv(&mut out, self.generation);
        put_uv(&mut out, self.height);
        if let Some(skip) = &self.skip {
            put_id(&mut out, skip);
        }
        out
    }

    /// Decodes a value, checking what it can without other records.
    fn decode(value: &[u8]) -> Option<GraphNode> {
        let read = || -> Result<GraphNode, DecodeError> {
            let mut reader = Reader::new(value);
            let count = reader.count(ObjectId::LEN)?;
            let mut parents = Vec::with_capacity(count);
            for _ in 0..count {
                parents.push(reader.id()?);
            }
            let generation = reader.uv()?;
            let height = reader.uv()?;
            let skip = if height == 0 {
                None
            } else {
                Some(reader.id()?)
            };
            reader.finish()?;
            Ok(GraphNode {
                parents,
                generation,
                height,
                skip,
            })
        };
        let node = read().ok()?;
        let consistent = if node.parents.is_empty() {
            node.generation == 1 && node.height == 0
        } else {
            node.generation >= 2 && node.height >= 1
        };
        consistent.then_some(node)
    }
}

/// The height the skip pointer of a changeset at `height` points to, by the
/// rule of Bitcoin's block index: 0 below 2; for an even height, the height
/// with its lowest set bit cleared; for an odd one, 1 more than `height - 1`
/// with its two lowest set bits cleared. Always below `height` when `height`
/// is not 0.
pub fn skip_height(height: u64) -> u64 {
    fn clear_lowest_bit(n: u64) -> u64 {
        n & n.wrapping_sub(1)
    }
    if height < 2 {
        0
    } else if height & 1 == 1 {
        clear_lowest_bit(clear_lowest_bit(height - 1)) + 1
    } else {
        clear_lowest_bit(height)
    }
}

/// Flags of the walk that finds merge bases.
const FROM_A: u8 = 1;
const FROM_B: u8 = 2;
const FROM_BOTH: u8 = FROM_A | FROM_B;
/// Below a merge base already found, so not a best one.
const STALE: u8 = 4;

impl<O: Objects, I: DerivedIndex> History<'_, O, I> {
    /// The commit graph data of a changeset, computed for it and for its
    /// ancestors when missing.
    pub fn graph(&mut self, id: ObjectId) -> Result<GraphNode, Error> {
        let node = self.node(id)?;
        Ok(GraphNode::clone(&node))
    }

    /// The generation of a changeset (see [`GraphNode::generation`]).
    pub fn generation(&mut self, id: ObjectId) -> Result<u64, Error> {
        Ok(self.node(id)?.generation)
    }

    /// The changeset `steps` first parents below `id` (`id` itself for 0), or
    /// `None` when the chain is shorter. Logarithmic in `steps`.
    pub fn first_parent_ancestor(
        &mut self,
        id: ObjectId,
        steps: u64,
    ) -> Result<Option<ObjectId>, Error> {
        let height = self.node(id)?.height;
        if steps > height {
            return Ok(None);
        }
        Ok(Some(self.ancestor_at(id, height - steps)?))
    }

    /// Whether `base` is `head` or one of its first-parent ancestors no more
    /// than `limit` steps away, as [`first_parent_chain_contains`] says, with
    /// skip pointers: logarithmic in the distance.
    pub fn first_parent_chain_contains(
        &mut self,
        head: ObjectId,
        base: ObjectId,
        limit: usize,
    ) -> Result<bool, Error> {
        if head == base {
            return Ok(true);
        }
        if limit == 0 {
            return Ok(false);
        }
        let top = self.node(head)?;
        if !self.is_changeset(base)? {
            return Ok(false);
        }
        let target = self.node(base)?;
        if target.height >= top.height || top.height - target.height > limit as u64 {
            return Ok(false);
        }
        Ok(self.ancestor_at(head, target.height)? == base)
    }

    /// Whether `ancestor` is `descendant` or one of its ancestors, as
    /// [`is_ancestor`] says. A first-parent ancestor is found through skip
    /// pointers; otherwise the walk back from `descendant` never goes below
    /// the generation of `ancestor`.
    pub fn is_ancestor(&mut self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool, Error> {
        self.all_ancestors(&[ancestor], descendant)
    }

    /// Whether each of `ids` is `head` or one of its ancestors: the answer of
    /// [`is_ancestor`] for all of them at once. The walk back from `head`
    /// never goes below the smallest generation among them.
    pub fn all_ancestors(&mut self, ids: &[ObjectId], head: ObjectId) -> Result<bool, Error> {
        let mut wanted: HashSet<ObjectId> = ids.iter().copied().collect();
        wanted.remove(&head);
        if wanted.is_empty() {
            return Ok(true);
        }
        let top = self.node(head)?;
        let mut floor = u64::MAX;
        let mut off_chain = HashSet::new();
        for id in wanted {
            if !self.is_changeset(id)? {
                return Ok(false);
            }
            let node = self.node(id)?;
            if node.generation >= top.generation {
                return Ok(false);
            }
            if node.height < top.height && self.ancestor_at(head, node.height)? == id {
                continue;
            }
            floor = floor.min(node.generation);
            off_chain.insert(id);
        }
        if off_chain.is_empty() {
            return Ok(true);
        }
        let mut seen = HashSet::from([head]);
        let mut stack = vec![head];
        while let Some(id) = stack.pop() {
            let node = self.node(id)?;
            for &parent in &node.parents {
                if !seen.insert(parent) {
                    continue;
                }
                if off_chain.remove(&parent) && off_chain.is_empty() {
                    return Ok(true);
                }
                if self.node(parent)?.generation > floor {
                    stack.push(parent);
                }
            }
        }
        Ok(false)
    }

    /// The best common ancestors of `a` and `b`, sorted by id, as
    /// [`merge_bases`] says.
    ///
    /// When one is a first-parent ancestor of the other, skip pointers find
    /// it. Otherwise the walk goes down from both in order of generation,
    /// marking what it reaches from each side; a changeset reached from both
    /// is a best common ancestor unless it is below one already found, and
    /// the walk stops when everything left to visit is below one. It visits
    /// the changesets between `a`, `b` and their merge bases, never all of
    /// history unless the histories are unrelated.
    pub fn merge_bases(&mut self, a: ObjectId, b: ObjectId) -> Result<Vec<ObjectId>, Error> {
        let (top_a, top_b) = (self.node(a)?, self.node(b)?);
        if a == b {
            return Ok(vec![a]);
        }
        if top_b.height < top_a.height && self.ancestor_at(a, top_b.height)? == b {
            return Ok(vec![b]);
        }
        if top_a.height < top_b.height && self.ancestor_at(b, top_a.height)? == a {
            return Ok(vec![a]);
        }

        let mut flags: HashMap<ObjectId, u8> = HashMap::from([(a, FROM_A), (b, FROM_B)]);
        let mut queue = BinaryHeap::from([(top_a.generation, a), (top_b.generation, b)]);
        // Queued changesets, and those of them that are not stale.
        let mut queued = HashSet::from([a, b]);
        let mut live = HashSet::from([a, b]);
        let mut bases = Vec::new();
        while !live.is_empty() {
            let Some((_, id)) = queue.pop() else {
                break;
            };
            queued.remove(&id);
            live.remove(&id);
            let mut mark = flags[&id];
            if mark & (FROM_BOTH | STALE) == FROM_BOTH {
                bases.push(id);
                mark |= STALE;
                flags.insert(id, mark);
            }
            let node = self.node(id)?;
            for &parent in &node.parents {
                let old = flags.get(&parent).copied().unwrap_or(0);
                let new = old | mark;
                if new == old {
                    continue;
                }
                flags.insert(parent, new);
                if queued.insert(parent) {
                    let generation = self.node(parent)?.generation;
                    queue.push((generation, parent));
                }
                if new & STALE == 0 {
                    live.insert(parent);
                } else {
                    live.remove(&parent);
                }
            }
        }
        bases.sort();
        Ok(bases)
    }

    /// One best common ancestor of `a` and `b`, the one with the smallest id,
    /// as [`merge_base`] says.
    pub fn merge_base(&mut self, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>, Error> {
        Ok(self.merge_bases(a, b)?.into_iter().next())
    }

    /// Whether `id` is stored as a changeset.
    fn is_changeset(&mut self, id: ObjectId) -> Result<bool, Error> {
        if self.nodes.contains_key(&id) {
            return Ok(true);
        }
        Ok(matches!(
            self.objects.stat(&id).map_err(store_error)?,
            Some((Kind::Changeset, _))
        ))
    }

    /// The first-parent ancestor of `id` at `height`, which must not be above
    /// the height of `id`, through skip pointers where they get closer.
    fn ancestor_at(&mut self, id: ObjectId, height: u64) -> Result<ObjectId, Error> {
        let mut at = id;
        let mut node = self.node(id)?;
        while node.height > height {
            let current = node.height;
            let skip = skip_height(current);
            let skip_below = skip_height(current - 1);
            // Take the skip pointer when it lands on the target, or above it
            // where the parent's own pointer would not land closer.
            let jump = skip == height
                || (skip > height && !(skip_below + 2 < skip && skip_below >= height));
            let (next, expected) = match (node.skip, node.parents.first()) {
                (Some(skip_to), _) if jump => (skip_to, skip),
                (_, Some(parent)) => (*parent, current - 1),
                (_, None) => return Err(Error::DerivedData { id: at }),
            };
            at = next;
            node = self.node(at)?;
            if node.height != expected {
                return Err(Error::DerivedData { id: at });
            }
        }
        Ok(at)
    }

    /// The commit graph data of `id`, from memory, from its record, or
    /// computed with that of every ancestor that lacks it.
    pub(crate) fn node(&mut self, id: ObjectId) -> Result<Rc<GraphNode>, Error> {
        if let Some(node) = self.nodes.get(&id) {
            return Ok(Rc::clone(node));
        }
        if let Some(node) = self.load_node(id)? {
            return Ok(node);
        }
        // Depth first, parents before children, with an explicit stack: a
        // history can be far longer than a thread's stack is deep.
        let mut parents_of: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
        let mut stack = vec![(id, false)];
        while let Some((top, expanded)) = stack.pop() {
            if self.nodes.contains_key(&top) {
                continue;
            }
            if expanded {
                let parents = parents_of.remove(&top).unwrap_or_default();
                self.compute_node(top, parents)?;
                continue;
            }
            if top != id && self.load_node(top)?.is_some() {
                continue;
            }
            let parents = read_changeset(self.objects, top)?.parents;
            stack.push((top, true));
            for parent in &parents {
                if !self.nodes.contains_key(parent) {
                    stack.push((*parent, false));
                }
            }
            parents_of.insert(top, parents);
        }
        self.nodes
            .get(&id)
            .cloned()
            .ok_or(Error::DerivedData { id })
    }

    /// The commit graph data of `id` from its record, remembered.
    fn load_node(&mut self, id: ObjectId) -> Result<Option<Rc<GraphNode>>, Error> {
        let key = DerivedKey::new(GRAPH, GRAPH_VERSION, &[id], b"");
        let Some(node) = self.load(&key, GraphNode::decode)? else {
            return Ok(None);
        };
        let node = Rc::new(node);
        self.nodes.insert(id, Rc::clone(&node));
        Ok(Some(node))
    }

    /// Computes and stores the commit graph data of `id`, whose parents have
    /// theirs.
    fn compute_node(&mut self, id: ObjectId, parents: Vec<ObjectId>) -> Result<(), Error> {
        let mut generation = 0;
        for parent in &parents {
            generation = generation.max(self.node(*parent)?.generation);
        }
        let (height, skip) = match parents.first() {
            None => (0, None),
            Some(first) => {
                let height = self.node(*first)?.height + 1;
                (height, Some(self.ancestor_at(*first, skip_height(height))?))
            }
        };
        let node = GraphNode {
            parents,
            generation: generation + 1,
            height,
            skip,
        };
        let key = DerivedKey::new(GRAPH, GRAPH_VERSION, &[id], b"");
        self.save(key, &node.encode())?;
        self.nodes.insert(id, Rc::new(node));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derived::MemoryIndex;
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
    fn ancestors_through_any_parent() {
        let repo = Repo::new();
        let root = node(&repo, &[], "root");
        let side = node(&repo, &[root], "side");
        let main = node(&repo, &[root], "main");
        let merge = node(&repo, &[main, side], "merge");
        let objects = &repo.objects;
        for ancestor in [merge, main, side, root] {
            assert!(is_ancestor(objects, ancestor, merge).unwrap());
        }
        assert!(!is_ancestor(objects, merge, main).unwrap());
        assert!(!is_ancestor(objects, side, main).unwrap());
        assert!(all_ancestors(objects, &[side, main, root], merge).unwrap());
        assert!(!all_ancestors(objects, &[side, merge], main).unwrap());
        assert!(all_ancestors(objects, &[], main).unwrap());
    }

    #[test]
    fn unrelated_histories_have_no_merge_base() {
        let repo = Repo::new();
        let one = node(&repo, &[], "one");
        let two = node(&repo, &[], "two");
        assert_eq!(merge_base(&repo.objects, one, two).unwrap(), None);
        let index = MemoryIndex::new();
        let mut history = History::new(&repo.objects, &index);
        assert_eq!(history.merge_bases(one, two).unwrap(), []);
        assert!(!history.is_ancestor(one, two).unwrap());
    }

    #[test]
    fn graph_data_counts_generations_and_heights() {
        let repo = Repo::new();
        let root = node(&repo, &[], "root");
        let a = node(&repo, &[root], "a");
        let b = node(&repo, &[a], "b");
        let side = node(&repo, &[root], "side");
        let merge = node(&repo, &[side, b], "merge");
        let index = MemoryIndex::new();
        let mut history = History::new(&repo.objects, &index);
        let graph = history.graph(merge).unwrap();
        // The merge is one above its highest parent, b, but its first
        // parent is the side branch: height 2.
        assert_eq!(graph.generation, 4);
        assert_eq!(graph.height, 2);
        assert_eq!(graph.parents, [side, b]);
        assert_eq!(graph.skip, Some(root));
        assert_eq!(history.graph(root).unwrap().skip, None);
        assert_eq!(history.generation(b).unwrap(), 3);
        assert_eq!(history.first_parent_ancestor(merge, 1).unwrap(), Some(side));
        assert_eq!(history.first_parent_ancestor(merge, 2).unwrap(), Some(root));
        assert_eq!(history.first_parent_ancestor(merge, 3).unwrap(), None);
        // One record per changeset, recorded on flush.
        assert_eq!(history.computed(), 5);
        history.flush().unwrap();
        assert_eq!(index.len(), 5);
        let decoded = GraphNode::decode(&graph.encode()).unwrap();
        assert_eq!(decoded, graph);
        // Values that contradict themselves are refused.
        let mut bad = graph.clone();
        bad.generation = 1;
        assert_eq!(GraphNode::decode(&bad.encode()), None);
    }

    #[test]
    fn history_answers_what_the_walks_answer() {
        let repo = Repo::new();
        let root = node(&repo, &[], "root");
        let mid = node(&repo, &[root], "mid");
        let a = node(&repo, &[mid], "a");
        let d = node(&repo, &[mid], "d");
        let b = node(&repo, &[root, d], "b");
        let x = node(&repo, &[a], "x");
        let y = node(&repo, &[b], "y");
        let m1 = node(&repo, &[x, y], "m1");
        let m2 = node(&repo, &[y, x], "m2");
        let octopus = node(&repo, &[m1, a, d, b], "octopus");
        let all = [root, mid, a, d, b, x, y, m1, m2, octopus];
        let index = MemoryIndex::new();
        let mut history = History::new(&repo.objects, &index);
        let objects = &repo.objects;
        for p in all {
            for q in all {
                assert_eq!(
                    history.merge_bases(p, q).unwrap(),
                    merge_bases(objects, p, q).unwrap()
                );
                assert_eq!(
                    history.is_ancestor(p, q).unwrap(),
                    is_ancestor(objects, p, q).unwrap()
                );
                for limit in [0, 1, 2, usize::MAX] {
                    assert_eq!(
                        history.first_parent_chain_contains(p, q, limit).unwrap(),
                        first_parent_chain_contains(objects, p, q, limit).unwrap()
                    );
                }
            }
        }
        let mut expected = vec![x, y];
        expected.sort();
        assert_eq!(history.merge_bases(m1, m2).unwrap(), expected);
        assert_eq!(history.merge_bases(a, b).unwrap(), [mid]);
        assert!(history.all_ancestors(&[a, d, root], octopus).unwrap());
        assert!(!history.all_ancestors(&[m2, a], octopus).unwrap());
        assert!(history.all_ancestors(&[], root).unwrap());
    }
}
