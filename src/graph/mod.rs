//! Id-level quad storage: one [`Partition`] (one s-graph tree) per graph.
//!
//! A quad `(g, s, p, o)` is the triple `(s, p', o)` in `g`'s partition,
//! where `p'` is `p`'s dense predicate id ([`PredicateTable`], shared by all
//! partitions). Nothing else is stored per quad: no graph column, no
//! secondary permutations; s-graph's Morton-ordered trie answers every
//! bound/unbound combination of S, P and O itself.
//!
//! Per-graph trees also give SPARQL's dataset model directly: `GRAPH ?g`
//! enumerates partitions, `GRAPH <g>` selects one, and a single-graph BGP is
//! one native s-graph join. The trade-off is a fixed per-graph cost (one
//! tree's metadata and first block), which favours datasets with fewer,
//! larger graphs over one graph per statement.
//!
//! This layer knows nothing about terms or queries: [`crate::store`] maps
//! terms to ids and [`crate::sparql`] plans over partitions.

pub mod domain;
pub mod partition;
pub mod predicates;

pub use partition::{Partition, Tree};
pub use predicates::PredicateTable;

use s_dict::TermId;
use s_graph::tree::Scan;
use s_graph::{BlockStorage, Triple};
use std::collections::BTreeMap;
use std::collections::btree_map;

/// Which graph of the dataset a quad belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GraphName {
    Default,
    Named(TermId),
}

/// A quad selection over ids; `None` matches anything. `graph: None` covers
/// the default graph and every named graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdPattern {
    pub graph: Option<GraphName>,
    pub subject: Option<TermId>,
    pub predicate: Option<TermId>,
    pub object: Option<TermId>,
}

/// An id quad: `(graph, subject, predicate, object)`, predicate as a term id.
pub type IdQuad = (GraphName, TermId, TermId, TermId);

pub struct QuadIndex<S: BlockStorage> {
    predicates: PredicateTable,
    default: Partition<S>,
    named: BTreeMap<TermId, Partition<S>>,
    block_capacity: usize,
}

impl<S: BlockStorage> QuadIndex<S> {
    pub fn new(block_capacity: usize) -> Self {
        Self { predicates: PredicateTable::new(), default: Partition::new(block_capacity), named: BTreeMap::new(), block_capacity }
    }

    /// Reassembles an index from persisted parts.
    pub fn from_parts(predicates: PredicateTable, default: Partition<S>, named: BTreeMap<TermId, Partition<S>>, block_capacity: usize) -> Self {
        Self { predicates, default, named, block_capacity }
    }

    pub fn predicates(&self) -> &PredicateTable {
        &self.predicates
    }

    pub fn block_capacity(&self) -> usize {
        self.block_capacity
    }

    /// Total number of quads.
    pub fn len(&self) -> u64 {
        self.default.len() + self.named.values().map(Partition::len).sum::<u64>()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn partition(&self, graph: GraphName) -> Option<&Partition<S>> {
        match graph {
            GraphName::Default => Some(&self.default),
            GraphName::Named(g) => self.named.get(&g),
        }
    }

    pub fn default_graph(&self) -> &Partition<S> {
        &self.default
    }

    /// Named graphs holding at least one quad, in graph id order.
    pub fn named_graphs(&self) -> btree_map::Iter<'_, TermId, Partition<S>> {
        self.named.iter()
    }

    fn partition_mut(&mut self, graph: GraphName) -> &mut Partition<S> {
        match graph {
            GraphName::Default => &mut self.default,
            GraphName::Named(g) => self.named.entry(g).or_insert_with(|| Partition::new(self.block_capacity)),
        }
    }

    pub fn insert(&mut self, graph: GraphName, s: TermId, p: TermId, o: TermId) -> bool {
        let p = self.predicates.intern(p);
        self.partition_mut(graph).insert(s.0, p, o.0)
    }

