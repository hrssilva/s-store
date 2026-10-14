//! Compiles s-parser's SPARQL algebra into a [`Plan`].
//!
//! - **Variables** become row slots. A subquery (`Project`) opens a scope:
//!   its projected variables share the enclosing scope's slots, its other
//!   variables get slots of their own, so an inner `?x` never meets an
//!   outer `?x` it doesn't project.
//! - **Constants** become term ids: dictionary ids, or query-local ids for
//!   terms the data doesn't contain (which then match nothing in a pattern,
//!   but still exist for `BIND`, `VALUES`, zero-length paths...).
//! - **Paths** follow §18.2.2.4: links, inverses, sequences and
//!   alternatives become patterns, joins and unions; `*`, `+`, `?` and
//!   negated property sets become automata (see [`super::path`]).
//! - **Join modes** come from a substitution-safety analysis
//!   ([`Analysis`]): the executor evaluates a join's right side once per
//!   left solution with that solution's bindings substituted (index nested
//!   loop, the efficient case for s-graph's bound lookups) only where
//!   substitution cannot change the result: SPARQL evaluates groups bottom
//!   up, and a FILTER, OPTIONAL or subquery inside the right side can see the
//!   difference. Elsewhere the right side is materialized once and probed.
//!
//! Also corrected here: s-parser nests `Slice` under `Project`/`Distinct`;
//! SPARQL applies LIMIT/OFFSET after DISTINCT.

use super::locals::LocalTerms;
use super::path::{NO_PREDICATE, NfaBuilder};
use super::plan::*;
use super::value::Arena;
use crate::error::{Result, StoreError};
use crate::graph::GraphName;
use crate::store::Store;
use crate::term::{KeyBuf, XSD};
use s_graph::BlockStorage;
use s_graph::traversal::Direction;
use s_parser::sparql::algebra::{Algebra, Projection, QueryData, QueryForm, TriplePattern};
use s_parser::sparql::expr::Expr as AExpr;
use s_parser::sparql::path::PathExpr;
use s_parser::sparql::term::{BlankNode, Iri, LiteralSuffix, NumKind, Term, Var};
use s_parser::{Query, ResolvedIri, ResolvedSuffix};
use std::collections::HashMap;

#[derive(Clone, PartialEq, Eq, Hash)]
enum VarKey {
    Named(Vec<u8>),
    Synthetic(u32),
    Blank(Vec<u8>),
    Anonymous(u32),
    Fresh(u32),
}

struct Scope {
    vars: HashMap<VarKey, Slot>,
    first_slot: Slot,
}

pub struct Compiler<'c, 'q, S: BlockStorage> {
    store: &'c Store<S>,
    query: &'c Query<'q>,
    q: &'c QueryData,
    base: Option<String>,
    locals: &'c mut LocalTerms,
    arena: &'c mut Arena,
    key: KeyBuf,
    plan: Plan,
    scopes: Vec<Scope>,
    fresh: u32,
    max_slot: Slot,
    /// Named variables of the outermost scope in order of appearance
    /// (`SELECT *`).
    appearance: Vec<(String, Slot)>,
    blanks: HashMap<VarKey, u32>,
}

fn unsupported(what: &str) -> StoreError {
    StoreError::Query(format!("unsupported: {what}"))
}

/// Compiles `query` against `store`. Constants missing from the dictionary
/// are interned into `locals`; expression constants are decoded into the
/// frozen prefix of `arena`.
pub fn compile<S: BlockStorage>(store: &Store<S>, query: &Query<'_>, locals: &mut LocalTerms, arena: &mut Arena) -> Result<Plan> {
    let q = query.data();
    let base = q.prologue.base.map(|sp| String::from_utf8_lossy(sp.text(query.bytes())).into_owned());
    let plan = Plan {
        root: 0,
        form: Form::Ask,
        dataset: Dataset { default: vec![GraphName::Default], named: None },
        width: 0,
        nodes: Vec::new(),
        exprs: Vec::new(),
        triples: Vec::new(),
        paths: Vec::new(),
        slots: Vec::new(),
        values: Vec::new(),
        expr_lists: Vec::new(),
        group_keys: Vec::new(),
        aggregates: Vec::new(),
        order_keys: Vec::new(),
        scopes: Vec::new(),
    };
    let mut c = Compiler { store, query, q, base, locals, arena, key: KeyBuf::new(), plan, scopes: vec![Scope { vars: HashMap::new(), first_slot: 0 }], fresh: 0, max_slot: 0, appearance: Vec::new(), blanks: HashMap::new() };
    c.dataset()?;
    let root = match q.root {
        Some(r) => c.node(r)?,
        None => c.push(Node::Unit),
    };
    c.plan.root = root;
    c.plan.form = c.form()?;
    c.plan.width = c.next_slot();
    let mut plan = c.plan;
    Analysis::run(&mut plan);
    arena.freeze();
    Ok(plan)
}

impl<S: BlockStorage> Compiler<'_, '_, S> {
    fn push(&mut self, node: Node) -> NodeId {
        self.plan.nodes.push(node);
        (self.plan.nodes.len() - 1) as NodeId
    }

    fn push_expr(&mut self, e: Expr) -> ExprId {
        self.plan.exprs.push(e);
        (self.plan.exprs.len() - 1) as ExprId
    }

    fn next_slot(&self) -> Slot {
        self.max_slot
    }

    fn var_key(&self, v: Var) -> VarKey {
        match v {
            Var::Named(sp) => VarKey::Named(sp.text(self.query.bytes()).to_vec()),
            Var::Synthetic(id) => VarKey::Synthetic(id),
        }
    }

    fn new_slot(&mut self) -> Slot {
        let slot = self.max_slot;
        self.max_slot += 1;
        slot
    }

    fn slot_for(&mut self, key: VarKey) -> Slot {
        if let Some(&slot) = self.scopes.last().expect("a scope").vars.get(&key) {
            return slot;
        }
        let slot = self.new_slot();
        if let (1, VarKey::Named(name)) = (self.scopes.len(), &key) {
            self.appearance.push((String::from_utf8_lossy(name).into_owned(), slot));
        }
        self.scopes.last_mut().expect("a scope").vars.insert(key, slot);
        slot
    }

    fn var_slot(&mut self, v: Var) -> Slot {
        let key = self.var_key(v);
        self.slot_for(key)
    }

    fn fresh_slot(&mut self) -> Slot {
        self.fresh += 1;
        self.slot_for(VarKey::Fresh(self.fresh))
    }

    // ---------- terms ----------

    fn iri_key(&mut self, iri: Iri) -> Result<&str> {
        let resolved = self.query.resolve(iri).ok_or_else(|| StoreError::Query("undeclared prefix".into()))?;
        Ok(self.key.iri(resolved, self.base.as_deref())?)
    }

