//! Property paths over s-graph's traversal primitives.
//!
//! Following SPARQL's translation (§18.2.2.4), sequences, alternatives and
//! inverses of plain links are rewritten into ordinary patterns by the
//! compiler (they have bag semantics). What remains, `*`, `+`, `?` and
//! negated property sets inside them, compiles to an epsilon-free automaton
//! over dense predicate ids ([`Nfa`]) and runs as s-graph's
//! [`PathAutomaton`], which deduplicates (node, state) pairs and emits
//! distinct endpoints: exactly the set semantics of those operators.
//!
//! [`View`] presents the active graph(s) to s-graph's traversal: one
//! partition's tree directly, or a merged default graph as a
//! [`PatternBackend`] whose scans skip triples an earlier graph already
//! produced, so merged graphs behave as an RDF merge (a set).

use super::idset::IdSet;
use crate::graph::Partition;
use s_graph::automaton::{PathAutomaton, Transition};
use s_graph::join::PatternBackend;
use s_graph::traversal::{Direction, Step};
use s_graph::tree::{ContainsBatchWorkspace, DimRestriction, Scan};
use s_graph::{BlockStorage, Triple};
use std::ops::ControlFlow;

/// A dense predicate id that matches nothing (outside every domain).
pub const NO_PREDICATE: u32 = u32::MAX;

/// An epsilon-free automaton: what `PathAutomaton::new` takes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Nfa {
    pub states: u32,
    pub start: u32,
    pub accepting: Vec<u32>,
    pub transitions: Vec<(u32, u32, Direction, u32)>,
}

impl Nfa {
    pub fn automaton(&self) -> PathAutomaton {
        let transitions = self.transitions.iter().map(|&(from, to, direction, p)| Transition { from, to, step: Step { direction, predicate: Some(p) } }).collect();
        PathAutomaton::new(self.states, self.start, &self.accepting, transitions).expect("compiled automata are well-formed")
    }

    /// The automaton of the reversed language: walking it from a path's end
    /// reaches the path's starts. A fresh start state stands for "any
    /// accepting state" of the original.
    pub fn reversed(&self) -> Nfa {
        let start = self.states;
        let flip = |d: Direction| match d {
            Direction::Outgoing => Direction::Incoming,
            Direction::Incoming => Direction::Outgoing,
            Direction::Both => Direction::Both,
        };
        let mut transitions = Vec::with_capacity(self.transitions.len() * 2);
        for &(from, to, d, p) in &self.transitions {
            transitions.push((to, from, flip(d), p));
            if self.accepting.contains(&to) {
                transitions.push((start, from, flip(d), p));
            }
        }
        let mut accepting = vec![self.start];
        if self.accepting.contains(&self.start) {
            accepting.push(start);
        }
        Nfa { states: self.states + 1, start, accepting, transitions }
    }
}

/// Thompson construction with epsilon edges, then epsilon elimination.
#[derive(Default)]
pub struct NfaBuilder {
    states: u32,
    eps: Vec<(u32, u32)>,
    steps: Vec<(u32, u32, Direction, u32)>,
}

impl NfaBuilder {
    pub fn state(&mut self) -> u32 {
        self.states += 1;
        self.states - 1
    }

    pub fn epsilon(&mut self, from: u32, to: u32) {
        self.eps.push((from, to));
    }

    pub fn step(&mut self, from: u32, to: u32, direction: Direction, predicate: u32) {
        self.steps.push((from, to, direction, predicate));
    }