    /// Inserts `(s, p, o)` id triples whose predicates are *term* ids; they
    /// are rewritten to dense ids in place. Returns how many were new.
    pub fn insert_batch(&mut self, graph: GraphName, triples: &mut [Triple]) -> u64 {
        if triples.is_empty() {
            return 0;
        }
        for t in triples.iter_mut() {
            t.1 = self.predicates.intern(TermId(t.1));
        }
        self.partition_mut(graph).insert_batch(triples)
    }

    /// Like [`Self::insert_batch`], taking ownership: an empty partition is
    /// bulk-built from `triples` in one pass (s-graph's bulk build is far
    /// cheaper than inserting one by one) instead of inserted into.
    pub fn extend(&mut self, graph: GraphName, mut triples: Vec<Triple>) -> u64 {
        if triples.is_empty() {
            return 0;
        }
        if self.partition(graph).is_some_and(|p| !p.is_empty()) {
            return self.insert_batch(graph, &mut triples);
        }
        for t in triples.iter_mut() {
            t.1 = self.predicates.intern(TermId(t.1));
        }
        let part = Partition::from_triples(triples, self.block_capacity);
        let len = part.len();
        match graph {
            GraphName::Default => self.default = part,
            GraphName::Named(g) => {
                self.named.insert(g, part);
            }
        }
        len
    }

    pub fn remove(&mut self, graph: GraphName, s: TermId, p: TermId, o: TermId) -> bool {
        let Some(p) = self.predicates.get(p) else { return false };
        match graph {
            GraphName::Default => self.default.remove(s.0, p, o.0),
            GraphName::Named(g) => {
                let Some(part) = self.named.get_mut(&g) else { return false };
                let removed = part.remove(s.0, p, o.0);
                if part.is_empty() {
                    self.named.remove(&g);
                }
                removed
            }
        }
    }

    pub fn contains(&self, graph: GraphName, s: TermId, p: TermId, o: TermId) -> bool {
        let Some(p) = self.predicates.get(p) else { return false };
        self.partition(graph).is_some_and(|part| part.contains(s.0, p, o.0))
    }

    /// [`Self::contains`] for each `(s, p, o)` of `quads` in `graph`, into
    /// `out` in input order; `probes` is scratch for the tree coordinates
    /// (reused across calls). One batched s-graph descent for all of them.
    pub fn contains_batch_into(&self, graph: GraphName, quads: &[(TermId, TermId, TermId)], probes: &mut Vec<Triple>, out: &mut Vec<bool>) {
        let Some(part) = self.partition(graph) else {
            out.clear();
            out.resize(quads.len(), false);
            return;
        };
        probes.clear();
        // An unknown predicate becomes `u32::MAX`, never a dense id, so the
        // probe misses.
        probes.extend(quads.iter().map(|&(s, p, o)| (s.0, self.predicates.get(p).unwrap_or(u32::MAX), o.0)));
        part.contains_batch_into(probes, out);
    }

    /// Drops every quad of `graph`.
    pub fn clear_graph(&mut self, graph: GraphName) {
        match graph {
            GraphName::Default => self.default = Partition::new(self.block_capacity),
            GraphName::Named(g) => {
                self.named.remove(&g);
            }
        }
    }

    /// Streams matching quads, graph by graph, without collecting them.
    pub fn quads(&self, pattern: IdPattern) -> Quads<'_, S> {
        let predicate = match pattern.predicate {
            Some(p) => match self.predicates.get(p) {
                Some(dense) => Some(dense),
                None => return Quads::empty(self),
            },
            None => None,
        };
        let bounds = [pattern.subject.map(|t| t.0), predicate, pattern.object.map(|t| t.0)];
        let (default_pending, named) = match pattern.graph {
            None => (true, self.named.range(..)),
            Some(GraphName::Default) => (true, self.named.range(TermId(0)..TermId(0))),
            Some(GraphName::Named(g)) => (false, self.named.range(g..=g)),
        };
        Quads { index: self, default_pending, named, current: None, bounds }
    }

    pub fn shrink_to_fit(&mut self) {
        self.default.shrink_to_fit();
        self.named.values_mut().for_each(Partition::shrink_to_fit);
    }

    /// Bytes held by trees and the predicate table.
    pub fn mem_size_bytes(&self) -> usize {
        self.predicates.size_bytes() + self.default.mem_size_bytes() + self.named.values().map(Partition::mem_size_bytes).sum::<usize>()
    }
}