    /// The canonical key of a non-variable term.
    fn const_key(&mut self, t: Term) -> Result<&str> {
        let src = self.query.bytes();
        let xsd = |local: &str| -> ResolvedSuffix<'static> {
            let iri: &'static str = match local {
                "integer" => crate::term::XSD_INTEGER,
                "decimal" => crate::term::XSD_DECIMAL,
                "double" => crate::term::XSD_DOUBLE,
                _ => crate::term::XSD_BOOLEAN,
            };
            ResolvedSuffix::Datatype(ResolvedIri::Direct(iri.as_bytes()))
        };
        match t {
            Term::Iri(iri) => self.iri_key(iri),
            Term::Literal(lit) => {
                let lexical = lit.lexical.text(src);
                let suffix = match lit.suffix {
                    LiteralSuffix::None => ResolvedSuffix::None,
                    LiteralSuffix::Lang(sp) => ResolvedSuffix::Lang(sp.text(src)),
                    LiteralSuffix::Datatype(iri) => {
                        let resolved = self.query.resolve(iri).ok_or_else(|| StoreError::Query("undeclared prefix".into()))?;
                        // Resolve relative datatype IRIs the same way as others.
                        let dt = self.key.iri(resolved, self.base.as_deref())?.to_owned();
                        return Ok(self.key.literal(lexical, ResolvedSuffix::Datatype(ResolvedIri::Direct(&dt.as_bytes()[1..dt.len() - 1])), None)?);
                    }
                };
                Ok(self.key.literal(lexical, suffix, None)?)
            }
            Term::Numeric(kind, sp) => {
                let local = match kind {
                    NumKind::Integer => "integer",
                    NumKind::Decimal => "decimal",
                    NumKind::Double => "double",
                };
                Ok(self.key.literal(sp.text(src), xsd(local), None)?)
            }
            Term::Boolean(b) => Ok(self.key.literal(if b { b"true" } else { b"false" }, xsd("boolean"), None)?),
            Term::Var(_) | Term::Blank(_) => unreachable!("not a constant"),
        }
    }

    fn const_id(&mut self, t: Term) -> Result<u32> {
        let key = self.const_key(t)?.to_owned();
        Ok(self.locals.id_for(self.store.dictionary(), &key))
    }

    fn pterm(&mut self, t: Term) -> Result<PTerm> {
        Ok(match t {
            Term::Var(v) => PTerm::Var(self.var_slot(v)),
            Term::Blank(BlankNode::Labeled(sp)) => PTerm::Var(self.slot_for(VarKey::Blank(sp.text(self.query.bytes()).to_vec()))),
            Term::Blank(BlankNode::Anonymous(n)) => PTerm::Var(self.slot_for(VarKey::Anonymous(n))),
            other => PTerm::Const(self.const_id(other)?),
        })
    }

    fn graph_name(&mut self, iri: Iri) -> Result<Option<u32>> {
        let key = self.iri_key(iri)?.to_owned();
        Ok(self.store.dictionary().lookup(&key).map(|id| id.0))
    }

    fn dataset(&mut self) -> Result<()> {
        let d = &self.q.dataset;
        if d.default.is_empty() && d.named.is_empty() {
            return Ok(());
        }
        let (default, named) = (d.default.clone(), d.named.clone());
        let mut graphs = Vec::new();
        for iri in default {
            if let Some(g) = self.graph_name(iri)? {
                graphs.push(GraphName::Named(s_dict::TermId(g)));
            }
        }
        graphs.sort_unstable();
        graphs.dedup();
        let mut names = Vec::new();
        for iri in named {
            if let Some(g) = self.graph_name(iri)? {
                names.push(g);
            }
        }
        names.sort_unstable();
        names.dedup();
        self.plan.dataset = Dataset { default: graphs, named: Some(names) };
        Ok(())
    }

    // ---------- patterns ----------

    fn node(&mut self, id: s_parser::sparql::ids::NodeId) -> Result<NodeId> {
        Ok(match self.q.node(id) {
            Algebra::Unit => self.push(Node::Unit),
            Algebra::Bgp(range) => {
                let triples: Vec<TriplePattern> = self.q.triples(range).to_vec();
                self.bgp(&triples)?
            }
            Algebra::Path { subject, path, object } => {
                let (s, o) = (self.pterm(subject)?, self.pterm(object)?);
                self.path(s, path, o)?
            }
            Algebra::Join(a, b) => {
                let (a, b) = (self.node(a)?, self.node(b)?);
                self.join(a, b)
            }
            Algebra::LeftJoin(a, b, cond) => {
                let (a, b) = (self.node(a)?, self.node(b)?);
                let cond = cond.map(|e| self.expr(e)).transpose()?;
                self.push(Node::LeftJoin(a, b, cond, JoinMode::Bind))
            }
            Algebra::Union(a, b) => {
                let (a, b) = (self.node(a)?, self.node(b)?);
                self.push(Node::Union(a, b))
            }
            Algebra::Minus(a, b) => {
                let (a, b) = (self.node(a)?, self.node(b)?);
                self.push(Node::Minus(a, b))
            }
            Algebra::Graph(g, a) => {
                let g = match g {
                    Term::Iri(iri) => PTerm::Const(match self.graph_name(iri)? {
                        Some(g) => g,
                        None => {
                            let key = self.iri_key(iri)?.to_owned();
                            self.locals.intern(&key)
                        }
                    }),
                    other => self.pterm(other)?,
                };
                let a = self.node(a)?;
                self.push(Node::Graph(g, a))
            }
            Algebra::Filter(e, a) => {
                let a = self.node(a)?;
                let e = self.expr(e)?;
                self.push(Node::Filter(e, a))
            }
            Algebra::Extend(a, v, e) => {
                let a = self.node(a)?;
                let e = self.expr(e)?;
                let slot = self.var_slot(v);
                self.push(Node::Extend(a, slot, e))
            }
            Algebra::Group { input, keys, aggregates } => {
                let input = self.node(input)?;
                let key_start = self.plan.group_keys.len() as u32;
                for &k in self.q.expr_ids(keys) {
                    let expr = self.expr(k)?;
                    let slot = match self.plan.expr(expr) {
                        Expr::Var(s) => Some(s),
                        _ => None,
                    };
                    self.plan.group_keys.push(GroupKey { expr, slot });
                }
                let keys = Range { start: key_start, len: self.plan.group_keys.len() as u32 - key_start };
                let agg_start = self.plan.aggregates.len() as u32;
                for ga in self.q.group_aggregates(aggregates).to_vec() {
                    let expr = ga.agg.expr.map(|e| self.expr(e)).transpose()?;
                    let separator = match ga.agg.separator {
                        Some(sp) => {
                            let key = self.key.literal(sp.text(self.query.bytes()), ResolvedSuffix::None, None)?.to_owned();
                            Some(self.const_expr(&key))
                        }
                        None => None,
                    };
                    let out = self.var_slot(ga.var);
                    self.plan.aggregates.push(Aggregate { kind: ga.agg.kind, distinct: ga.agg.distinct, expr, separator, out });
                }
                let aggregates = Range { start: agg_start, len: self.plan.aggregates.len() as u32 - agg_start };
                self.push(Node::Group { input, keys, aggregates })
            }
            Algebra::OrderBy(input, range) => {
                let input = self.node(input)?;
                let start = self.plan.order_keys.len() as u32;
                for k in self.q.order_keys(range).to_vec() {
                    let expr = self.expr(k.expr)?;
                    self.plan.order_keys.push(OrderKey { expr, descending: k.descending });
                }
                let keys = Range { start, len: self.plan.order_keys.len() as u32 - start };
                self.push(Node::OrderBy { input, keys, scope: Range::default() })
            }
            Algebra::Project(input, vars) => self.project_over(input, vars)?,
            Algebra::Distinct(input) | Algebra::Reduced(input) => {
                let reduced = matches!(self.q.node(id), Algebra::Reduced(_));
                // Move a Slice nested under the projection above DISTINCT.
                if let Algebra::Project(p_in, vars) = self.q.node(input)
                    && let Algebra::Slice { input: s_in, offset, limit } = self.q.node(p_in)
                {
                    let projected = self.project_over(s_in, vars)?;
                    let distinct = if reduced { projected } else { self.push(Node::Distinct { input: projected, scope: Range::default() }) };
                    return Ok(self.push(Node::Slice { input: distinct, offset: offset.unwrap_or(0), limit }));
                }
                let input = self.node(input)?;
                if reduced { input } else { self.push(Node::Distinct { input, scope: Range::default() }) }
            }
            Algebra::Slice { input, offset, limit } => {
                let input = self.node(input)?;
                self.push(Node::Slice { input, offset: offset.unwrap_or(0), limit })
            }
            Algebra::Table { vars, rows } => {
                let slot_start = self.plan.slots.len() as u32;
                let vars = self.q.vars(vars).to_vec();
                let slots: Vec<Slot> = vars.iter().map(|&v| self.var_slot(v)).collect();
                self.plan.slots.extend_from_slice(&slots);
                let value_start = self.plan.values.len() as u32;
                for v in self.q.values(rows).to_vec() {
                    let id = match v {
                        None => UNBOUND,
                        Some(t) => self.const_id(t)?,
                    };
                    self.plan.values.push(id);
                }
                let slots = Range { start: slot_start, len: slots.len() as u32 };
                let rows = Range { start: value_start, len: self.plan.values.len() as u32 - value_start };
                self.push(Node::Table { slots, rows })
            }
        })
    }

    /// A subquery scope over `input` exposing `vars`.
    fn project_over(&mut self, input: s_parser::sparql::ids::NodeId, vars: s_parser::sparql::pool::Range) -> Result<NodeId> {
        let vars = self.q.vars(vars).to_vec();
        let mut inner = HashMap::new();
        for &v in &vars {
            let key = self.var_key(v);
            let slot = self.slot_for(key.clone());
            inner.insert(key, slot);
        }
        let first_slot = self.max_slot;
        self.scopes.push(Scope { vars: inner, first_slot });
        let input = self.node(input);
        let scope = self.scopes.pop().expect("pushed above");
        let input = input?;
        let hidden_start = self.plan.slots.len() as u32;
        self.plan.slots.extend(scope.first_slot..self.max_slot);
        let hidden = Range { start: hidden_start, len: self.plan.slots.len() as u32 - hidden_start };
        Ok(self.push(Node::Project { input, hidden }))
    }

    fn join(&mut self, a: NodeId, b: NodeId) -> NodeId {
        match (self.plan.node(a), self.plan.node(b)) {
            (Node::Unit, _) => b,
            (_, Node::Unit) => a,
            (Node::Bgp(x), Node::Bgp(y)) => {
                let start = self.plan.triples.len() as u32;
                self.plan.triples.extend_from_within(x.indices());
                self.plan.triples.extend_from_within(y.indices());
                self.push(Node::Bgp(Range { start, len: x.len + y.len }))
            }
            _ => self.push(Node::Join(a, b, JoinMode::Bind)),
        }
    }

    fn bgp(&mut self, triples: &[TriplePattern]) -> Result<NodeId> {
        let mut compiled = Vec::with_capacity(triples.len());
        for t in triples {
            compiled.push(PTriple { s: self.pterm(t.subject)?, p: self.pterm(t.predicate)?, o: self.pterm(t.object)? });
        }
        let start = self.plan.triples.len() as u32;
        self.plan.triples.extend(compiled);
        Ok(self.push(Node::Bgp(Range { start, len: triples.len() as u32 })))
    }

    fn single(&mut self, s: PTerm, p: PTerm, o: PTerm) -> NodeId {
        let start = self.plan.triples.len() as u32;
        self.plan.triples.push(PTriple { s, p, o });
        self.push(Node::Bgp(Range { start, len: 1 }))
    }

    // ---------- paths ----------

    fn path(&mut self, s: PTerm, path: s_parser::sparql::ids::PathId, o: PTerm) -> Result<NodeId> {
        Ok(match self.q.path(path) {
            PathExpr::Link(iri) => {
                let p = self.const_id(Term::Iri(iri))?;
                self.single(s, PTerm::Const(p), o)
            }
            PathExpr::Inv(inner) => self.path(o, inner, s)?,
            PathExpr::Seq(a, b) => {
                let mid = PTerm::Var(self.fresh_slot());
                let a = self.path(s, a, mid)?;
                let b = self.path(mid, b, o)?;
                self.join(a, b)
            }
            PathExpr::Alt(a, b) => {
                let (a, b) = (self.path(s, a, o)?, self.path(s, b, o)?);
                self.push(Node::Union(a, b))
            }
            PathExpr::NegatedPropertySet(items) => {
                let items = self.q.neg_path_items(items).to_vec();
                let (mut forward, mut backward) = (Vec::new(), Vec::new());
                for item in &items {
                    let dense = self.dense_predicate(item.iri)?;
                    if item.inverse { backward.push(dense) } else { forward.push(dense) }
                }
                let plan = PathPlan::Negated { forward_any: items.iter().any(|i| !i.inverse), forward, inverse_any: items.iter().any(|i| i.inverse), backward };
                self.plan.paths.push(plan);
                let id = (self.plan.paths.len() - 1) as PathId;
                self.push(Node::Path { s, path: id, o })
            }
            PathExpr::ZeroOrMore(_) | PathExpr::OneOrMore(_) | PathExpr::ZeroOrOne(_) => {
                let mut b = NfaBuilder::default();
                let (start, end) = self.nfa(&mut b, path, false)?;
                let automaton = b.finish(start, end);
                let zero_length = automaton.accepting.contains(&automaton.start);
                self.plan.paths.push(PathPlan::Automaton { automaton, zero_length });
                let id = (self.plan.paths.len() - 1) as PathId;
                self.push(Node::Path { s, path: id, o })
            }
        })
    }

    fn dense_predicate(&mut self, iri: Iri) -> Result<u32> {
        let key = self.iri_key(iri)?.to_owned();
        Ok(self.store.dictionary().lookup(&key).and_then(|id| self.store.index().predicates().get(id)).unwrap_or(NO_PREDICATE))
    }

    /// Builds `path`'s epsilon-NFA fragment; `inverse` walks it backwards.
    fn nfa(&mut self, b: &mut NfaBuilder, path: s_parser::sparql::ids::PathId, inverse: bool) -> Result<(u32, u32)> {
        let forward = if inverse { Direction::Incoming } else { Direction::Outgoing };
        Ok(match self.q.path(path) {
            PathExpr::Link(iri) => {
                let p = self.dense_predicate(iri)?;
                let (s, e) = (b.state(), b.state());
                b.step(s, e, forward, p);
                (s, e)
            }
            PathExpr::Inv(inner) => self.nfa(b, inner, !inverse)?,
            PathExpr::Seq(x, y) => {
                let (x, y) = if inverse { (y, x) } else { (x, y) };
                let (s1, e1) = self.nfa(b, x, inverse)?;
                let (s2, e2) = self.nfa(b, y, inverse)?;
                b.epsilon(e1, s2);
                (s1, e2)
            }
            PathExpr::Alt(x, y) => {
                let (s, e) = (b.state(), b.state());
                for part in [x, y] {
                    let (ps, pe) = self.nfa(b, part, inverse)?;
                    b.epsilon(s, ps);
                    b.epsilon(pe, e);
                }
                (s, e)
            }
            PathExpr::ZeroOrMore(x) | PathExpr::OneOrMore(x) | PathExpr::ZeroOrOne(x) => {
                let (s, e) = (b.state(), b.state());
                let (ps, pe) = self.nfa(b, x, inverse)?;
                b.epsilon(s, ps);
                b.epsilon(pe, e);
                let shape = self.q.path(path);
                if matches!(shape, PathExpr::ZeroOrMore(_) | PathExpr::ZeroOrOne(_)) {
                    b.epsilon(s, e);
                }
                if matches!(shape, PathExpr::ZeroOrMore(_) | PathExpr::OneOrMore(_)) {
                    b.epsilon(pe, ps);
                }
                (s, e)
            }
            PathExpr::NegatedPropertySet(items) => {
                let items = self.q.neg_path_items(items).to_vec();
                let (s, e) = (b.state(), b.state());
                let mut excluded = [Vec::new(), Vec::new()];
                for item in &items {
                    let dense = self.dense_predicate(item.iri)?;
                    excluded[usize::from(item.inverse)].push(dense);
                }
                let all: Vec<u32> = self.store.index().predicates().iter().map(|(d, _)| d).collect();
                for (inverse_item, excluded) in excluded.iter().enumerate() {
                    let present = items.iter().any(|i| i.inverse == (inverse_item == 1));
                    if !present {
                        continue;
                    }
                    let backwards = (inverse_item == 1) != inverse;
                    let direction = if backwards { Direction::Incoming } else { Direction::Outgoing };
                    for &p in all.iter().filter(|p| !excluded.contains(p)) {
                        b.step(s, e, direction, p);
                    }
                }
                (s, e)
            }
        })
    }

    // ---------- expressions ----------

    fn const_expr(&mut self, key: &str) -> ExprId {
        let id = self.locals.id_for(self.store.dictionary(), key);
        let value = self.arena.decode(key).expect("compiled keys decode");
        self.push_expr(Expr::Const(id, value))
    }

    fn expr_list(&mut self, range: s_parser::sparql::pool::Range) -> Result<Range> {
        let ids = self.q.expr_ids(range).to_vec();
        let mut compiled = Vec::with_capacity(ids.len());
        for e in ids {
            compiled.push(self.expr(e)?);
        }
        let start = self.plan.expr_lists.len() as u32;
        self.plan.expr_lists.extend(compiled);
        Ok(Range { start, len: self.plan.expr_lists.len() as u32 - start })
    }

    fn expr(&mut self, id: s_parser::sparql::ids::ExprId) -> Result<ExprId> {
        let e = match self.q.expr(id) {
            AExpr::Term(Term::Var(v)) => Expr::Var(self.var_slot(v)),
            AExpr::Term(Term::Blank(_)) => return Err(StoreError::Query("blank node in expression".into())),
            AExpr::Term(t) => {
                let key = self.const_key(t)?.to_owned();
                return Ok(self.const_expr(&key));
            }
            AExpr::Or(a, b) => Expr::Or(self.expr(a)?, self.expr(b)?),
            AExpr::And(a, b) => Expr::And(self.expr(a)?, self.expr(b)?),
            AExpr::Not(a) => Expr::Not(self.expr(a)?),
            AExpr::Eq(a, b) => Expr::Eq(self.expr(a)?, self.expr(b)?),
            AExpr::Ne(a, b) => Expr::Ne(self.expr(a)?, self.expr(b)?),
            AExpr::Lt(a, b) => Expr::Lt(self.expr(a)?, self.expr(b)?),
            AExpr::Le(a, b) => Expr::Le(self.expr(a)?, self.expr(b)?),
            AExpr::Gt(a, b) => Expr::Gt(self.expr(a)?, self.expr(b)?),
            AExpr::Ge(a, b) => Expr::Ge(self.expr(a)?, self.expr(b)?),
            AExpr::In(a, list) => Expr::In(self.expr(a)?, self.expr_list(list)?),
            AExpr::NotIn(a, list) => Expr::NotIn(self.expr(a)?, self.expr_list(list)?),
            AExpr::Add(a, b) => Expr::Add(self.expr(a)?, self.expr(b)?),
            AExpr::Sub(a, b) => Expr::Sub(self.expr(a)?, self.expr(b)?),
            AExpr::Mul(a, b) => Expr::Mul(self.expr(a)?, self.expr(b)?),
            AExpr::Div(a, b) => Expr::Div(self.expr(a)?, self.expr(b)?),
            AExpr::Plus(a) => Expr::Plus(self.expr(a)?),
            AExpr::Neg(a) => Expr::Neg(self.expr(a)?),
            AExpr::Call(builtin, args) => Expr::Call(builtin, self.expr_list(args)?),
            AExpr::FunctionCall(iri, args) => {
                let key = self.iri_key(iri)?.to_owned();
                let cast = match key[1..key.len() - 1].strip_prefix(XSD) {
                    Some("string") => Cast::String,
                    Some("boolean") => Cast::Boolean,
                    Some("integer") => Cast::Integer,
                    Some("decimal") => Cast::Decimal,
                    Some("float") => Cast::Float,
                    Some("double") => Cast::Double,
                    Some("dateTime") => Cast::DateTime,
                    _ => return Err(unsupported(&format!("function {key}"))),
                };
                let args = self.q.expr_ids(args).to_vec();
                let [arg] = args[..] else { return Err(StoreError::Query("a cast takes one argument".into())) };
                Expr::Cast(cast, self.expr(arg)?)
            }
            AExpr::Aggregate(_) => return Err(StoreError::Query("aggregate outside a group".into())),
            AExpr::Exists(n) => Expr::Exists(self.node(n)?),
            AExpr::NotExists(n) => Expr::NotExists(self.node(n)?),
        };
        Ok(self.push_expr(e))
    }

    // ---------- query forms ----------

    fn form(&mut self) -> Result<Form> {
        Ok(match self.q.form.ok_or_else(|| StoreError::Query("no query form".into()))? {
            QueryForm::Select { projection, .. } => Form::Select(match projection {
                Projection::All => self.appearance.clone(),
                Projection::Vars(range) => {
                    let vars = self.q.vars(range).to_vec();
                    vars.into_iter()
                        .map(|v| {
                            let name = match v {
                                Var::Named(sp) => String::from_utf8_lossy(sp.text(self.query.bytes())).into_owned(),
                                Var::Synthetic(n) => format!("_{n}"),
                            };
                            (name, self.var_slot(v))
                        })
                        .collect()
                }
            }),
            QueryForm::Ask => Form::Ask,
            QueryForm::Construct { template } => {
                let triples = self.q.triples(template).to_vec();
                let mut out = Vec::with_capacity(triples.len());
                for t in triples {
                    out.push([self.tterm(t.subject)?, self.tterm(t.predicate)?, self.tterm(t.object)?]);
                }
                Form::Construct(out)
            }
            QueryForm::Describe { targets, .. } => {
                let terms = self.q.terms(targets).to_vec();
                let mut out = Vec::with_capacity(terms.len());
                for t in terms {
                    out.push(self.pterm(t)?);
                }
                Form::Describe(out)
            }
        })
    }

    fn tterm(&mut self, t: Term) -> Result<TTerm> {
        let src = self.query.bytes();
        let blank = |key: VarKey, blanks: &mut HashMap<VarKey, u32>| {
            let n = blanks.len() as u32;
            *blanks.entry(key).or_insert(n)
        };
        Ok(match t {
            Term::Var(v) => TTerm::Var(self.var_slot(v)),
            Term::Blank(BlankNode::Labeled(sp)) => TTerm::Blank(blank(VarKey::Blank(sp.text(src).to_vec()), &mut self.blanks)),
            Term::Blank(BlankNode::Anonymous(n)) => TTerm::Blank(blank(VarKey::Anonymous(n), &mut self.blanks)),
            other => TTerm::Const(self.const_id(other)?),
        })
    }
}