    /// Eliminates epsilon edges: `q -x-> r` for every `p` in `q`'s epsilon
    /// closure with `p -x-> r`; `q` accepts if its closure reaches `end`.
    pub fn finish(self, start: u32, end: u32) -> Nfa {
        let n = self.states as usize;
        let mut eps_from: Vec<Vec<u32>> = vec![Vec::new(); n];
        for &(a, b) in &self.eps {
            eps_from[a as usize].push(b);
        }
        let mut steps_from: Vec<Vec<(u32, Direction, u32)>> = vec![Vec::new(); n];
        for &(a, b, d, p) in &self.steps {
            steps_from[a as usize].push((b, d, p));
        }
        let mut transitions = Vec::new();
        let mut accepting = Vec::new();
        let mut seen = vec![u32::MAX; n];
        let mut stack = Vec::new();
        for q in 0..self.states {
            stack.push(q);
            seen[q as usize] = q;
            let mut accepts = false;
            while let Some(p) = stack.pop() {
                accepts |= p == end;
                for &(r, d, pred) in &steps_from[p as usize] {
                    transitions.push((q, r, d, pred));
                }
                for &r in &eps_from[p as usize] {
                    if seen[r as usize] != q {
                        seen[r as usize] = q;
                        stack.push(r);
                    }
                }
            }
            if accepts {
                accepting.push(q);
            }
        }
        transitions.sort_unstable_by_key(|&(a, b, d, p)| (a, b, d as u8, p));
        transitions.dedup();
        Nfa { states: self.states, start, accepting, transitions }
    }
}

/// The active graph(s) as s-graph's traversal sees them.
pub enum View<'a, S: BlockStorage> {
    Empty,
    One(&'a Partition<S>),
    Merged(&'a [&'a Partition<S>]),
}

impl<S: BlockStorage> Clone for View<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<S: BlockStorage> Copy for View<'_, S> {}

impl<'a, S: BlockStorage> View<'a, S> {
    pub fn scan(self, bounds: [Option<u32>; 3]) -> ViewScan<'a, S> {
        match self {
            View::Empty => ViewScan::new(&[], None, bounds),
            View::One(p) => ViewScan::new(&[], Some(p.scan(bounds)), bounds),
            View::Merged(parts) => ViewScan::new(parts, None, bounds),
        }
    }

    pub fn contains(self, s: u32, p: u32, o: u32) -> bool {
        match self {
            View::Empty => false,
            View::One(part) => part.contains(s, p, o),
            View::Merged(parts) => parts.iter().any(|part| part.contains(s, p, o)),
        }
    }

    /// The graphs the view reads.
    fn parts(&self) -> &[&'a Partition<S>] {
        match self {
            View::Empty => &[],
            View::One(part) => std::slice::from_ref(part),
            View::Merged(parts) => parts,
        }
    }

    /// Adds to `set` every `dim` end (0: subject, 2: object) of the triples
    /// with dense predicate `predicate`: s-graph streams the tree's
    /// projection onto that position (`restricted_descent_visit`, pruned by
    /// the predicate at every level) and the set absorbs repeats, so
    /// nothing is buffered or sorted.
    pub fn ends_into(self, predicate: u32, dim: usize, set: &mut IdSet) {
        let restrictions = [DimRestriction::Free, DimRestriction::Exact(predicate), DimRestriction::Free];
        for part in self.parts() {
            let _: ControlFlow<()> = part.tree().restricted_descent_visit(restrictions, dim, |id| {
                set.insert(id);
                ControlFlow::Continue(())
            });
        }
    }

    /// Adds to `set` every subject and object: the nodes of the graph.
    /// One scan per graph; the set itself merges graphs, so a merged view
    /// needs none of [`ViewScan`]'s cross-graph checks.
    pub fn nodes_into(self, set: &mut IdSet) {
        for part in self.parts() {
            part.scan([None; 3]).for_each(|(s, _, o)| {
                set.insert(s);
                set.insert(o);
            });
        }
    }

    /// [`Self::contains`] for every probe, into `out` in input order, with
    /// one batched s-graph descent per graph; `scratch` holds s-graph's
    /// sort buffers and a merged view's per-graph answers.
    pub fn contains_batch_into(self, probes: &[Triple], out: &mut Vec<bool>, scratch: &mut BatchScratch) {
        match self {
            View::Empty => {
                out.clear();
                out.resize(probes.len(), false);
            }
            View::One(part) => part.contains_batch_into(probes, out, &mut scratch.sort),
            View::Merged(parts) => {
                out.clear();
                out.resize(probes.len(), false);
                for part in parts {
                    part.contains_batch_into(probes, &mut scratch.hits, &mut scratch.sort);
                    out.iter_mut().zip(scratch.hits.iter()).for_each(|(hit, &found)| *hit |= found);
                }
            }
        }
    }
}

