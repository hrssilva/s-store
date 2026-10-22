//! One graph's triples: an editable s-graph FST whose domains grow on
//! demand.
//!
//! Coordinates are `(subject term id, dense predicate id, object term id)`.
//! The graph is s-graph's [`EditableFst`]: a packed, suffix-sharing FST
//! (identical suffix languages share one state) with local, minimality
//! preserving insertion and removal over it. A bulk load builds the packed
//! FST once; edits go to s-graph's in-memory edit arena. [`Partition::seal`]
//! writes the edited states into blocks next to the packed ones (cheap,
//! proportional to the edits); [`Partition::shrink_to_fit`] repacks
//! everything, reclaiming the records the edits made obsolete.
//!
//! An FST's domains are fixed when it is built, so an insert outside them
//! rebuilds it from its own triples with grown domains; [`Domains::grown`]
//! at least doubles the subject/object domain, so rebuild cost is amortized
//! O(1) per insert.

use super::domain::Domains;
use s_graph::automata::MutableFstSource;
use s_graph::path_ops::PatternIter;
use s_graph::mutable_fst::UpdateLookup;
use s_graph::storage::ReclaimingBlockStorage;
use s_graph::{BlockStorage, EditableFst, PackedFst, Triple};

pub type Tree<S> = EditableFst<S>;

/// How s-graph finds an edited state's existing twin to re-share suffixes.
const LOOKUP: UpdateLookup = UpdateLookup::Sampled(8);

/// Matching triples of a [`Partition`], decoded straight from the FST.
pub type Scan<'a, S> = PatternIter<'a, MutableFstSource<'a, S>>;

pub struct Partition<S: BlockStorage> {
    tree: Tree<S>,
    domains: Domains,
    /// Largest predicate id stored, so growth never narrows the predicate
    /// domain below what the tree holds.
    max_predicate: u32,
    len: u64,
    block_capacity: usize,
    /// Edits were sealed into blocks since the last repack, leaving
    /// obsolete records behind.
    sealed: bool,
}

impl<S: BlockStorage> Partition<S> {
    pub fn new(block_capacity: usize) -> Self {
        Self::build(Vec::new(), Domains::fitting(0, 0), 0, block_capacity)
    }

    /// Bulk-builds from `triples` (duplicates allowed; s-graph sorts and
    /// deduplicates them as it builds).
    pub fn from_triples(triples: Vec<Triple>, block_capacity: usize) -> Self {
        let (max_term, max_predicate) = triples.iter().fold((0, 0), |(t, p), &(s, pp, o)| (t.max(s).max(o), p.max(pp)));
        Self::build(triples, Domains::fitting(max_term, max_predicate), max_predicate, block_capacity)
    }

    fn build(triples: Vec<Triple>, domains: Domains, max_predicate: u32, block_capacity: usize) -> Self {
        let tree = PackedFst::try_build(triples, domains.sizes(), block_capacity).expect("domains hold every triple").into_mutable_with_lookup(LOOKUP);
        // Counted over the FST's shared states, without listing triples.
        let len = tree.count_matching([None; 3]);
        Self { tree, domains, max_predicate, len, block_capacity, sealed: false }
    }

    /// Reassembles a partition from a deserialized tree, its stored count
    /// and the largest predicate id it holds.
    pub fn from_tree(tree: Tree<S>, len: u64, max_predicate: u32, block_capacity: usize) -> Self {
        let term_bits = tree.domain_size(0).trailing_zeros();
        let predicate_bits = tree.domain_size(1).trailing_zeros();
        Self { tree, domains: Domains { term_bits, predicate_bits }, max_predicate, len, block_capacity, sealed: false }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn max_predicate(&self) -> u32 {
        self.max_predicate
    }

    pub fn domains(&self) -> Domains {
        self.domains
    }

    pub fn tree(&self) -> &Tree<S> {
        &self.tree
    }

    /// The FST as an automata source: what [`s_graph::automata`] searches
    /// compile against.
    pub fn source(&self) -> MutableFstSource<'_, S> {
        self.tree.source()
    }

    pub fn contains(&self, s: u32, p: u32, o: u32) -> bool {
        self.tree.contains(s, p, o)
    }

    /// [`Self::contains`] for every triple of `triples`, into `out` in input
    /// order.
    pub fn contains_batch_into(&self, triples: &[Triple], out: &mut Vec<bool>) {
        self.tree.contains_batch_into(triples, out);
    }

