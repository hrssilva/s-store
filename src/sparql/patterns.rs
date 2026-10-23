//! Triple patterns and paths, delegated to s-graph.
//!
//! A BGP over one graph is one s-graph automaton ([`automata::Bgp`]): its
//! patterns are the atoms of one register automaton that s-graph runs over
//! the graph's FST, all patterns at once, streaming bindings without
//! building relations. The automaton is compiled once per query and graph
//! ([`BgpSearch`]); every evaluation passes the row's current bindings as
//! parameters (`Search::visit_matching`), which s-graph pushes into the
//! traversal, so a BGP joined per left solution is never recompiled. A BGP
//! over a merged default graph (`FROM` several graphs) joins pattern by
//! pattern over merged scans, most-bound pattern first, since an automaton
//! runs over one FST.
//!
//! Predicates are dense ids in the FSTs. A variable seen in predicate
//! position *and* in subject/object position is two automaton variables,
//! one per id space, checked equal (through the predicate table) on output.

use super::exec::{Active, CONTINUE, Ctx, Env, Flow, Sink};
use super::path::{BatchScratch, View};
use super::plan::*;
use crate::dict::LOCAL_ID_BASE;
use crate::graph::Partition;
use s_graph::automata::{self, Binding, MutableFstSource, Schedule, Search, Term};
use s_graph::query_ops::BgpWorkspace;
use s_graph::traversal::{Direction, HopRange};
use s_graph::{BlockStorage, Triple};
use std::ops::ControlFlow;
use std::rc::Rc;

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
    scratch: BatchScratch,
    current: Vec<u32>,
}

/// The traversal schedule of a BGP automaton with `atoms` triple patterns.
/// No s-graph schedule is safe on every shape (4.9M triples, s-graph
/// 4833f68). Up to three atoms, `Levels` (all atoms advance one FST level
/// at a time) is the fastest or close to it: a 2-atom star 16 ms against
/// 800 ms under `Adaptive`, `?a ?p ?b . ?b a ?c` 0.2 s against 6 s, 3-atom
/// stars and chains 0.2-0.5 s against 1.4-4.4 s nested; `Adaptive` took
/// over 100 s on `?a link ?b . ?b link ?c . ?c a C`. From four atoms on
/// `Levels` blows up (a 4-atom chain 14-23 s, a 4-atom star or 5-atom
/// chain over 100 s), while `Adaptive` and its connected orders matched or
/// beat `Nested` on every shape measured (0.7-7 s, chains up to 10 atoms).
/// See DESIGN.md.
fn schedule(atoms: usize) -> Schedule {
    if atoms <= 3 { Schedule::Levels } else { Schedule::Adaptive }
}

/// A BGP compiled for one graph: an s-graph search and its variables.
pub struct BgpSearch<'q, S: BlockStorage> {
    /// `None` when a constant cannot occur (a predicate no graph uses).
    search: Option<s_graph::query_ops::Bgp<MutableFstSource<'q, S>>>,
    /// The automaton's variables by index: `(slot, predicate position?)`.
    vars: Vec<(Slot, bool)>,
}

/// Reusable buffers of a [`BgpSearch`] run (pooled in `Env`): s-graph's
/// workspace and the parameters taken from the row.
#[derive(Default)]
pub struct BgpBuffers {
    ws: BgpWorkspace<u64>,
    values: Vec<u32>,
    bound: Vec<u64>,
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

impl<'q, S: BlockStorage> Ctx<'q, S> {
    /// Resolves a pattern's bounds in FST coordinates; `None` when a bound
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

    /// `t`'s bounds in FST coordinates with `row` substituted (`None`
    /// for a free position); `None` when a bound term cannot occur.
    pub(super) fn pattern_bounds(&self, t: &PTriple, row: &[u32]) -> Option<[Option<u32>; 3]> {
        self.bounds(t, row).map(|(bounds, _)| bounds)
    }

    pub(super) fn bgp(&self, env: &mut Env, range: Range, active: Active, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let triples = self.plan.triples(range);
        if triples.is_empty() {
            return k(env, row);
        }
        if let Some(part) = self.single(active) {
            let key = (range.start, range.len, active.cache_key());
            let cached = self.searches.borrow().get(&key).cloned();
            let search = cached.unwrap_or_else(|| {
                let search = Rc::new(self.compile_bgp(part, triples));
                self.searches.borrow_mut().insert(key, search.clone());
                search
            });
            return self.run_bgp(env, &search, row, k);
        }
        match self.view(active) {
            View::Empty => CONTINUE,
            view => self.nested_loop(env, view, triples, 0, row, k),
        }
    }