/// Reusable buffers of [`View::contains_batch_into`].
#[derive(Default)]
pub struct BatchScratch {
    /// s-graph's probe sort.
    sort: ContainsBatchWorkspace,
    /// One graph's answers, in a merged view.
    hits: Vec<bool>,
}

/// Triples of a later merged graph checked one by one before switching to
/// batches: short scans (the common bound lookups) never allocate.
const SINGLE_CHECKS: usize = 64;

/// Triples of a later merged graph checked against the earlier ones per
/// s-graph `contains_batch` call.
const DEDUP_BATCH: usize = 4096;

/// Scans each partition in turn; in a merged view, a triple is skipped when
/// an earlier partition holds it too. Past [`SINGLE_CHECKS`] triples of one
/// graph, that check runs in batches of [`DEDUP_BATCH`].
pub struct ViewScan<'a, S: BlockStorage> {
    parts: &'a [&'a Partition<S>],
    index: usize,
    current: Option<Scan<'a, S>>,
    bounds: [Option<u32>; 3],
    /// Triples of the current graph checked singly so far.
    singles: usize,
    /// A checked batch: its triples, which an earlier graph holds, and the
    /// next one to yield.
    pending: Vec<Triple>,
    held: Vec<bool>,
    scratch: BatchScratch,
    next: usize,
}

impl<'a, S: BlockStorage> ViewScan<'a, S> {
    fn new(parts: &'a [&'a Partition<S>], current: Option<Scan<'a, S>>, bounds: [Option<u32>; 3]) -> Self {
        Self { parts, index: 0, current, bounds, singles: 0, pending: Vec::new(), held: Vec::new(), scratch: BatchScratch::default(), next: 0 }
    }
}

impl<S: BlockStorage> Iterator for ViewScan<'_, S> {
    type Item = Triple;

    fn next(&mut self) -> Option<Triple> {
        loop {
            while self.next < self.pending.len() {
                let i = self.next;
                self.next += 1;
                if !self.held[i] {
                    return Some(self.pending[i]);
                }
            }
            if let Some(scan) = &mut self.current {
                let earlier = &self.parts[..self.index.saturating_sub(1)];
                if earlier.is_empty() {
                    if let Some(t) = scan.next() {
                        return Some(t);
                    }
                } else if self.singles < SINGLE_CHECKS {
                    if let Some(t) = scan.next() {
                        self.singles += 1;
                        if !earlier.iter().any(|p| p.contains(t.0, t.1, t.2)) {
                            return Some(t);
                        }
                        continue;
                    }
                } else {
                    self.pending.clear();
                    self.pending.extend(scan.by_ref().take(DEDUP_BATCH));
                    self.next = 0;
                    if !self.pending.is_empty() {
                        View::Merged(earlier).contains_batch_into(&self.pending, &mut self.held, &mut self.scratch);
                        continue;
                    }
                }
                self.current = None;
            }
            let part = self.parts.get(self.index)?;
            self.index += 1;
            self.singles = 0;
            self.current = Some(part.scan(self.bounds));
        }
    }
}

impl<'a, S: BlockStorage> PatternBackend for View<'a, S> {
    type Scan<'b>
        = ViewScan<'a, S>
    where
        Self: 'b;