    /// Matching triples; `None` is a wildcard.
    pub fn scan(&self, bounds: [Option<u32>; 3]) -> Scan<'_, S> {
        self.tree.scan(bounds)
    }

    /// Matching triples, counted over shared FST states without decoding
    /// them.
    pub fn count_matching(&self, bounds: [Option<u32>; 3]) -> u64 {
        if bounds == [None; 3] { self.len } else { self.tree.count_matching(bounds) }
    }

    pub fn insert(&mut self, s: u32, p: u32, o: u32) -> bool {
        self.ensure_holds(s.max(o), p);
        let inserted = self.tree.insert(s, p, o).expect("domains were grown to hold the triple");
        self.len += u64::from(inserted);
        inserted
    }

    /// Inserts a batch (reordered in place), returning how many were new.
    pub fn insert_batch(&mut self, triples: &mut [Triple]) -> u64 {
        let Some((max_term, max_predicate)) = triples.iter().map(|&(s, p, o)| (s.max(o), p)).reduce(|a, b| (a.0.max(b.0), a.1.max(b.1))) else {
            return 0;
        };
        self.ensure_holds(max_term, max_predicate);
        let inserted = self.tree.insert_batch(triples).expect("domains were grown to hold the batch");
        self.len += inserted;
        inserted
    }

    pub fn remove(&mut self, s: u32, p: u32, o: u32) -> bool {
        let removed = self.tree.remove(s, p, o).expect("remove never fails on coordinates");
        self.len -= u64::from(removed);
        removed
    }

    /// Ends a burst of edits: s-graph writes the live edited states into
    /// blocks beside the packed FST and drops the edit arena and indexes.
    /// Costs time proportional to the edits, not the graph; the records
    /// the edits replaced stay until [`Self::shrink_to_fit`].
    pub fn seal(&mut self) {
        if self.tree.edited_node_count() > 0 {
            self.replace_tree(|tree| tree.freeze_in_place().map(|frozen| frozen.into_mutable_with_lookup(LOOKUP)));
            self.sealed = true;
        }
    }

    /// Repacks the FST, edits and sealed records included, into ordinary
    /// packed blocks, releasing source blocks as they are copied.
    pub fn shrink_to_fit(&mut self)
    where
        S: ReclaimingBlockStorage,
    {
        if self.sealed || self.tree.edited_node_count() > 0 {
            self.replace_tree(|tree| tree.freeze().map(|packed| packed.into_mutable_with_lookup(LOOKUP)));
            self.sealed = false;
        }
    }

    /// s-graph's freezes consume the FST; an empty one stands in meanwhile.
    fn replace_tree(&mut self, f: impl FnOnce(Tree<S>) -> s_graph::Result<Tree<S>>) {
        let empty = Tree::try_build(Vec::new(), self.domains.sizes(), self.block_capacity).expect("empty FST");
        let tree = std::mem::replace(&mut self.tree, empty);
        self.tree = f(tree).expect("freezing a valid FST");
    }

    pub fn mem_size_bytes(&self) -> usize {
        self.tree.mem_size_bytes()
    }

    fn ensure_holds(&mut self, max_term: u32, predicate: u32) {
        self.max_predicate = self.max_predicate.max(predicate);
        if self.domains.holds(max_term, predicate, 0) {
            return;
        }
        let domains = self.domains.grown(max_term, self.max_predicate);
        let triples: Vec<Triple> = self.tree.scan([None; 3]).collect();
        *self = Self::build(triples, domains, self.max_predicate, self.block_capacity);
    }
}

