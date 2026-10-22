//! Push-based plan evaluation.
//!
//! Every operator takes the current row (one `u32` term id per slot), binds
//! the slots it produces, passes the row on to its continuation, and unbinds
//! them again: the evaluator backtracks over one row buffer instead of
//! building solution sequences. Continuations return [`ControlFlow`], so
//! LIMIT, ASK and EXISTS stop the whole pipeline early, including s-graph's
//! own traversals.
//!
//! Intermediate results exist only where the algebra needs them:
//! [`JoinMode::Materialize`] right sides (once per active graph, cached),
//! MINUS right sides, [`Node::SetJoin`]'s id sets, the blocking operators (ORDER BY, GROUP BY,
//! DISTINCT's seen-set), and bounded batches of rows awaiting a
//! [`Node::Check`].

use super::locals::LocalTerms;
use super::path::View;
use super::idset::IdSet;
use super::patterns::{BgpBuffers, BgpSearch, CheckBuffers};
use super::plan::*;
use super::value::Arena;
use crate::graph::{GraphName, Partition};
use crate::store::Store;
use crate::term::KeyBuf;
use s_dict::TermId;
use s_graph::BlockStorage;
use s_graph::automaton::{AutomatonWorkspace, PathAutomaton};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::rc::Rc;

pub type Flow = ControlFlow<()>;
pub type Sink<'s> = dyn FnMut(&mut Env, &mut [u32]) -> Flow + 's;

pub const CONTINUE: Flow = ControlFlow::Continue(());

/// The graph(s) patterns match against: the query's default graph, or one
/// named graph inside `GRAPH`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Active {
    Default,
    Named(u32),
}

impl Active {
    pub(super) fn cache_key(self) -> u64 {
        match self {
            Active::Default => u64::MAX,
            Active::Named(g) => u64::from(g),
        }
    }
}

/// A right-hand side evaluated once: rows over `slots`, sorted by
/// `key_column` when there is one.
pub struct Materialized {
    slots: Vec<Slot>,
    rows: Vec<u32>,
    key_column: Option<usize>,
}

impl Materialized {
    fn width(&self) -> usize {
        self.slots.len()
    }

    fn len(&self) -> usize {
        if self.slots.is_empty() { self.rows.len() } else { self.rows.len() / self.slots.len() }
    }

    fn row(&self, i: usize) -> &[u32] {
        let w = self.width();
        &self.rows[i * w..(i + 1) * w]
    }

    /// Rows that can be compatible with `row`: those with its key value, or
    /// all when there is no key or the row leaves it unbound.
    fn candidates(&self, row: &[u32]) -> std::ops::Range<usize> {
        let (Some(col), w) = (self.key_column, self.width()) else { return 0..self.len() };
        let value = row[self.slots[col] as usize];
        if value == UNBOUND {
            return 0..self.len();
        }
        let n = self.len();
        let key = |i: usize| self.rows[i * w + col];
        let lo = partition_point(n, |i| key(i) < value);
        let hi = partition_point(n, |i| key(i) <= value);
        lo..hi
    }
}

fn partition_point(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if pred(mid) { lo = mid + 1 } else { hi = mid }
    }
    lo
}

/// Mutable evaluation state, threaded through continuations.
pub struct Env {
    pub arena: Arena,
    pub locals: LocalTerms,
    pub key: Vec<u8>,
    pub keybuf: KeyBuf,
    pub scratch: String,
    pub saved: Vec<u32>,
    bgp_buffers: Vec<BgpBuffers>,
    check_buffers: Vec<CheckBuffers>,
    id_sets: Vec<IdSet>,
    automaton_workspaces: Vec<AutomatonWorkspace>,
    automata: HashMap<PathId, Rc<[PathAutomaton; 2]>>,
    materialized: HashMap<(NodeId, u64), Rc<Materialized>>,
    pub regexes: HashMap<(String, String), Option<Rc<regex::Regex>>>,
    pub fresh: u64,
    pub rng: u64,
    pub now: String,
}