    fn scan_pattern(&self, bounds: [Option<u32>; 3]) -> ViewScan<'a, S> {
        self.scan(bounds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s_graph::automaton::AutomatonWorkspace;
    use s_graph::storage::HeapStorage;
    use s_graph::traversal::HopRange;
    use std::ops::ControlFlow;

    fn reach(nfa: &Nfa, view: View<'_, HeapStorage>, start: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut ws = AutomatonWorkspace::default();
        let _: ControlFlow<()> = nfa.automaton().reachable(&view, start, HopRange::new(0, u32::MAX).unwrap(), &mut ws, |n, _| {
            out.push(n);
            ControlFlow::Continue(())
        });
        out.sort_unstable();
        out
    }

    /// `p*`: start = end with a loop.
    fn star(p: u32) -> Nfa {
        let mut b = NfaBuilder::default();
        let (s, e, s1, e1) = (b.state(), b.state(), b.state(), b.state());
        b.step(s1, e1, Direction::Outgoing, p);
        b.epsilon(s, s1);
        b.epsilon(s, e);
        b.epsilon(e1, s1);
        b.epsilon(e1, e);
        b.finish(s, e)
    }

    #[test]
    fn epsilon_elimination_preserves_the_language() {
        let part = Partition::<HeapStorage>::from_triples(vec![(1, 0, 2), (2, 0, 3), (3, 1, 4)], 4096);
        let nfa = star(0);
        assert!(nfa.accepting.contains(&nfa.start), "zero-length accepted");
        assert_eq!(reach(&nfa, View::One(&part), 1), vec![1, 2, 3]);
        assert_eq!(reach(&nfa, View::One(&part), 9), vec![9], "an isolated start still matches itself");
        assert_eq!(reach(&nfa.reversed(), View::One(&part), 3), vec![1, 2, 3]);
    }

    #[test]
    fn reversed_one_or_more() {
        let part = Partition::<HeapStorage>::from_triples(vec![(1, 0, 2), (2, 0, 3)], 4096);
        let mut b = NfaBuilder::default();
        let (s, e, s1, e1) = (b.state(), b.state(), b.state(), b.state());
        b.step(s1, e1, Direction::Outgoing, 0);
        b.epsilon(s, s1);
        b.epsilon(e1, s1);
        b.epsilon(e1, e);
        let plus = b.finish(s, e);
        assert_eq!(reach(&plus, View::One(&part), 1), vec![2, 3]);
        assert_eq!(reach(&plus.reversed(), View::One(&part), 3), vec![1, 2]);
        assert_eq!(reach(&plus.reversed(), View::One(&part), 1), Vec::<u32>::new());
    }

    #[test]
    fn large_merged_scans_deduplicate_in_batches() {
        let a = Partition::<HeapStorage>::from_triples((0..6000).filter(|i| i % 2 == 0).map(|i| (i, 0, i + 1)).collect(), 4096);
        let b = Partition::<HeapStorage>::from_triples((0..6000).filter(|i| i % 3 == 0).map(|i| (i, 0, i + 1)).collect(), 4096);
        let c = Partition::<HeapStorage>::from_triples((0..6000).map(|i| (i, 0, i + 1)).collect(), 4096);
        let parts = [&a, &b, &c];
        let mut all: Vec<Triple> = View::Merged(&parts).scan([None; 3]).collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!((n, all.len()), (6000, 6000));
        assert_eq!(View::Merged(&parts).scan([None, None, Some(7)]).collect::<Vec<_>>(), vec![(6, 0, 7)]);
    }

    #[test]
    fn merged_views_deduplicate_across_graphs() {
        let a = Partition::<HeapStorage>::from_triples(vec![(1, 0, 2), (5, 0, 6)], 4096);
        let b = Partition::<HeapStorage>::from_triples(vec![(1, 0, 2), (2, 0, 3)], 4096);
        let parts = [&a, &b];
        let view = View::Merged(&parts);
        let mut all: Vec<Triple> = view.scan([None; 3]).collect();
        all.sort_unstable();
        assert_eq!(all, vec![(1, 0, 2), (2, 0, 3), (5, 0, 6)]);
        assert_eq!(reach(&star(0), view, 1), vec![1, 2, 3], "paths cross graphs of a merge");
        assert!(view.contains(2, 0, 3));
        let (mut out, mut scratch) = (Vec::new(), BatchScratch::default());
        view.contains_batch_into(&[(2, 0, 3), (9, 0, 9), (5, 0, 6), (1, 0, 2)], &mut out, &mut scratch);
        assert_eq!(out, vec![true, false, true, true], "a triple is in a merge if any graph holds it");
        View::One(&b).contains_batch_into(&[(5, 0, 6), (2, 0, 3)], &mut out, &mut scratch);
        assert_eq!(out, vec![false, true]);
        View::<HeapStorage>::Empty.contains_batch_into(&[(5, 0, 6)], &mut out, &mut scratch);
        assert_eq!(out, vec![false]);
        assert_eq!(View::<HeapStorage>::Empty.scan([None; 3]).count(), 0);
    }
}