#[cfg(all(test, feature = "file"))]
mod file_backend_tests {
    #[test]
    fn grows_on_file_storage() {
        let mut p = super::Partition::<s_graph::storage_file::FileStorage>::new(4096);
        assert!(p.insert(1, 0, 2));
        assert!(p.insert(1, 0, 70_000));
        assert!(p.contains(1, 0, 70_000));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s_graph::storage::HeapStorage;

    type P = Partition<HeapStorage>;

    fn all(p: &P) -> Vec<Triple> {
        let mut v: Vec<Triple> = p.scan([None; 3]).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn insert_contains_remove_and_count() {
        let mut p = P::new(4096);
        assert!(p.is_empty());
        assert!(p.insert(1, 0, 2));
        assert!(!p.insert(1, 0, 2));
        assert!(p.insert(2, 1, 1));
        assert_eq!(p.len(), 2);
        assert!(p.contains(1, 0, 2));
        assert!(p.remove(1, 0, 2));
        assert!(!p.remove(1, 0, 2));
        assert!(!p.contains(1, 0, 2));
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn batched_contains_answers_in_input_order() {
        let p = P::from_triples(vec![(1, 0, 2), (2, 1, 1), (7, 0, 3)], 4096);
        let probes = [(7, 0, 3), (1, 0, 3), (1, 0, 2), (1, 0, 2), (u32::MAX, 0, 2), (2, u32::MAX, 1)];
        let mut out = vec![true; 10];
        p.contains_batch_into(&probes, &mut out);
        assert_eq!(out, vec![true, false, true, true, false, false], "duplicates repeat, out-of-domain is absent");
        p.contains_batch_into(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn counts_and_compacts_edits() {
        let mut p = P::from_triples(vec![(1, 0, 2), (1, 0, 3), (2, 1, 3)], 4096);
        assert_eq!(p.count_matching([Some(1), None, None]), 2);
        assert_eq!(p.count_matching([None; 3]), 3);
        assert!(p.insert(4, 1, 3));
        assert!(p.remove(1, 0, 2));
        p.seal();
        assert_eq!(p.tree().edited_node_count(), 0);
        assert_eq!(all(&p), vec![(1, 0, 3), (2, 1, 3), (4, 1, 3)]);
        // Edits resume over the sealed records.
        assert!(p.insert(5, 0, 2));
        assert!(p.remove(4, 1, 3));
        p.seal();
        assert_eq!(all(&p), vec![(1, 0, 3), (2, 1, 3), (5, 0, 2)]);
        p.shrink_to_fit();
        assert_eq!(all(&p), vec![(1, 0, 3), (2, 1, 3), (5, 0, 2)]);
        assert_eq!(p.count_matching([None, Some(1), Some(3)]), 1);
        assert_eq!(p.len(), 3);
    }

    #[test]
    fn domains_grow_and_keep_existing_triples() {
        let mut p = P::new(4096);
        p.insert(1, 0, 2);
        p.insert(3, 1, 4);
        let before = p.domains();
        assert!(p.insert(1_000_000, 0, 5));
        assert!(p.domains().term_bits > before.term_bits);
        assert!(p.insert(7, 300, 8), "predicate domain grows too");
        assert_eq!(all(&p), vec![(1, 0, 2), (3, 1, 4), (7, 300, 8), (1_000_000, 0, 5)]);
        assert_eq!(p.len(), 4);
        // Predicate growth does not lose room for the old predicates.
        assert!(p.insert(9, 1, 9));
    }

    #[test]
    fn batch_insert_grows_once_and_counts_new_triples() {
        let mut p = P::new(4096);
        p.insert(0, 0, 0);
        let mut batch = vec![(5, 1, 70_000), (0, 0, 0), (6, 2, 6), (5, 1, 70_000)];
        assert_eq!(p.insert_batch(&mut batch), 2);
        assert_eq!(p.len(), 3);
        assert_eq!(all(&p), vec![(0, 0, 0), (5, 1, 70_000), (6, 2, 6)]);
        assert_eq!(p.insert_batch(&mut []), 0);
    }

    #[test]
    fn bulk_build_deduplicates() {
        let p = P::from_triples(vec![(3, 0, 1), (1, 1, 1), (3, 0, 1), (100_000, 4, 2)], 4096);
        assert_eq!(p.len(), 3);
        assert_eq!(all(&p), vec![(1, 1, 1), (3, 0, 1), (100_000, 4, 2)]);
        assert_eq!((2 * p.domains().term_bits + p.domains().predicate_bits) % 6, 0);
    }

    #[test]
    fn bounded_scans() {
        let p = P::from_triples(vec![(1, 0, 2), (1, 1, 3), (2, 0, 2)], 4096);
        let mut v: Vec<Triple> = p.scan([Some(1), None, None]).collect();
        v.sort_unstable();
        assert_eq!(v, vec![(1, 0, 2), (1, 1, 3)]);
        assert_eq!(p.scan([None, Some(0), Some(2)]).count(), 2);
        assert_eq!(p.scan([Some(1u32 << 30), None, None]).count(), 0, "out-of-domain bound matches nothing");
    }
}