/// Variable sets as bitsets over slots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct VarSet(Vec<u64>);

impl VarSet {
    fn insert(&mut self, s: Slot) {
        let (w, b) = (s as usize / 64, s % 64);
        if self.0.len() <= w {
            self.0.resize(w + 1, 0);
        }
        self.0[w] |= 1 << b;
    }

    fn union(&self, other: &VarSet) -> VarSet {
        let n = self.0.len().max(other.0.len());
        VarSet((0..n).map(|i| self.0.get(i).copied().unwrap_or(0) | other.0.get(i).copied().unwrap_or(0)).collect())
    }

    fn intersect(&self, other: &VarSet) -> VarSet {
        VarSet(self.0.iter().zip(&other.0).map(|(a, b)| a & b).collect())
    }

    fn minus(&self, other: &VarSet) -> VarSet {
        VarSet(self.0.iter().enumerate().map(|(i, a)| a & !other.0.get(i).copied().unwrap_or(0)).collect())
    }

    fn is_subset(&self, other: &VarSet) -> bool {
        self.0.iter().enumerate().all(|(i, a)| a & !other.0.get(i).copied().unwrap_or(0) == 0)
    }

    fn is_empty(&self) -> bool {
        self.0.iter().all(|&w| w == 0)
    }

    fn contains(&self, s: Slot) -> bool {
        self.0.get(s as usize / 64).is_some_and(|w| w & (1 << (s % 64)) != 0)
    }