impl Env {
    pub fn new(arena: Arena, locals: LocalTerms) -> Self {
        let since_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Env {
            arena,
            locals,
            key: Vec::new(),
            keybuf: KeyBuf::new(),
            scratch: String::new(),
            saved: Vec::new(),
            bgp_buffers: Vec::new(),
            check_buffers: Vec::new(),
            id_sets: Vec::new(),
            automaton_workspaces: Vec::new(),
            automata: HashMap::new(),
            materialized: HashMap::new(),
            regexes: HashMap::new(),
            fresh: 0,
            rng: since_epoch.as_nanos() as u64 | 1,
            now: format_now(since_epoch),
        }
    }

    pub fn into_locals(self) -> LocalTerms {
        self.locals
    }

    pub fn next_random(&mut self) -> u64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

fn format_now(d: std::time::Duration) -> String {
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem % 3600 / 60, rem % 60, d.subsec_millis())
}

/// A compiled BGP's cache key: its pattern range (start, length) and graph.
pub(super) type SearchKey = (u32, u32, u64);

/// Read-only evaluation context: the store, the plan, the dataset.
pub struct Ctx<'q, S: BlockStorage> {
    pub store: &'q Store<S>,
    pub plan: &'q Plan,
    default_parts: Vec<&'q Partition<S>>,
    /// Graphs `GRAPH ?g` ranges over, sorted.
    named: Vec<u32>,
    /// The reserved properties graph, reachable only by name
    /// (`GRAPH <urn:x-s-store:properties>`) when no FROM NAMED is given.
    properties: Option<u32>,
    /// BGPs compiled to s-graph searches, by (pattern range, graph): each
    /// is compiled once per query and graph, then rerun with each row's
    /// bindings as parameters.
    pub(super) searches: RefCell<HashMap<SearchKey, Rc<BgpSearch<'q, S>>>>,
}

impl<'q, S: BlockStorage> Ctx<'q, S> {
    pub fn new(store: &'q Store<S>, plan: &'q Plan) -> Self {
        let index = store.index();
        let default_parts = plan.dataset.default.iter().filter_map(|&g| index.partition(g)).collect();
        let reserved = store.properties_graph();
        let mut named: Vec<u32> = match &plan.dataset.named {
            Some(graphs) => graphs.iter().copied().filter(|&g| index.partition(GraphName::Named(TermId(g))).is_some()).collect(),
            None => index.named_graphs().map(|(g, _)| g.0).filter(|&g| Some(GraphName::Named(TermId(g))) != reserved).collect(),
        };
        named.sort_unstable();
        let properties = match (reserved, &plan.dataset.named) {
            (Some(GraphName::Named(g)), None) => Some(g.0),
            _ => None,
        };
        Ctx { store, plan, default_parts, named, properties, searches: RefCell::new(HashMap::new()) }
    }