    /// Compiles `triples` into one s-graph automaton over `part`. Every
    /// variable stays a variable (bindings arrive as parameters at run
    /// time), so the search serves every row.
    fn compile_bgp(&self, part: &'q Partition<S>, triples: &[PTriple]) -> BgpSearch<'q, S> {
        let mut vars: Vec<(Slot, bool)> = Vec::new();
        let mut patterns = Vec::with_capacity(triples.len());
        for t in triples {
            let mut terms = [Term::Const(0); 3];
            for (i, position) in [t.s, t.p, t.o].into_iter().enumerate() {
                terms[i] = match position {
                    PTerm::Var(slot) => {
                        let var = (slot, i == 1);
                        Term::Var(vars.iter().position(|&v| v == var).unwrap_or_else(|| {
                            vars.push(var);
                            vars.len() - 1
                        }))
                    }
                    PTerm::Const(id) if i == 1 => match self.dense(id) {
                        Some(dense) => Term::Const(dense),
                        None => return BgpSearch { search: None, vars },
                    },
                    // A query-local id lies outside every FST domain, which
                    // s-graph compiles to an empty search.
                    PTerm::Const(id) => Term::Const(id),
                };
            }
            patterns.push(terms);
        }
        let atoms = patterns.len();
        let machine = patterns.into_iter().fold(automata::Bgp::new(vars.len()), automata::Bgp::triple).schedule(schedule(atoms));
        let search = part.tree().search(&machine).expect("a well-formed BGP over valid domains");
        BgpSearch { search: Some(search), vars }
    }

    /// Runs a compiled BGP with `row`'s bindings as parameters, binding its
    /// variables for each solution.
    fn run_bgp(&self, env: &mut Env, bgp: &BgpSearch<'q, S>, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let Some(search) = &bgp.search else { return CONTINUE };
        let mut b = Self::bgp_buffers(env);
        let width = bgp.vars.len();
        b.values.clear();
        b.values.resize(width, 0);
        b.bound.clear();
        b.bound.resize(width.div_ceil(64), 0);
        let mut parameters = false;
        for (j, &(slot, predicate)) in bgp.vars.iter().enumerate() {
            let value = row[slot as usize];
            if value == UNBOUND {
                continue;
            }
            let Some(value) = (if predicate { self.dense(value) } else { (value < LOCAL_ID_BASE).then_some(value) }) else {
                Self::return_bgp_buffers(env, b);
                return CONTINUE;
            };
            b.values[j] = value;
            b.bound[j / 64] |= 1 << (j % 64);
            parameters = true;
        }
        let BgpBuffers { ws, values, bound } = &mut b;
        let mut newly: Vec<Slot> = Vec::new();
        let mut emit = |binding: Binding<'_>| {
            newly.clear();
            for (j, &(slot, predicate)) in bgp.vars.iter().enumerate() {
                let value = if predicate { self.predicate_term(binding.values[j]) } else { binding.values[j] };
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
        };
        let flow = if parameters { search.visit_matching(ws, Binding { values, bound }, &mut emit) } else { search.visit(ws, &mut emit) };
        Self::return_bgp_buffers(env, b);
        flow
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
    pub(super) fn set_join(&self, env: &mut Env, left: NodeId, joined: Range, slot: Slot, kind: SetJoinKind, active: Active, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
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

    /// `t` with `solution` substituted, in FST coordinates; [`MISS`] when a
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
                    (Pos::Free(ss), Pos::Free(os)) => self.walk_all(env, path, &automata[0], view, ss, os, row, k),
                }
            }
        }
    }

    /// A path with both ends free. Only nodes with an edge matching one of
    /// the start state's transitions can take a step, so the walks start
    /// from those alone (found by s-graph projections, not by trying every
    /// node); when the path matches zero-length paths, every other node of
    /// the graph pairs with itself only. Nodes go in id order either way.
    #[allow(clippy::too_many_arguments)]
    fn walk_all(&self, env: &mut Env, path: PathId, automaton: &s_graph::automaton::PathAutomaton, view: View<'_, S>, ss: Slot, os: Slot, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let PathPlan::Automaton { automaton: nfa, zero_length } = &self.plan.paths[path as usize] else { unreachable!("automaton path") };
        let mut starts = self.id_set(env);
        for &(_, _, direction, p) in nfa.transitions.iter().filter(|t| t.0 == nfa.start) {
            if direction != Direction::Incoming {
                view.ends_into(p, 0, &mut starts);
            }
            if direction != Direction::Outgoing {
                view.ends_into(p, 2, &mut starts);
            }
        }
        starts.freeze();
        let nodes = zero_length.then(|| {
            let mut nodes = self.id_set(env);
            view.nodes_into(&mut nodes);
            nodes.freeze();
            nodes
        });
        let mut flow = CONTINUE;
        for start in nodes.as_ref().unwrap_or(&starts).iter() {
            row[ss as usize] = start;
            flow = if nodes.is_none() || starts.contains(start) {
                self.walk(env, automaton, view, start, &mut |env, row, node| self.emit_pair(env, Pos::Bound(start), Pos::Free(os), start, node, row, k), row)
            } else {
                self.emit_pair(env, Pos::Bound(start), Pos::Free(os), start, start, row, k)
            };
            row[ss as usize] = UNBOUND;
            if flow.is_break() {
                break;
            }
        }
        if let Some(nodes) = nodes {
            Self::return_id_set(env, nodes);
        }
        Self::return_id_set(env, starts);
        flow
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
