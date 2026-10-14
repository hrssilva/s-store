//! Triple patterns and paths, delegated to s-graph.
//!
//! A BGP over one graph with more than one pattern becomes one s-graph
//! [`NativeJoin`]: bound slots are substituted as constants, the remaining
//! variables are join variables, and s-graph traverses the S/P/O trie for
//! all patterns at once, streaming bindings without building relations.
//! A single pattern is one bounded [`View::scan`]. A BGP over a merged
//! default graph (`FROM` several graphs) joins pattern by pattern over
//! merged scans, most-bound pattern first, since a native join runs within
//! one tree.
//!
//! Predicates are dense ids in the trees. A variable seen in predicate
//! position *and* in subject/object position is two join variables, one per
//! id space, checked equal (through the predicate table) on output.

use super::exec::{Active, CONTINUE, Ctx, Env, Flow, Sink};
use super::path::View;
use super::plan::*;
use crate::dict::LOCAL_ID_BASE;
use crate::graph::Partition;
use s_graph::{BlockStorage, Triple};
use s_graph::graph::{JoinQuery, Term as JTerm, TriplePattern as JTriple};
use s_graph::native_join::NativeJoin;
use s_graph::traversal::HopRange;
use std::ops::ControlFlow;

/// How many solutions a [`Node::Check`] buffers before probing them all in
/// one batch: large enough for s-graph to share most of the descents,
/// small enough to keep the buffer (`CHECK_BATCH` rows) in cache-sized
/// memory and to bound the extra work done past a LIMIT.
const CHECK_BATCH: usize = 4096;

/// A probe that matches nothing: `u32::MAX` is never a stored id.
const MISS: Triple = (u32::MAX, u32::MAX, u32::MAX);

/// Reusable buffers of a [`Node::Check`] (pooled in `Env`): the buffered
/// solutions (row-major), their probes, and s-graph's answers.
#[derive(Default)]
pub struct CheckBuffers {
    len: usize,
    rows: Vec<u32>,
    probes: Vec<Triple>,
    hits: Vec<bool>,
    scratch: Vec<bool>,
    current: Vec<u32>,
}

/// A pattern position after substituting the current row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pos {
    Bound(u32),
    Free(Slot),
}

fn resolve(t: PTerm, row: &[u32]) -> Pos {
    match t {
        PTerm::Const(id) => Pos::Bound(id),
        PTerm::Var(s) if row[s as usize] != UNBOUND => Pos::Bound(row[s as usize]),
        PTerm::Var(s) => Pos::Free(s),
    }
}

/// Binds `slot` to `value` unless already bound; `None` on a conflict
/// (a variable repeated within one pattern), else whether it was bound now.
fn bind(row: &mut [u32], slot: Slot, value: u32) -> Option<bool> {
    let cell = &mut row[slot as usize];
    if *cell == UNBOUND {
        *cell = value;
        Some(true)
    } else if *cell == value {
        Some(false)
    } else {
        None
    }
}