/// Iterator returned by [`QuadIndex::quads`].
pub struct Quads<'a, S: BlockStorage> {
    index: &'a QuadIndex<S>,
    default_pending: bool,
    named: btree_map::Range<'a, TermId, Partition<S>>,
    current: Option<(GraphName, Scan<'a, S>)>,
    bounds: [Option<u32>; 3],
}

impl<'a, S: BlockStorage> Quads<'a, S> {
    fn empty(index: &'a QuadIndex<S>) -> Self {
        Quads { index, default_pending: false, named: index.named.range(TermId(0)..TermId(0)), current: None, bounds: [None; 3] }
    }
}

impl<S: BlockStorage> Iterator for Quads<'_, S> {
    type Item = IdQuad;

    fn next(&mut self) -> Option<IdQuad> {
        loop {
            if let Some((graph, scan)) = &mut self.current {
                if let Some((s, p, o)) = scan.next() {
                    return Some((*graph, TermId(s), self.index.predicates.term(p), TermId(o)));
                }
                self.current = None;
            }
            let (graph, part) = if std::mem::take(&mut self.default_pending) {
                (GraphName::Default, &self.index.default)
            } else {
                let (&g, part) = self.named.next()?;
                (GraphName::Named(g), part)
            };
            self.current = Some((graph, part.scan(self.bounds)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s_graph::storage::HeapStorage;

    type Q = QuadIndex<HeapStorage>;
    const G1: GraphName = GraphName::Named(TermId(100));
    const G2: GraphName = GraphName::Named(TermId(101));

    fn t(s: u32, p: u32, o: u32) -> (TermId, TermId, TermId) {
        (TermId(s), TermId(p), TermId(o))
    }

    fn sorted(q: &Q, pattern: IdPattern) -> Vec<IdQuad> {
        let mut v: Vec<IdQuad> = q.quads(pattern).collect();
        v.sort_unstable();
        v
    }

    fn sample() -> Q {
        let mut q = Q::new(4096);
        for (g, (s, p, o)) in [(GraphName::Default, t(1, 50, 2)), (G1, t(1, 50, 2)), (G1, t(3, 51, 4)), (G2, t(5, 50, 6))] {
            assert!(q.insert(g, s, p, o));
        }
        q
    }

    #[test]
    fn quads_live_in_their_own_graph() {
        let q = sample();
        assert_eq!(q.len(), 4);
        assert!(q.contains(GraphName::Default, TermId(1), TermId(50), TermId(2)));
        assert!(q.contains(G1, TermId(1), TermId(50), TermId(2)));
        assert!(!q.contains(G2, TermId(1), TermId(50), TermId(2)));
        assert!(!q.contains(GraphName::Default, TermId(3), TermId(51), TermId(4)));
        assert!(!q.contains(G1, TermId(1), TermId(999), TermId(2)), "unknown predicate");
        assert_eq!(q.named_graphs().map(|(g, _)| *g).collect::<Vec<_>>(), vec![TermId(100), TermId(101)]);
    }

    #[test]
    fn predicates_are_dense_and_shared() {
        let q = sample();
        assert_eq!(q.predicates().len(), 2);
        assert_eq!(q.predicates().get(TermId(50)), Some(0));
        assert_eq!(q.predicates().get(TermId(51)), Some(1));
    }

    #[test]
    fn pattern_iteration_maps_back_to_term_ids() {
        let q = sample();
        let all = sorted(&q, IdPattern::default());
        assert_eq!(all.len(), 4);
        assert_eq!(
            sorted(&q, IdPattern { predicate: Some(TermId(50)), ..Default::default() }),
            vec![
                (GraphName::Default, TermId(1), TermId(50), TermId(2)),
                (G1, TermId(1), TermId(50), TermId(2)),
                (G2, TermId(5), TermId(50), TermId(6)),
            ]
        );
        assert_eq!(sorted(&q, IdPattern { graph: Some(G1), ..Default::default() }).len(), 2);
        assert_eq!(sorted(&q, IdPattern { graph: Some(GraphName::Default), ..Default::default() }).len(), 1);
        assert_eq!(sorted(&q, IdPattern { graph: Some(GraphName::Named(TermId(7))), ..Default::default() }), vec![]);
        assert_eq!(sorted(&q, IdPattern { predicate: Some(TermId(77)), ..Default::default() }), vec![]);
        assert_eq!(sorted(&q, IdPattern { subject: Some(TermId(3)), object: Some(TermId(4)), ..Default::default() }), vec![(G1, TermId(3), TermId(51), TermId(4))]);
    }

    #[test]
    fn removing_the_last_quad_drops_a_named_graph() {
        let mut q = sample();
        assert!(q.remove(G2, TermId(5), TermId(50), TermId(6)));
        assert!(!q.remove(G2, TermId(5), TermId(50), TermId(6)));
        assert!(q.partition(G2).is_none());
        assert!(!q.remove(GraphName::Default, TermId(1), TermId(99), TermId(2)));
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn batch_insert_rewrites_predicates_in_place() {
        let mut q = Q::new(4096);
        let mut batch = vec![(1, 900, 2), (2, 901, 3), (1, 900, 2)];
        assert_eq!(q.insert_batch(G1, &mut batch), 2);
        assert!(batch.iter().all(|&(_, p, _)| p < 2), "dense predicates");
        assert!(q.contains(G1, TermId(2), TermId(901), TermId(3)));
    }

    #[test]
    fn extend_bulk_builds_empty_graphs_and_inserts_into_others() {
        let mut q = Q::new(4096);
        assert_eq!(q.extend(G1, vec![(1, 900, 2), (1, 900, 2), (3, 901, 4)]), 2);
        assert_eq!(q.extend(G1, vec![(5, 900, 6), (1, 900, 2)]), 1);
        assert_eq!(q.extend(GraphName::Default, vec![(7, 902, 8)]), 1);
        assert_eq!(q.len(), 4);
        assert!(q.contains(G1, TermId(5), TermId(900), TermId(6)));
        assert!(q.contains(GraphName::Default, TermId(7), TermId(902), TermId(8)));
        assert_eq!(q.extend(G2, vec![]), 0);
        assert!(q.partition(G2).is_none());
    }

    #[test]
    fn batched_contains_maps_predicates_and_misses_unknown_ones() {
        let q = sample();
        let quads = [t(3, 51, 4), t(1, 50, 2), t(3, 99, 4), t(5, 50, 6), t(1, 51, 2)];
        let (mut probes, mut out) = (Vec::new(), Vec::new());
        q.contains_batch_into(G1, &quads, &mut probes, &mut out);
        assert_eq!(out, vec![true, true, false, false, false]);
        q.contains_batch_into(GraphName::Default, &quads, &mut probes, &mut out);
        assert_eq!(out, vec![false, true, false, false, false]);
        q.contains_batch_into(GraphName::Named(TermId(7)), &quads, &mut probes, &mut out);
        assert_eq!(out, vec![false; 5], "a missing graph holds nothing");
    }

    #[test]
    fn clear_graph() {
        let mut q = sample();
        q.clear_graph(G1);
        q.clear_graph(GraphName::Default);
        assert_eq!(q.len(), 1);
    }
}
