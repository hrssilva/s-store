//! The executable query plan: s-parser's algebra with every variable mapped
//! to a row slot and every constant resolved to a term id.
//!
//! Like s-parser's [`QueryData`](s_parser::sparql::algebra::QueryData), a plan
//! is a set of flat pools addressed by `u32` ids and [`Range`]s, so nodes are
//! `Copy` and a plan is a handful of allocations regardless of its size.
//!
//! Solutions are rows of `u32` term ids, one slot per variable, [`UNBOUND`]
//! for an unbound slot. Ids below [`crate::dict::LOCAL_ID_BASE`] are
//! dictionary ids; ids above are terms the query computed
//! ([`super::locals::LocalTerms`]). Since every RDF term has one canonical
//! key, id equality is term equality, for dictionary and local ids alike.

use super::value::Value;
use crate::graph::GraphName;
use s_parser::sparql::expr::{AggKind, Builtin};

pub type Slot = u32;
pub type NodeId = u32;
pub type ExprId = u32;
pub type PathId = u32;

/// An unbound row slot.
pub const UNBOUND: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    pub start: u32,
    pub len: u32,
}

impl Range {
    pub fn indices(self) -> std::ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }
}

/// A pattern position: a variable's slot or a constant term id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PTerm {
    Var(Slot),
    Const(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PTriple {
    pub s: PTerm,
    pub p: PTerm,
    pub o: PTerm,
}

/// How a join evaluates its right-hand side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinMode {
    /// Once per left solution, with the left solution's bindings substituted
    /// (index nested loop). Chosen when substitution cannot change the
    /// right side's meaning (see `compile::Analysis`).
    Bind,
    /// Once, independently; left solutions probe the materialized result.
    /// `key` is a slot certainly bound on both sides, used to index it.
    Materialize { key: Option<Slot> },
}

#[derive(Clone, Copy, Debug)]
pub enum Node {
    /// One empty solution.
    Unit,
    /// No solution.
    Empty,
    /// Range into `Plan::triples`.
    Bgp(Range),
    /// A path with set semantics (`*`, `+`, `?`), or a negated property set.
    Path { s: PTerm, path: PathId, o: PTerm },
    Join(NodeId, NodeId, JoinMode),
    LeftJoin(NodeId, NodeId, Option<ExprId>, JoinMode),
    Union(NodeId, NodeId),
    Minus(NodeId, NodeId),
    Graph(PTerm, NodeId),
    Filter(ExprId, NodeId),
    Extend(NodeId, Slot, ExprId),
    /// `keys` ranges over `Plan::group_keys`, `aggregates` over
    /// `Plan::aggregates`, `scope` over `Plan::slots` (slots cleared on output).
    Group { input: NodeId, keys: Range, aggregates: Range },
    /// Range into `Plan::order_keys`; `scope` ranges over `Plan::slots`.
    OrderBy { input: NodeId, keys: Range, scope: Range },
    /// `hidden` ranges over `Plan::slots`: the subquery's own variables,
    /// unbound in its output.
    Project { input: NodeId, hidden: Range },
    /// `scope` ranges over `Plan::slots`: the slots a solution is compared on.
    Distinct { input: NodeId, scope: Range },
    Slice { input: NodeId, offset: u64, limit: Option<u64> },
    /// `slots` and `rows` range over `Plan::slots` and `Plan::values`
    /// (row-major, [`UNBOUND`] for `UNDEF`).
    Table { slots: Range, rows: Range },
}

#[derive(Clone, Copy, Debug)]
pub enum Expr {
    Var(Slot),
    /// A constant: its term id and its value, decoded once at compile time
    /// into the frozen prefix of the expression arena.
    Const(u32, Value),
    Or(ExprId, ExprId),
    And(ExprId, ExprId),
    Not(ExprId),
    Eq(ExprId, ExprId),
    Ne(ExprId, ExprId),
    Lt(ExprId, ExprId),
    Le(ExprId, ExprId),
    Gt(ExprId, ExprId),
    Ge(ExprId, ExprId),
    /// Range into `Plan::expr_lists`.
    In(ExprId, Range),
    NotIn(ExprId, Range),
    Add(ExprId, ExprId),
    Sub(ExprId, ExprId),
    Mul(ExprId, ExprId),
    Div(ExprId, ExprId),
    Plus(ExprId),
    Neg(ExprId),
    Call(Builtin, Range),
    Cast(Cast, ExprId),
    Exists(NodeId),
    NotExists(NodeId),
}