    pub fn view(&self, active: Active) -> View<'_, S> {
        match active {
            Active::Default => match self.default_parts.as_slice() {
                [] => View::Empty,
                [one] => View::One(one),
                many => View::Merged(many),
            },
            Active::Named(g) => self.store.index().partition(GraphName::Named(TermId(g))).map_or(View::Empty, View::One),
        }
    }

    /// The active graph when it is exactly one graph, borrowed for the
    /// query's lifetime (what compiled searches hold).
    pub(super) fn single(&self, active: Active) -> Option<&'q Partition<S>> {
        match active {
            Active::Default => match self.default_parts.as_slice() {
                [one] => Some(*one),
                _ => None,
            },
            Active::Named(g) => self.store.index().partition(GraphName::Named(TermId(g))),
        }
    }

    fn is_named(&self, g: u32) -> bool {
        self.named.binary_search(&g).is_ok()
    }

    /// The dense predicate id of a predicate term id.
    pub fn dense(&self, term: u32) -> Option<u32> {
        self.store.index().predicates().get(TermId(term))
    }

    pub fn predicate_term(&self, dense: u32) -> u32 {
        self.store.index().predicates().term(dense).0
    }

    pub fn eval(&self, env: &mut Env, id: NodeId, active: Active, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        match self.plan.node(id) {
            Node::Unit => k(env, row),
            Node::Empty => CONTINUE,
            Node::Bgp(r) => self.bgp(env, r, active, row, k),
            Node::Path { s, path, o } => self.path(env, s, path, o, active, row, k),
            Node::Join(a, b, JoinMode::Bind) => self.eval(env, a, active, row, &mut |env, row| self.eval(env, b, active, row, k)),
            Node::Join(a, b, JoinMode::Materialize { key }) => {
                let m = self.materialize(env, b, active, key);
                self.eval(env, a, active, row, &mut |env, row| probe(env, &m, row, k))
            }
            Node::LeftJoin(a, b, cond, mode) => {
                let m = match mode {
                    JoinMode::Bind => None,
                    JoinMode::Materialize { key } => Some(self.materialize(env, b, active, key)),
                };
                self.eval(env, a, active, row, &mut |env, row| {
                    let mut matched = false;
                    let mut right = |env: &mut Env, row: &mut [u32]| {
                        if cond.is_some_and(|c| !self.ebv(env, c, row, active)) {
                            return CONTINUE;
                        }
                        matched = true;
                        k(env, row)
                    };
                    match &m {
                        None => self.eval(env, b, active, row, &mut right)?,
                        Some(m) => probe(env, m, row, &mut right)?,
                    }
                    if matched { CONTINUE } else { k(env, row) }
                })
            }
            Node::Union(a, b) => {
                self.eval(env, a, active, row, k)?;
                self.eval(env, b, active, row, k)
            }
            Node::Minus(a, b) => {
                let m = self.materialize(env, b, active, None);
                self.eval(env, a, active, row, &mut |env, row| if minus_hit(&m, row) { CONTINUE } else { k(env, row) })
            }
            Node::Graph(g, a) => match g {
                PTerm::Const(g) => {
                    if self.is_named(g) || self.properties == Some(g) { self.eval(env, a, Active::Named(g), row, k) } else { CONTINUE }
                }
                PTerm::Var(slot) => {
                    let bound = row[slot as usize];
                    if bound != UNBOUND {
                        return if self.is_named(bound) { self.eval(env, a, Active::Named(bound), row, k) } else { CONTINUE };
                    }
                    for &g in &self.named {
                        row[slot as usize] = g;
                        let flow = self.eval(env, a, Active::Named(g), row, k);
                        row[slot as usize] = UNBOUND;
                        flow?;
                    }
                    CONTINUE
                }
            },
            Node::SetJoin { left, joined, slot, kind } => self.set_join(env, left, joined, slot, kind, active, row, k),
            Node::Check { input, triples, negated } => self.check(env, input, self.plan.triples(triples), negated, active, row, k),
            Node::Filter(e, a) => self.eval(env, a, active, row, &mut |env, row| if self.ebv(env, e, row, active) { k(env, row) } else { CONTINUE }),
            Node::Extend(a, slot, e) => self.eval(env, a, active, row, &mut |env, row| {
                let s = slot as usize;
                match self.expr_id(env, e, row, active) {
                    None => k(env, row),
                    Some(id) if row[s] == UNBOUND => {
                        row[s] = id;
                        let flow = k(env, row);
                        row[s] = UNBOUND;
                        flow
                    }
                    Some(id) if row[s] == id => k(env, row),
                    Some(_) => CONTINUE,
                }
            }),
            Node::Group { input, keys, aggregates } => self.group(env, input, keys, aggregates, active, row, k),
            Node::OrderBy { input, keys, scope } => self.order_by(env, input, keys, scope, active, row, k),
            Node::Project { input, hidden } => {
                let hidden = self.plan.slots(hidden);
                self.eval(env, input, active, row, &mut |env, row| {
                    let mark = env.saved.len();
                    for &s in hidden {
                        env.saved.push(row[s as usize]);
                        row[s as usize] = UNBOUND;
                    }
                    let flow = k(env, row);
                    for (i, &s) in hidden.iter().enumerate() {
                        row[s as usize] = env.saved[mark + i];
                    }
                    env.saved.truncate(mark);
                    flow
                })
            }
            Node::Distinct { input, scope } => {
                let scope = self.plan.slots(scope);
                let mut seen = super::hash::RowSet::new(scope.len());
                let mut key = Vec::with_capacity(scope.len());
                self.eval(env, input, active, row, &mut |env, row| {
                    key.clear();
                    key.extend(scope.iter().map(|&s| row[s as usize]));
                    if seen.insert(&key).1 { k(env, row) } else { CONTINUE }
                })
            }
            Node::Slice { input, offset, limit } => {
                if limit == Some(0) {
                    return CONTINUE;
                }
                let (mut skipped, mut emitted, mut done) = (0u64, 0u64, false);
                let flow = self.eval(env, input, active, row, &mut |env, row| {
                    if skipped < offset {
                        skipped += 1;
                        return CONTINUE;
                    }
                    emitted += 1;
                    k(env, row)?;
                    if Some(emitted) == limit {
                        done = true;
                        return ControlFlow::Break(());
                    }
                    CONTINUE
                });
                if done { CONTINUE } else { flow }
            }
            Node::Table { slots, rows } => {
                let slots = self.plan.slots(slots);
                let values = self.plan.values(rows);
                if slots.is_empty() {
                    // `VALUES () { () () }`: one empty solution per row.
                    for _ in 0..rows.len.max(1) {
                        k(env, row)?;
                    }
                    return CONTINUE;
                }
                for table_row in values.chunks(slots.len()) {
                    let mark = env.saved.len();
                    if merge(row, slots, table_row, &mut env.saved) {
                        let flow = k(env, row);
                        unmerge(row, &mut env.saved, mark);
                        flow?;
                    } else {
                        unmerge(row, &mut env.saved, mark);
                    }
                }
                CONTINUE
            }
        }
    }

    /// Evaluates `node` once, independently of any outer bindings, keeping
    /// the slots it may bind. Cached per active graph.
    fn materialize(&self, env: &mut Env, node: NodeId, active: Active, key: Option<Slot>) -> Rc<Materialized> {
        let cache_key = (node, active.cache_key());
        if let Some(m) = env.materialized.get(&cache_key) {
            return m.clone();
        }
        let slots: Vec<Slot> = self.plan.scope(node).to_vec();
        let mut rows = Vec::new();
        let mut fresh = vec![UNBOUND; self.plan.width as usize];
        let _ = self.eval(env, node, active, &mut fresh, &mut |_, row| {
            rows.extend(slots.iter().map(|&s| row[s as usize]));
            CONTINUE
        });
        let key_column = key.and_then(|k| slots.iter().position(|&s| s == k));
        if let (Some(col), w) = (key_column, slots.len()) {
            let mut order: Vec<usize> = (0..rows.len() / w).collect();
            order.sort_by_key(|&i| rows[i * w + col]);
            rows = order.iter().flat_map(|&i| rows[i * w..(i + 1) * w].iter().copied()).collect();
        }
        let m = Rc::new(Materialized { slots, rows, key_column });
        env.materialized.insert(cache_key, m.clone());
        m
    }

    pub(super) fn bgp_buffers(env: &mut Env) -> BgpBuffers {
        env.bgp_buffers.pop().unwrap_or_default()
    }

    pub(super) fn return_bgp_buffers(env: &mut Env, buffers: BgpBuffers) {
        env.bgp_buffers.push(buffers);
    }

    pub(super) fn check_buffers(env: &mut Env) -> CheckBuffers {
        env.check_buffers.pop().unwrap_or_default()
    }

    pub(super) fn return_check_buffers(env: &mut Env, buffers: CheckBuffers) {
        env.check_buffers.push(buffers);
    }

    /// An empty id set over the dictionary's ids, from the query's pool.
    pub(super) fn id_set(&self, env: &mut Env) -> IdSet {
        let bound = self.store.dictionary().id_bound();
        match env.id_sets.pop() {
            Some(set) if set.covers(bound) => set,
            _ => IdSet::new(bound),
        }
    }

    pub(super) fn return_id_set(env: &mut Env, mut set: IdSet) {
        set.clear();
        env.id_sets.push(set);
    }

    pub(super) fn automaton_workspace(env: &mut Env) -> AutomatonWorkspace {
        env.automaton_workspaces.pop().unwrap_or_default()
    }

    pub(super) fn return_automaton_workspace(env: &mut Env, ws: AutomatonWorkspace) {
        env.automaton_workspaces.push(ws);
    }

    pub(super) fn automata(&self, env: &mut Env, path: PathId) -> Rc<[PathAutomaton; 2]> {
        let PathPlan::Automaton { automaton, .. } = &self.plan.paths[path as usize] else { unreachable!("automaton path") };
        env.automata.entry(path).or_insert_with(|| Rc::new([automaton.automaton(), automaton.reversed().automaton()])).clone()
    }
}