impl<S: BlockStorage> Ctx<'_, S> {
    /// Resolves a pattern's bounds in tree coordinates; `None` when a bound
    /// term cannot occur (an unknown predicate, a query-local id).
    fn bounds(&self, t: &PTriple, row: &[u32]) -> Option<([Option<u32>; 3], [Pos; 3])> {
        let pos = [resolve(t.s, row), resolve(t.p, row), resolve(t.o, row)];
        let mut bounds = [None; 3];
        for (i, p) in pos.iter().enumerate() {
            if let Pos::Bound(id) = *p {
                if id >= LOCAL_ID_BASE {
                    return None;
                }
                bounds[i] = Some(if i == 1 { self.dense(id)? } else { id });
            }
        }
        Some((bounds, pos))
    }

    /// `t`'s bounds in tree coordinates with `row` substituted (`None`
    /// for a free position); `None` when a bound term cannot occur.
    pub(super) fn pattern_bounds(&self, t: &PTriple, row: &[u32]) -> Option<[Option<u32>; 3]> {
        self.bounds(t, row).map(|(bounds, _)| bounds)
    }

    pub(super) fn bgp(&self, env: &mut Env, triples: &[PTriple], active: Active, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        if triples.is_empty() {
            return k(env, row);
        }
        match self.view(active) {
            View::Empty => CONTINUE,
            View::One(part) if triples.len() > 1 => self.native_join(env, part, triples, row, k),
            view => self.nested_loop(env, view, triples, 0, row, k),
        }
    }

    /// Binds the free positions of one matched triple, calls `k`, unbinds.
    fn emit_triple(&self, env: &mut Env, pos: &[Pos; 3], t: (u32, u32, u32), row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let values = [t.0, self.predicate_term(t.1), t.2];
        let mut newly = [None; 3];
        for i in 0..3 {
            if let Pos::Free(slot) = pos[i] {
                match bind(row, slot, values[i]) {
                    Some(true) => newly[i] = Some(slot),
                    Some(false) => {}
                    None => {
                        newly.iter().flatten().for_each(|&s| row[s as usize] = UNBOUND);
                        return CONTINUE;
                    }
                }
            }
        }
        let flow = k(env, row);
        newly.iter().flatten().for_each(|&s| row[s as usize] = UNBOUND);
        flow
    }

    /// Pattern-at-a-time join; `done` marks evaluated patterns (BGPs past 64
    /// patterns are evaluated in written order).
    fn nested_loop(&self, env: &mut Env, view: View<'_, S>, triples: &[PTriple], done: u64, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let remaining = |i: usize| i >= 64 || done & (1 << i) == 0;
        let next = if triples.len() > 64 {
            (done.count_ones() as usize..triples.len()).next()
        } else {
            (0..triples.len())
                .filter(|&i| remaining(i))
                .max_by_key(|&i| {
                    let t = &triples[i];
                    let bound = |p: PTerm| u32::from(matches!(resolve(p, row), Pos::Bound(_)));
                    (2 * bound(t.s) + bound(t.p) + 2 * bound(t.o), std::cmp::Reverse(i))
                })
        };
        let Some(i) = next.filter(|&i| i < triples.len()) else { return k(env, row) };
        let done = if triples.len() > 64 { done + 1 } else { done | (1 << i) };
        let Some((bounds, pos)) = self.bounds(&triples[i], row) else { return CONTINUE };
        for t in view.scan(bounds) {
            self.emit_triple(env, &pos, t, row, &mut |env, row| self.nested_loop(env, view, triples, done, row, k))?;
        }
        CONTINUE
    }

    fn native_join(&self, env: &mut Env, part: &Partition<S>, triples: &[PTriple], row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        // Join variables: (slot, predicate position?) pairs, in first-use order.
        let mut vars: Vec<(Slot, bool)> = Vec::new();
        let mut patterns = Vec::with_capacity(triples.len());
        let mut var_of = |slot: Slot, predicate: bool| -> usize {
            match vars.iter().position(|&v| v == (slot, predicate)) {
                Some(i) => i,
                None => {
                    vars.push((slot, predicate));
                    vars.len() - 1
                }
            }
        };
        for t in triples {
            let Some((bounds, pos)) = self.bounds(t, row) else { return CONTINUE };
            let term = |i: usize, var_of: &mut dyn FnMut(Slot, bool) -> usize| match (bounds[i], pos[i]) {
                (Some(c), _) => JTerm::Const(c),
                (None, Pos::Free(slot)) => JTerm::Var(var_of(slot, i == 1)),
                (None, Pos::Bound(_)) => unreachable!("bound positions have bounds"),
            };
            patterns.push(JTriple { subject: term(0, &mut var_of), predicate: term(1, &mut var_of), object: term(2, &mut var_of) });
        }
        let query = JoinQuery { num_vars: vars.len(), patterns };
        let Ok(plan) = NativeJoin::new(&query, part.tree()) else { return CONTINUE };
        let mut ws = Self::join_workspace(env);
        let mut newly: Vec<Slot> = Vec::with_capacity(vars.len());
        let flow = plan.visit(&mut ws, |binding| {
            newly.clear();
            for (j, &(slot, predicate)) in vars.iter().enumerate() {
                let value = if predicate { self.predicate_term(binding[j]) } else { binding[j] };
                match bind(row, slot, value) {
                    Some(true) => newly.push(slot),
                    Some(false) => {}
                    None => {
                        newly.iter().for_each(|&s| row[s as usize] = UNBOUND);
                        return CONTINUE;
                    }
                }
            }
            let flow = k(env, row);
            newly.iter().for_each(|&s| row[s as usize] = UNBOUND);
            flow
        });
        Self::return_join_workspace(env, ws);
        flow
    }

    /// [`Node::Check`]: buffers `input`'s solutions, probes every pattern of
    /// each with the solution substituted in one s-graph batch, and passes
    /// on, in their original order, those whose patterns are all present
    /// (or, `negated`, not all present).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn check(&self, env: &mut Env, input: NodeId, triples: &[PTriple], negated: bool, active: Active, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let view = self.view(active);
        let mut buffers = Self::check_buffers(env);
        buffers.rows.clear();
        buffers.len = 0;
        let mut flow = self.eval(env, input, active, row, &mut |env, row| {
            buffers.rows.extend_from_slice(row);
            buffers.len += 1;
            if buffers.len == CHECK_BATCH { self.flush_checks(env, &mut buffers, view, triples, negated, row, k) } else { CONTINUE }
        });
        if flow.is_continue() && buffers.len > 0 {
            flow = self.flush_checks(env, &mut buffers, view, triples, negated, row, k);
        }
        Self::return_check_buffers(env, buffers);
        flow
    }

    #[allow(clippy::too_many_arguments)]
    fn flush_checks(&self, env: &mut Env, b: &mut CheckBuffers, view: View<'_, S>, triples: &[PTriple], negated: bool, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let width = row.len();
        let n = b.len;
        b.probes.clear();
        for i in 0..n {
            let solution = &b.rows[i * width..(i + 1) * width];
            b.probes.extend(triples.iter().map(|t| self.probe(t, solution)));
        }
        view.contains_batch_into(&b.probes, &mut b.hits, &mut b.scratch);
        b.current.clear();
        b.current.extend_from_slice(row);
        let mut flow = CONTINUE;
        let m = triples.len();
        for i in 0..n {
            if b.hits[i * m..(i + 1) * m].iter().all(|&hit| hit) == negated {
                continue;
            }
            row.copy_from_slice(&b.rows[i * width..(i + 1) * width]);
            flow = k(env, row);
            if flow.is_break() {
                break;
            }
        }
        row.copy_from_slice(&b.current);
        b.rows.clear();
        b.len = 0;
        flow
    }

    /// [`Node::SetJoin`]: one native join of both BGPs (`joined`) records
    /// which `slot` values have a match (passing the joined solutions on
    /// for an OPTIONAL), then one pass over `left` keeps, drops or adds its
    /// solutions by membership. Two s-graph traversals and one bit per
    /// dictionary id, instead of a lookup or a probe per left solution.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn set_join(&self, env: &mut Env, left: NodeId, joined: &[PTriple], slot: Slot, kind: SetJoinKind, active: Active, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let s = slot as usize;
        let mut matched = self.id_set(env);
        let mut flow = self.bgp(env, joined, active, row, &mut |env, row| {
            matched.insert(row[s]);
            if kind == SetJoinKind::LeftOuter { k(env, row) } else { CONTINUE }
        });
        if flow.is_continue() {
            matched.freeze();
            let keep = kind == SetJoinKind::Semi;
            flow = self.eval(env, left, active, row, &mut |env, row| if matched.contains(row[s]) == keep { k(env, row) } else { CONTINUE });
        }
        Self::return_id_set(env, matched);
        flow
    }

    /// `t` with `solution` substituted, in tree coordinates; [`MISS`] when a
    /// term cannot occur in the data.
    fn probe(&self, t: &PTriple, solution: &[u32]) -> Triple {
        match self.bounds(t, solution) {
            Some(([Some(s), Some(p), Some(o)], _)) => (s, p, o),
            _ => MISS,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn path(&self, env: &mut Env, s: PTerm, path: PathId, o: PTerm, active: Active, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let view = self.view(active);
        match &self.plan.paths[path as usize] {
            PathPlan::Negated { forward_any, forward, inverse_any, backward } => {
                let (sp, op) = (resolve(s, row), resolve(o, row));
                let bound = |p: Pos| match p {
                    Pos::Bound(id) if id >= LOCAL_ID_BASE => Err(()),
                    Pos::Bound(id) => Ok(Some(id)),
                    Pos::Free(_) => Ok(None),
                };
                let (Ok(sb), Ok(ob)) = (bound(sp), bound(op)) else { return CONTINUE };
                if *forward_any {
                    for t in view.scan([sb, None, ob]) {
                        if !forward.contains(&t.1) {
                            self.emit_pair(env, sp, op, t.0, t.2, row, k)?;
                        }
                    }
                }
                if *inverse_any {
                    for t in view.scan([ob, None, sb]) {
                        if !backward.contains(&t.1) {
                            self.emit_pair(env, sp, op, t.2, t.0, row, k)?;
                        }
                    }
                }
                CONTINUE
            }
            PathPlan::Automaton { .. } => {
                let automata = self.automata(env, path);
                match (resolve(s, row), resolve(o, row)) {
                    (Pos::Bound(a), Pos::Bound(b)) => {
                        let found = self.walk(env, &automata[0], view, a, &mut |_, _, node| if node == b { ControlFlow::Break(()) } else { CONTINUE }, row);
                        if found.is_break() { k(env, row) } else { CONTINUE }
                    }
                    (Pos::Bound(a), Pos::Free(slot)) => self.walk(env, &automata[0], view, a, &mut |env, row, node| self.emit_pair(env, Pos::Bound(a), Pos::Free(slot), a, node, row, k), row),
                    (Pos::Free(slot), Pos::Bound(b)) => self.walk(env, &automata[1], view, b, &mut |env, row, node| self.emit_pair(env, Pos::Free(slot), Pos::Bound(b), node, b, row, k), row),
                    (Pos::Free(ss), Pos::Free(os)) => {
                        let mut nodes: Vec<u32> = view.scan([None; 3]).flat_map(|t| [t.0, t.2]).collect();
                        nodes.sort_unstable();
                        nodes.dedup();
                        for start in nodes {
                            row[ss as usize] = start;
                            let flow = self.walk(env, &automata[0], view, start, &mut |env, row, node| self.emit_pair(env, Pos::Bound(start), Pos::Free(os), start, node, row, k), row);
                            row[ss as usize] = UNBOUND;
                            flow?;
                        }
                        CONTINUE
                    }
                }
            }
        }
    }

    /// Binds a path's free ends to `(a, b)` and calls `k`.
    #[allow(clippy::too_many_arguments)]
    fn emit_pair(&self, env: &mut Env, s: Pos, o: Pos, a: u32, b: u32, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let mut newly = [None; 2];
        for (i, (p, v)) in [(s, a), (o, b)].into_iter().enumerate() {
            match p {
                Pos::Bound(id) if id != v => {
                    newly.iter().flatten().for_each(|&s: &Slot| row[s as usize] = UNBOUND);
                    return CONTINUE;
                }
                Pos::Bound(_) => {}
                Pos::Free(slot) => match bind(row, slot, v) {
                    Some(true) => newly[i] = Some(slot),
                    Some(false) => {}
                    None => {
                        newly.iter().flatten().for_each(|&s| row[s as usize] = UNBOUND);
                        return CONTINUE;
                    }
                },
            }
        }
        let flow = k(env, row);
        newly.iter().flatten().for_each(|&s| row[s as usize] = UNBOUND);
        flow
    }

    #[allow(clippy::too_many_arguments)]
    /// Runs an automaton from `start`, calling `emit` once per distinct
    /// endpoint.
    fn walk(&self, env: &mut Env, automaton: &s_graph::automaton::PathAutomaton, view: View<'_, S>, start: u32, emit: &mut dyn FnMut(&mut Env, &mut [u32], u32) -> Flow, row: &mut [u32]) -> Flow {
        let mut ws = Self::automaton_workspace(env);
        let hops = HopRange::new(0, u32::MAX).expect("valid range");
        let flow = automaton.reachable(&view, start, hops, &mut ws, |node, _| emit(env, row, node));
        Self::return_automaton_workspace(env, ws);
        flow
    }
}