    fn slots(&self) -> impl Iterator<Item = Slot> + '_ {
        self.0.iter().enumerate().flat_map(|(i, &w)| (0..64).filter(move |b| w & (1 << b) != 0).map(move |b| (i * 64 + b) as Slot))
    }
}

/// Which slots each node may bind (`maybe`), always binds (`certain`), and
/// references anywhere inside (`refs`: also expression variables).
/// Children always have smaller ids than their parents, so one forward pass
/// computes all three.
struct Analysis {
    maybe: Vec<VarSet>,
    certain: Vec<VarSet>,
    refs: Vec<VarSet>,
}

impl Analysis {
    fn run(plan: &mut Plan) {
        let n = plan.nodes.len();
        let mut a = Analysis { maybe: vec![VarSet::default(); n], certain: vec![VarSet::default(); n], refs: vec![VarSet::default(); n] };
        for id in 0..n {
            a.compute(plan, id as NodeId);
        }
        for maybe in &a.maybe {
            let start = plan.slots.len() as u32;
            plan.slots.extend(maybe.slots());
            plan.scopes.push(Range { start, len: plan.slots.len() as u32 - start });
        }
        // Blocking operators keep (and compare) their input's slots.
        for id in 0..n {
            if let Node::OrderBy { input, scope, .. } | Node::Distinct { input, scope } = &mut plan.nodes[id] {
                *scope = plan.scopes[*input as usize];
            }
        }
        a.annotate(plan, plan.root, &VarSet::default());
    }