/// XSD constructor functions (`xsd:integer(?x)`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cast {
    String,
    Boolean,
    Integer,
    Decimal,
    Float,
    Double,
    DateTime,
}

#[derive(Clone, Copy, Debug)]
pub struct GroupKey {
    pub expr: ExprId,
    /// Where the key's value goes: the variable itself for `GROUP BY ?v`,
    /// the alias for `GROUP BY (expr AS ?v)`, `None` for an unnamed key.
    pub slot: Option<Slot>,
}

#[derive(Clone, Copy, Debug)]
pub struct Aggregate {
    pub kind: AggKind,
    pub distinct: bool,
    /// `None` for `COUNT(*)`.
    pub expr: Option<ExprId>,
    /// `GROUP_CONCAT` separator, a constant expression.
    pub separator: Option<ExprId>,
    pub out: Slot,
}

#[derive(Clone, Copy, Debug)]
pub struct OrderKey {
    pub expr: ExprId,
    pub descending: bool,
}

/// A compiled path (see `super::path`): an epsilon-free automaton over
/// dense predicate ids, or a negated property set.
#[derive(Clone, Debug)]
pub enum PathPlan {
    Automaton { automaton: super::path::Nfa, zero_length: bool },
    /// `!(...)`: forward edges whose predicate is not in `forward`, and,
    /// when `inverse` is set, backward edges not in `backward`.
    Negated { forward_any: bool, forward: Vec<u32>, inverse_any: bool, backward: Vec<u32> },
}

/// A CONSTRUCT template position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TTerm {
    Var(Slot),
    Const(u32),
    /// A template blank node: fresh per solution.
    Blank(u32),
}

#[derive(Clone, Debug)]
pub enum Form {
    /// Output columns: `(name, slot)`.
    Select(Vec<(String, Slot)>),
    Ask,
    Construct(Vec<[TTerm; 3]>),
    Describe(Vec<PTerm>),
}

/// The dataset a query runs against.
#[derive(Clone, Debug)]
pub struct Dataset {
    /// The graphs merged into the default graph.
    pub default: Vec<GraphName>,
    /// The named graphs `GRAPH` ranges over; `None` for every named graph
    /// of the store (except the reserved properties graph).
    pub named: Option<Vec<u32>>,
}

#[derive(Debug)]
pub struct Plan {
    pub root: NodeId,
    pub form: Form,
    pub dataset: Dataset,
    /// Row width: number of slots.
    pub width: u32,
    pub nodes: Vec<Node>,
    pub exprs: Vec<Expr>,
    pub triples: Vec<PTriple>,
    pub paths: Vec<PathPlan>,
    pub slots: Vec<Slot>,
    pub values: Vec<u32>,
    pub expr_lists: Vec<ExprId>,
    pub group_keys: Vec<GroupKey>,
    pub aggregates: Vec<Aggregate>,
    pub order_keys: Vec<OrderKey>,
    /// For every node, the slots it may bind (range over `slots`): what a
    /// materialized result keeps.
    pub scopes: Vec<Range>,
}

impl Plan {
    pub fn node(&self, id: NodeId) -> Node {
        self.nodes[id as usize]
    }

    pub fn expr(&self, id: ExprId) -> Expr {
        self.exprs[id as usize]
    }

    pub fn triples(&self, r: Range) -> &[PTriple] {
        &self.triples[r.indices()]
    }

    pub fn slots(&self, r: Range) -> &[Slot] {
        &self.slots[r.indices()]
    }

    pub fn values(&self, r: Range) -> &[u32] {
        &self.values[r.indices()]
    }

    pub fn expr_list(&self, r: Range) -> &[ExprId] {
        &self.expr_lists[r.indices()]
    }

    pub fn scope(&self, node: NodeId) -> &[Slot] {
        self.slots(self.scopes[node as usize])
    }

    /// A SELECT's output column names.
    pub fn form_columns(&self) -> Vec<&str> {
        match &self.form {
            Form::Select(cols) => cols.iter().map(|(n, _)| n.as_str()).collect(),
            _ => Vec::new(),
        }
    }
}