/// Binds `values` into `row` at `slots` where unbound; `false` on a
/// conflicting binding. Newly bound slots are pushed on `saved`.
pub fn merge(row: &mut [u32], slots: &[Slot], values: &[u32], saved: &mut Vec<u32>) -> bool {
    for (&slot, &v) in slots.iter().zip(values) {
        if v == UNBOUND {
            continue;
        }
        let current = &mut row[slot as usize];
        if *current == UNBOUND {
            *current = v;
            saved.push(slot);
        } else if *current != v {
            return false;
        }
    }
    true
}

/// Undoes [`merge`] back to `mark`.
pub fn unmerge(row: &mut [u32], saved: &mut Vec<u32>, mark: usize) {
    for &slot in &saved[mark..] {
        row[slot as usize] = UNBOUND;
    }
    saved.truncate(mark);
}

fn probe(env: &mut Env, m: &Materialized, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
    for i in m.candidates(row) {
        let mark = env.saved.len();
        if merge(row, &m.slots, m.row(i), &mut env.saved) {
            let flow = k(env, row);
            unmerge(row, &mut env.saved, mark);
            flow?;
        } else {
            unmerge(row, &mut env.saved, mark);
        }
    }
    CONTINUE
}

/// MINUS: some right row is compatible with `row` and shares a bound slot.
fn minus_hit(m: &Materialized, row: &[u32]) -> bool {
    m.candidates(row).any(|i| {
        let mut shared = false;
        for (&slot, &v) in m.slots.iter().zip(m.row(i)) {
            let current = row[slot as usize];
            if v == UNBOUND || current == UNBOUND {
                continue;
            }
            if current != v {
                return false;
            }
            shared = true;
        }
        shared
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_and_unmerge_restore_the_row() {
        let mut row = [UNBOUND, 5, UNBOUND];
        let mut saved = Vec::new();
        assert!(merge(&mut row, &[0, 1], &[7, 5], &mut saved));
        assert_eq!(row, [7, 5, UNBOUND]);
        unmerge(&mut row, &mut saved, 0);
        assert_eq!(row, [UNBOUND, 5, UNBOUND]);
        assert!(!merge(&mut row, &[1], &[6], &mut saved));
        unmerge(&mut row, &mut saved, 0);
        assert_eq!(row, [UNBOUND, 5, UNBOUND]);
    }

    #[test]
    fn keyed_candidates_narrow_the_scan() {
        let m = Materialized { slots: vec![0, 1], rows: vec![1, 10, 2, 20, 2, 21, 3, 30], key_column: Some(0) };
        assert_eq!(m.candidates(&[2, UNBOUND]), 1..3);
        assert_eq!(m.candidates(&[UNBOUND, UNBOUND]), 0..4);
        assert_eq!(m.candidates(&[9, UNBOUND]).len(), 0);
    }

    #[test]
    fn minus_needs_a_shared_binding() {
        let m = Materialized { slots: vec![1], rows: vec![5], key_column: None };
        assert!(minus_hit(&m, &[UNBOUND, 5]));
        assert!(!minus_hit(&m, &[UNBOUND, 6]));
        assert!(!minus_hit(&m, &[3, UNBOUND]), "no shared variable");
    }

    #[test]
    fn now_is_an_xsd_date_time() {
        assert_eq!(format_now(std::time::Duration::from_millis(0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_now(std::time::Duration::from_secs(951_782_400)), "2000-02-29T00:00:00.000Z");
    }
}