    fn expr_vars(&self, plan: &Plan, e: ExprId, out: &mut VarSet) {
        match plan.expr(e) {
            Expr::Var(s) => out.insert(s),
            Expr::Const(..) => {}
            Expr::Or(a, b) | Expr::And(a, b) | Expr::Eq(a, b) | Expr::Ne(a, b) | Expr::Lt(a, b) | Expr::Le(a, b) | Expr::Gt(a, b) | Expr::Ge(a, b) | Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
                self.expr_vars(plan, a, out);
                self.expr_vars(plan, b, out);
            }
            Expr::Not(a) | Expr::Plus(a) | Expr::Neg(a) | Expr::Cast(_, a) => self.expr_vars(plan, a, out),
            Expr::In(a, list) | Expr::NotIn(a, list) => {
                self.expr_vars(plan, a, out);
                for &x in plan.expr_list(list) {
                    self.expr_vars(plan, x, out);
                }
            }
            Expr::Call(_, args) => {
                for &x in plan.expr_list(args) {
                    self.expr_vars(plan, x, out);
                }
            }
            Expr::Exists(n) | Expr::NotExists(n) => *out = out.union(&self.refs[n as usize]),
        }
    }

    fn vars_of_expr(&self, plan: &Plan, e: ExprId) -> VarSet {
        let mut v = VarSet::default();
        self.expr_vars(plan, e, &mut v);
        v
    }

    fn compute(&mut self, plan: &Plan, id: NodeId) {
        let i = id as usize;
        let term_vars = |terms: &[PTerm], set: &mut VarSet| {
            for t in terms {
                if let PTerm::Var(s) = t {
                    set.insert(*s);
                }
            }
        };
        let (maybe, certain, refs) = match plan.node(id) {
            Node::Unit | Node::Empty => Default::default(),
            Node::Bgp(r) => {
                let mut v = VarSet::default();
                for t in plan.triples(r) {
                    term_vars(&[t.s, t.p, t.o], &mut v);
                }
                (v.clone(), v.clone(), v)
            }
            Node::Path { s, o, .. } => {
                let mut v = VarSet::default();
                term_vars(&[s, o], &mut v);
                (v.clone(), v.clone(), v)
            }
            Node::Join(a, b, _) => {
                let (a, b) = (a as usize, b as usize);
                (self.maybe[a].union(&self.maybe[b]), self.certain[a].union(&self.certain[b]), self.refs[a].union(&self.refs[b]))
            }
            Node::LeftJoin(a, b, cond, _) => {
                let (a, b) = (a as usize, b as usize);
                let mut refs = self.refs[a].union(&self.refs[b]);
                if let Some(c) = cond {
                    refs = refs.union(&self.vars_of_expr(plan, c));
                }
                (self.maybe[a].union(&self.maybe[b]), self.certain[a].clone(), refs)
            }
            Node::Union(a, b) => {
                let (a, b) = (a as usize, b as usize);
                (self.maybe[a].union(&self.maybe[b]), self.certain[a].intersect(&self.certain[b]), self.refs[a].union(&self.refs[b]))
            }
            Node::Minus(a, b) => (self.maybe[a as usize].clone(), self.certain[a as usize].clone(), self.refs[a as usize].union(&self.refs[b as usize])),
            Node::Graph(g, a) => {
                let mut extra = VarSet::default();
                term_vars(&[g], &mut extra);
                let a = a as usize;
                (self.maybe[a].union(&extra), self.certain[a].union(&extra), self.refs[a].union(&extra))
            }
            Node::Filter(e, a) => {
                let a = a as usize;
                (self.maybe[a].clone(), self.certain[a].clone(), self.refs[a].union(&self.vars_of_expr(plan, e)))
            }
            Node::Check { input, .. } => {
                let a = input as usize;
                (self.maybe[a].clone(), self.certain[a].clone(), self.refs[a].clone())
            }
            Node::SetJoin { left, joined, kind, .. } => {
                let mut v = VarSet::default();
                for t in plan.triples(joined) {
                    term_vars(&[t.s, t.p, t.o], &mut v);
                }
                let a = left as usize;
                let maybe = if kind == SetJoinKind::LeftOuter { v.clone() } else { self.maybe[a].clone() };
                (maybe, self.certain[a].clone(), v)
            }
            Node::Extend(a, v, e) => {
                let a = a as usize;
                let mut maybe = self.maybe[a].clone();
                maybe.insert(v);
                let refs = self.refs[a].union(&self.vars_of_expr(plan, e)).union(&maybe);
                (maybe, self.certain[a].clone(), refs)
            }
            Node::Group { input, keys, aggregates } => {
                let mut v = VarSet::default();
                for k in &plan.group_keys[keys.indices()] {
                    if let Some(s) = k.slot {
                        v.insert(s);
                    }
                }
                for agg in &plan.aggregates[aggregates.indices()] {
                    v.insert(agg.out);
                }
                (v.clone(), VarSet::default(), self.refs[input as usize].union(&v))
            }
            Node::OrderBy { input, .. } | Node::Distinct { input, .. } | Node::Slice { input, .. } => {
                let a = input as usize;
                (self.maybe[a].clone(), self.certain[a].clone(), self.refs[a].clone())
            }
            Node::Project { input, hidden } => {
                let mut h = VarSet::default();
                plan.slots(hidden).iter().for_each(|&s| h.insert(s));
                let a = input as usize;
                (self.maybe[a].minus(&h), self.certain[a].minus(&h), self.refs[a].minus(&h))
            }
            Node::Table { slots, rows } => {
                let slots = plan.slots(slots);
                let values = plan.values(rows);
                let (mut maybe, mut certain) = (VarSet::default(), VarSet::default());
                for (c, &s) in slots.iter().enumerate() {
                    maybe.insert(s);
                    if values.chunks(slots.len().max(1)).all(|row| row.get(c).is_some_and(|&v| v != UNBOUND)) {
                        certain.insert(s);
                    }
                }
                (maybe.clone(), certain, maybe)
            }
        };
        self.maybe[i] = maybe;
        self.certain[i] = certain;
        self.refs[i] = refs;
    }

    /// Whether evaluating `id` with any of `input` already bound gives the
    /// bottom-up result joined with those bindings.
    fn transparent(&self, plan: &Plan, id: NodeId, input: &VarSet) -> bool {
        let i = id as usize;
        match plan.node(id) {
            Node::Unit | Node::Empty | Node::Bgp(_) | Node::Path { .. } | Node::Table { .. } => true,
            Node::Join(a, ..) | Node::Minus(a, _) | Node::Graph(_, a) | Node::Check { input: a, .. } | Node::SetJoin { left: a, .. } => self.transparent(plan, a, input),
            Node::Union(a, b) => self.transparent(plan, a, input) && self.transparent(plan, b, input),
            Node::LeftJoin(a, b, cond, _) => {
                let mut right = self.refs[b as usize].clone();
                if let Some(c) = cond {
                    right = right.union(&self.vars_of_expr(plan, c));
                }
                self.transparent(plan, a, input) && right.intersect(input).is_subset(&self.certain[a as usize])
            }
            Node::Filter(e, a) | Node::Extend(a, _, e) => self.transparent(plan, a, input) && self.vars_of_expr(plan, e).intersect(input).is_subset(&self.certain[a as usize]),
            Node::Group { .. } | Node::OrderBy { .. } | Node::Distinct { .. } | Node::Slice { .. } | Node::Project { .. } => self.maybe[i].intersect(input).is_empty(),
        }
    }

    fn key_slot(&self, a: NodeId, b: NodeId) -> Option<Slot> {
        self.certain[a as usize].intersect(&self.certain[b as usize]).slots().next()
    }

    /// Rewrites `id` into a [`Node::Check`] over `left` when `right` is a
    /// BGP whose every variable `left` certainly binds (and, for MINUS,
    /// shares at least one with it): substituting a left solution leaves
    /// only point lookups, which s-graph answers in batches.
    fn check(&self, plan: &mut Plan, id: NodeId, left: NodeId, right: NodeId, negated: bool, minus: bool) -> bool {
        let Node::Bgp(triples) = plan.node(right) else { return false };
        let vars = &self.refs[right as usize];
        if triples.len == 0 || !vars.is_subset(&self.certain[left as usize]) || (minus && vars.is_empty()) {
            return false;
        }
        plan.nodes[id as usize] = Node::Check { input: left, triples, negated };
        true
    }

    /// Rewrites `id` into a [`Node::SetJoin`] when `left` and `right` are
    /// BGPs whose only shared variable — among the right side's and
    /// everything visible to it (`input`, the left side's) — is one both
    /// certainly bind: a left solution's match then depends on that value
    /// alone, so one native join of both BGPs decides all of them.
    fn set_join(&self, plan: &mut Plan, id: NodeId, left: NodeId, right: NodeId, input: &VarSet, kind: SetJoinKind) -> bool {
        let (Node::Bgp(l), Node::Bgp(r)) = (plan.node(left), plan.node(right)) else { return false };
        let shared = self.refs[right as usize].intersect(&input.union(&self.maybe[left as usize]));
        let mut slots = shared.slots();
        let (Some(slot), None) = (slots.next(), slots.next()) else { return false };
        if !self.certain[left as usize].contains(slot) || !self.certain[right as usize].contains(slot) {
            return false;
        }
        let start = plan.triples.len() as u32;
        plan.triples.extend_from_within(l.indices());
        plan.triples.extend_from_within(r.indices());
        plan.nodes[id as usize] = Node::SetJoin { left, joined: Range { start, len: l.len + r.len }, slot, kind };
        true
    }

    /// Chooses join modes top-down; `input` are the slots that may be bound
    /// when `id` is evaluated. Existence tests and joins against patterns
    /// the left side binds fully become batched [`Node::Check`]s; those
    /// sharing one variable with a left BGP become [`Node::SetJoin`]s.
    fn annotate(&self, plan: &mut Plan, id: NodeId, input: &VarSet) {
        let node = plan.node(id);
        let exists_in = |s: &Self, plan: &mut Plan, e: ExprId, scope: &VarSet| s.annotate_expr(plan, e, scope);
        match node {
            Node::Join(a, b, _) | Node::LeftJoin(a, b, _, _) => {
                self.annotate(plan, a, input);
                if matches!(node, Node::Join(..)) && self.check(plan, id, a, b, false, false) {
                    return;
                }
                if matches!(node, Node::LeftJoin(_, _, None, _)) && self.set_join(plan, id, a, b, input, SetJoinKind::LeftOuter) {
                    return;
                }
                let left = input.union(&self.maybe[a as usize]);
                let bind = self.transparent(plan, b, &left);
                let mode = if bind { JoinMode::Bind } else { JoinMode::Materialize { key: self.key_slot(a, b) } };
                let empty = VarSet::default();
                self.annotate(plan, b, if bind { &left } else { &empty });
                match &mut plan.nodes[id as usize] {
                    Node::Join(_, _, m) => *m = mode,
                    Node::LeftJoin(_, _, cond, m) => {
                        *m = mode;
                        if let Some(c) = *cond {
                            let scope = left.union(&self.maybe[b as usize]);
                            exists_in(self, plan, c, &scope);
                        }
                    }
                    _ => unreachable!(),
                }
            }
            Node::Union(a, b) => {
                self.annotate(plan, a, input);
                self.annotate(plan, b, input);
            }
            Node::Minus(a, b) => {
                self.annotate(plan, a, input);
                if !self.check(plan, id, a, b, true, true) && !self.set_join(plan, id, a, b, input, SetJoinKind::Anti) {
                    self.annotate(plan, b, &VarSet::default());
                }
            }
            Node::Graph(_, a) | Node::Check { input: a, .. } | Node::SetJoin { left: a, .. } | Node::Slice { input: a, .. } | Node::Distinct { input: a, .. } | Node::Project { input: a, .. } => self.annotate(plan, a, input),
            Node::Filter(e, a) | Node::Extend(a, _, e) => {
                self.annotate(plan, a, input);
                if let (Node::Filter(..), Expr::Exists(b) | Expr::NotExists(b)) = (node, plan.expr(e)) {
                    let negated = matches!(plan.expr(e), Expr::NotExists(_));
                    let kind = if negated { SetJoinKind::Anti } else { SetJoinKind::Semi };
                    if self.check(plan, id, a, b, negated, false) || self.set_join(plan, id, a, b, input, kind) {
                        return;
                    }
                }
                let scope = input.union(&self.maybe[a as usize]);
                exists_in(self, plan, e, &scope);
            }
            Node::Group { input: a, keys, aggregates } => {
                self.annotate(plan, a, input);
                let scope = input.union(&self.maybe[a as usize]);
                let exprs: Vec<ExprId> = plan.group_keys[keys.indices()].iter().map(|k| k.expr).chain(plan.aggregates[aggregates.indices()].iter().filter_map(|g| g.expr)).collect();
                for e in exprs {
                    exists_in(self, plan, e, &scope);
                }
            }
            Node::OrderBy { input: a, keys, .. } => {
                self.annotate(plan, a, input);
                let scope = input.union(&self.maybe[a as usize]);
                let exprs: Vec<ExprId> = plan.order_keys[keys.indices()].iter().map(|k| k.expr).collect();
                for e in exprs {
                    exists_in(self, plan, e, &scope);
                }
            }
            Node::Unit | Node::Empty | Node::Bgp(_) | Node::Path { .. } | Node::Table { .. } => {}
        }
    }

    fn annotate_list(&self, plan: &mut Plan, list: Range, scope: &VarSet) {
        let items: Vec<ExprId> = plan.expr_list(list).to_vec();
        for x in items {
            self.annotate_expr(plan, x, scope);
        }
    }

    /// Annotates EXISTS patterns inside an expression: they are evaluated
    /// with the current solution substituted.
    fn annotate_expr(&self, plan: &mut Plan, e: ExprId, scope: &VarSet) {
        match plan.expr(e) {
            Expr::Var(_) | Expr::Const(..) => {}
            Expr::Or(a, b) | Expr::And(a, b) | Expr::Eq(a, b) | Expr::Ne(a, b) | Expr::Lt(a, b) | Expr::Le(a, b) | Expr::Gt(a, b) | Expr::Ge(a, b) | Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
                self.annotate_expr(plan, a, scope);
                self.annotate_expr(plan, b, scope);
            }
            Expr::Not(a) | Expr::Plus(a) | Expr::Neg(a) | Expr::Cast(_, a) => self.annotate_expr(plan, a, scope),
            Expr::In(a, list) | Expr::NotIn(a, list) => {
                self.annotate_expr(plan, a, scope);
                self.annotate_list(plan, list, scope);
            }
            Expr::Call(_, list) => self.annotate_list(plan, list, scope),
            Expr::Exists(n) | Expr::NotExists(n) => self.annotate(plan, n, scope),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::{Format, LoadOptions, load_slice};
    use s_parser::parse_query;

    fn plan_for(store: &Store, text: &str) -> Plan {
        let query = parse_query(text.as_bytes()).unwrap();
        compile(store, &query, &mut LocalTerms::new(), &mut Arena::new()).unwrap()
    }

    fn store() -> Store {
        let mut st = Store::new();
        load_slice(&mut st, b"<http://a> <http://p> <http://b> .\n", Format::NTriples, &LoadOptions::default()).unwrap();
        st
    }

    fn join_modes(plan: &Plan) -> Vec<JoinMode> {
        plan.nodes.iter().filter_map(|n| match n {
            Node::Join(_, _, m) | Node::LeftJoin(_, _, _, m) => Some(*m),
            _ => None,
        }).collect()
    }

    #[test]
    fn adjacent_bgps_merge_and_constants_resolve() {
        let st = store();
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o . ?o <http://p> <http://missing> }");
        let Node::Bgp(r) = plan.node(plan.root) else { panic!("{:?}", plan.node(plan.root)) };
        assert_eq!(r.len, 2);
        let t = plan.triples(r)[1];
        assert!(matches!(t.o, PTerm::Const(id) if LocalTerms::is_local(id)), "missing constants get local ids");
        assert!(matches!(t.p, PTerm::Const(id) if !LocalTerms::is_local(id)));
        assert_eq!(plan.form_columns(), vec!["s", "o"]);
    }

    #[test]
    fn optional_without_outer_filters_binds() {
        let st = store();
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o OPTIONAL { ?o <http://p> ?z . ?z <http://p> ?s } }");
        assert_eq!(join_modes(&plan), vec![JoinMode::Bind]);
    }

    #[test]
    fn filters_seeing_outer_variables_materialize() {
        let st = store();
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o { FILTER(?o = <http://b>) } }");
        assert_eq!(join_modes(&plan), vec![JoinMode::Materialize { key: None }]);
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o { ?o <http://p> ?z FILTER(?o = <http://b>) } }");
        assert_eq!(join_modes(&plan), vec![JoinMode::Bind], "?o is certainly bound inside: substitution is safe");
    }

    #[test]
    fn nested_optional_with_a_maybe_bound_variable_materializes() {
        let st = store();
        let plan = plan_for(&st, "SELECT * WHERE { ?a <http://p> ?b OPTIONAL { ?b <http://p> ?c OPTIONAL { ?c <http://p> ?a } } }");
        let modes = join_modes(&plan);
        assert!(modes.contains(&JoinMode::Materialize { key: Some(1) }) || modes.iter().any(|m| matches!(m, JoinMode::Materialize { .. })), "{modes:?}");
    }

    /// The `Check` nodes of a plan: (number of patterns, negated).
    fn checks(plan: &Plan) -> Vec<(u32, bool)> {
        plan.nodes.iter().filter_map(|n| match n {
            Node::Check { triples, negated, .. } => Some((triples.len, *negated)),
            _ => None,
        }).collect()
    }

    #[test]
    fn fully_bound_existence_patterns_become_batched_checks() {
        let st = store();
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o FILTER EXISTS { ?o <http://p> ?s } }");
        assert_eq!(checks(&plan), vec![(1, false)]);
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o FILTER NOT EXISTS { ?o <http://p> <http://b> . ?s <http://p> ?o } }");
        assert_eq!(checks(&plan), vec![(2, true)]);
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o MINUS { ?o <http://p> ?s } }");
        assert_eq!(checks(&plan), vec![(1, true)]);
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o OPTIONAL { ?o <http://p> ?z } ?s <http://p> <http://b> }");
        assert_eq!(checks(&plan), vec![(1, false)], "a join whose right side the left binds fully");
    }

    #[test]
    fn existence_patterns_with_free_or_maybe_bound_variables_stay_as_they_are() {
        let st = store();
        for q in [
            "SELECT * WHERE { ?s <http://p> ?o FILTER NOT EXISTS { ?o <http://p> ?z } }",
            "SELECT * WHERE { ?s <http://p> ?o OPTIONAL { ?o <http://p> ?z } FILTER EXISTS { ?z <http://p> ?s } }",
            "SELECT * WHERE { ?s <http://p> ?o MINUS { <http://a> <http://p> <http://b> } }",
            "SELECT * WHERE { ?s <http://p> ?o FILTER (EXISTS { ?o <http://p> ?s } || ?s = ?o) }",
        ] {
            assert_eq!(checks(&plan_for(&st, q)), vec![], "{q}");
        }
    }

    /// The `SetJoin` nodes of a plan: (joined pattern count, kind).
    fn set_joins(plan: &Plan) -> Vec<(u32, SetJoinKind)> {
        plan.nodes.iter().filter_map(|n| match n {
            Node::SetJoin { joined, kind, .. } => Some((joined.len, *kind)),
            _ => None,
        }).collect()
    }

    #[test]
    fn single_variable_optionals_and_negations_become_set_joins() {
        let st = store();
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o OPTIONAL { ?s <http://q> ?v } }");
        assert_eq!(set_joins(&plan), vec![(2, SetJoinKind::LeftOuter)]);
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o FILTER NOT EXISTS { ?s <http://q> ?v . ?v <http://q> ?w } }");
        assert_eq!(set_joins(&plan), vec![(3, SetJoinKind::Anti)]);
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o FILTER EXISTS { ?s <http://q> ?v } }");
        assert_eq!(set_joins(&plan), vec![(2, SetJoinKind::Semi)]);
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p> ?o MINUS { ?s <http://q> ?v } }");
        assert_eq!(set_joins(&plan), vec![(2, SetJoinKind::Anti)]);
    }

    #[test]
    fn set_joins_need_one_shared_variable_between_two_bgps() {
        let st = store();
        for q in [
            // A join condition.
            "SELECT * WHERE { ?s <http://p> ?o OPTIONAL { ?s <http://q> ?v FILTER(?v != ?o) } }",
            // Two shared variables.
            "SELECT * WHERE { ?s <http://p> ?o OPTIONAL { ?s <http://q> ?o2 . ?o <http://q> ?z } }",
            // No shared variable.
            "SELECT * WHERE { ?s <http://p> ?o MINUS { ?x <http://q> ?v } }",
            // The right side is not a BGP.
            "SELECT * WHERE { ?s <http://p> ?o OPTIONAL { { ?s <http://q> ?v } UNION { ?s <http://p> ?v } } }",
            // The left side is not a BGP.
            "SELECT * WHERE { { ?s <http://p> ?o } UNION { ?s <http://q> ?o } MINUS { ?s <http://q> ?v } }",
            // A fully bound right side is a batched check instead.
            "SELECT * WHERE { ?s <http://p> ?o FILTER NOT EXISTS { ?o <http://p> ?s } }",
        ] {
            assert_eq!(set_joins(&plan_for(&st, q)), vec![], "{q}");
        }
    }

    #[test]
    fn subqueries_get_their_own_slots() {
        let st = store();
        let plan = plan_for(&st, "SELECT ?s WHERE { ?s <http://p> ?x { SELECT ?s WHERE { ?s <http://p> ?x } } }");
        assert_eq!(plan.width, 3, "outer ?s and ?x, inner ?x");
        assert!(matches!(join_modes(&plan)[0], JoinMode::Materialize { .. }));
    }

    #[test]
    fn distinct_limit_applies_the_limit_last() {
        let st = store();
        let plan = plan_for(&st, "SELECT DISTINCT ?s WHERE { ?s ?p ?o } LIMIT 1");
        let Node::Slice { input, .. } = plan.node(plan.root) else { panic!("{:?}", plan.node(plan.root)) };
        assert!(matches!(plan.node(input), Node::Distinct { .. }));
    }

    #[test]
    fn closure_paths_compile_to_automata() {
        let st = store();
        let plan = plan_for(&st, "SELECT * WHERE { ?s (<http://p>/<http://p>)* ?o }");
        let Node::Path { path, .. } = plan.node(plan.root) else { panic!() };
        let PathPlan::Automaton { zero_length, .. } = &plan.paths[path as usize] else { panic!() };
        assert!(zero_length);
        let plan = plan_for(&st, "SELECT * WHERE { ?s <http://p>/^<http://p> ?o }");
        assert!(matches!(plan.node(plan.root), Node::Bgp(r) if r.len == 2), "sequences of links are joins");
    }
}
