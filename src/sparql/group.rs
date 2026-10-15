//! Blocking operators: GROUP BY with aggregates, and ORDER BY.
//!
//! Groups are keyed by their key terms' ids in a [`RowSet`] (one flat
//! buffer, no per-group key allocation), with one accumulator per group and
//! aggregate in a flat vector.
//!
//! ORDER BY evaluates each row's sort keys to term ids, then decodes every
//! *distinct* key id once, sorts those values in SPARQL order and replaces
//! each id by its rank: the row sort itself compares integers, never terms.

use super::exec::{Active, CONTINUE, Ctx, Env, Flow, Sink};
use super::path::View;
use crate::graph::Partition;
use super::expr::{term_value, value_id};
use super::hash::RowSet;
use super::idset::IdSet;
use super::plan::*;
use super::value::{self, Num, Value};
use s_graph::BlockStorage;
use s_parser::sparql::expr::AggKind;
use std::cmp::Ordering;

enum Acc {
    Count(i64),
    /// `None` after an error.
    Sum(Option<Num>),
    Avg(Option<Num>, i64),
    Min(Option<u32>),
    Max(Option<u32>),
    Sample(Option<u32>),
    /// Text so far; `None` after an error.
    Concat(Option<String>, bool),
}

impl Acc {
    fn new(kind: AggKind) -> Acc {
        match kind {
            AggKind::Count => Acc::Count(0),
            AggKind::Sum => Acc::Sum(Some(Num::Int(0))),
            AggKind::Avg => Acc::Avg(Some(Num::Int(0)), 0),
            AggKind::Min => Acc::Min(None),
            AggKind::Max => Acc::Max(None),
            AggKind::Sample => Acc::Sample(None),
            AggKind::GroupConcat => Acc::Concat(Some(String::new()), true),
        }
    }
}

fn add(a: Num, b: Num) -> Option<Num> {
    let kind = a.kind().max(b.kind());
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => x.checked_add(y).map(Num::Int),
        _ => Some(Num::of_kind(kind, a.as_f64() + b.as_f64())),
    }
}

/// The values a DISTINCT aggregate has seen.
enum Seen {
    /// Per group (`(group, value)`, or `(group, solution...)` for `*`).
    Rows(RowSet),
    /// Ungrouped, over an expression: one bit per dictionary id, query-local
    /// ids aside. No hashing for the common `COUNT(DISTINCT ?v)`.
    Ids(IdSet, RowSet),
}

/// A GROUP s-graph can count without listing the matches: the input is one
/// triple pattern without a repeated variable, every aggregate is `COUNT(*)`,
/// `COUNT(DISTINCT *)` (a pattern's matches are a set) or `COUNT(?v)` of a
/// variable of the pattern (always bound), and the grouping is none or the
/// pattern's predicate variable alone.
struct CountShape {
    triple: PTriple,
    by_predicate: bool,
}

fn count_shape(plan: &Plan, input: NodeId, keys: &[GroupKey], aggs: &[Aggregate]) -> Option<CountShape> {
    let Node::Bgp(r) = plan.node(input) else { return None };
    let [triple] = plan.triples(r) else { return None };
    let vars: Vec<Slot> = [triple.s, triple.p, triple.o].into_iter().filter_map(|t| if let PTerm::Var(v) = t { Some(v) } else { None }).collect();
    if (1..vars.len()).any(|i| vars[..i].contains(&vars[i])) {
        return None;
    }
    let countable = |a: &Aggregate| {
        a.kind == AggKind::Count
            && match a.expr {
                None => true,
                Some(e) => !a.distinct && matches!(plan.expr(e), Expr::Var(v) if vars.contains(&v)),
            }
    };
    if !aggs.iter().all(countable) {
        return None;
    }
    let by_predicate = match keys {
        [] => false,
        [key] => matches!((plan.expr(key.expr), triple.p), (Expr::Var(k), PTerm::Var(p)) if k == p),
        _ => return None,
    };
    (keys.is_empty() || by_predicate).then_some(CountShape { triple: *triple, by_predicate })
}

impl<S: BlockStorage> Ctx<'_, S> {
    /// GROUP via s-graph's counting traversal (see [`CountShape`]): no
    /// solution is produced, only counts. The graph's size answers an
    /// all-variable pattern; a GROUP BY of the predicate counts once per
    /// predicate. `None` when the active graph is a merge (whose counts
    /// would double-count shared triples) or the grouping predicate is
    /// already bound: the caller then groups solutions as usual.
    #[allow(clippy::too_many_arguments)]
    fn count_group(&self, env: &mut Env, shape: &CountShape, keys: &[GroupKey], aggs: &[Aggregate], active: Active, row: &mut [u32], k: &mut Sink<'_>) -> Option<Flow> {
        let part = match self.view(active) {
            View::Empty => None,
            View::One(part) => Some(part),
            View::Merged(_) => return None,
        };
        let resolved = part.and_then(|part| Some((part, self.pattern_bounds(&shape.triple, row)?)));
        let count = |part: &Partition<S>, bounds: [Option<u32>; 3]| if bounds == [None; 3] { part.len() } else { part.tree().count_matching(bounds) };
        let counts = |n: u64| aggs.iter().map(|_| Acc::Count(n as i64)).collect::<Vec<_>>();
        if !shape.by_predicate {
            let n = resolved.map_or(0, |(part, bounds)| count(part, bounds));
            return Some(self.emit_group(env, &[], &counts(n), keys, aggs, row, k));
        }
        if resolved.is_some_and(|(_, bounds)| bounds[1].is_some()) {
            return None;
        }
        let Some((part, mut bounds)) = resolved else { return Some(CONTINUE) };
        for dense in 0..self.store.index().predicates().terms().len() as u32 {
            bounds[1] = Some(dense);
            let n = count(part, bounds);
            if n > 0 {
                let flow = self.emit_group(env, &[self.predicate_term(dense)], &counts(n), keys, aggs, row, k);
                if flow.is_break() {
                    return Some(flow);
                }
            }
        }
        Some(CONTINUE)
    }

    /// Binds one group's keys and aggregate results and calls `k`.
    #[allow(clippy::too_many_arguments)]
    fn emit_group(&self, env: &mut Env, key_values: &[u32], accs: &[Acc], keys: &[GroupKey], aggs: &[Aggregate], row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let mut newly: Vec<Slot> = Vec::with_capacity(keys.len() + aggs.len());
        let mut compatible = true;
        for (gk, &v) in keys.iter().zip(key_values) {
            if let (Some(slot), true) = (gk.slot, v != UNBOUND) {
                compatible &= set(row, slot, v, &mut newly);
            }
        }
        for (agg, acc) in aggs.iter().zip(accs) {
            if let Some(v) = self.finish(env, acc, agg) {
                compatible &= set(row, agg.out, v, &mut newly);
            }
        }
        let flow = if compatible { k(env, row) } else { CONTINUE };
        newly.iter().for_each(|&s| row[s as usize] = UNBOUND);
        flow
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn group(&self, env: &mut Env, input: NodeId, keys: Range, aggregates: Range, active: Active, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let keys = &self.plan.group_keys[keys.indices()];
        let aggs = &self.plan.aggregates[aggregates.indices()];
        if let Some(shape) = count_shape(self.plan, input, keys, aggs)
            && let Some(flow) = self.count_group(env, &shape, keys, aggs, active, row, k)
        {
            return flow;
        }
        let input_scope = self.plan.scope(input);
        let mut groups = RowSet::new(keys.len());
        let mut accs: Vec<Acc> = Vec::new();
        let mut distinct: Vec<Seen> = aggs
            .iter()
            .map(|a| match (a.distinct && a.expr.is_some() && keys.is_empty(), a.expr) {
                (true, _) => Seen::Ids(self.id_set(env), RowSet::new(1)),
                (false, Some(_)) => Seen::Rows(RowSet::new(2)),
                (false, None) => Seen::Rows(RowSet::new(1 + input_scope.len())),
            })
            .collect();
        let mut key = vec![UNBOUND; keys.len()];
        let mut distinct_key: Vec<u32> = Vec::new();
        let _ = self.eval(env, input, active, row, &mut |env, row| {
            for (slot, gk) in key.iter_mut().zip(keys) {
                *slot = self.expr_id(env, gk.expr, row, active).unwrap_or(UNBOUND);
            }
            // Ungrouped: one group, no key to hash per solution.
            let (g, new) = if keys.is_empty() { (0, groups.is_empty() && groups.insert(&[]).1) } else { groups.insert(&key) };
            if new {
                accs.extend(aggs.iter().map(|a| Acc::new(a.kind)));
            }
            for (j, agg) in aggs.iter().enumerate() {
                let value = match agg.expr {
                    None => Some(UNBOUND),
                    Some(e) => self.expr_id(env, e, row, active),
                };
                if agg.distinct {
                    let new = match &mut distinct[j] {
                        // Unbound inputs are never accumulated.
                        Seen::Ids(ids, others) => value.is_some_and(|v| ids.insert_covered(v).unwrap_or_else(|| others.insert(&[v]).1)),
                        Seen::Rows(seen) => {
                            distinct_key.clear();
                            distinct_key.push(g);
                            match (agg.expr, value) {
                                (None, _) => distinct_key.extend(input_scope.iter().map(|&s| row[s as usize])),
                                (Some(_), Some(v)) => distinct_key.push(v),
                                (Some(_), None) => distinct_key.push(UNBOUND),
                            }
                            seen.insert(&distinct_key).1
                        }
                    };
                    if !new {
                        continue;
                    }
                }
                self.accumulate(env, &mut accs[g as usize * aggs.len() + j], agg, value);
            }
            CONTINUE
        });
        for seen in distinct {
            if let Seen::Ids(ids, _) = seen {
                Self::return_id_set(env, ids);
            }
        }
        if groups.is_empty() && keys.is_empty() {
            groups.insert(&[]);
            accs.extend(aggs.iter().map(|a| Acc::new(a.kind)));
        }
        for g in 0..groups.len() {
            let accs = &accs[g * aggs.len()..(g + 1) * aggs.len()];
            self.emit_group(env, groups.row(g as u32), accs, keys, aggs, row, k)?;
        }
        CONTINUE
    }

    /// Adds one input value. Unbound/error inputs (`None`) are skipped, as
    /// Jena and RDF4J do; a bound value of the wrong type still poisons
    /// SUM/AVG/GROUP_CONCAT.
    fn accumulate(&self, env: &mut Env, acc: &mut Acc, agg: &Aggregate, value: Option<u32>) {
        if value.is_none() {
            return;
        }
        let mark = env.arena.mark();
        match acc {
            Acc::Count(n) => *n += i64::from(value.is_some()),
            Acc::Sum(sum) | Acc::Avg(sum, _) => {
                let n = value.and_then(|id| term_value(self, env, id).ok()).and_then(Value::num);
                *sum = match (*sum, n) {
                    (Some(s), Some(n)) => add(s, n),
                    _ => None,
                };
                if let Acc::Avg(_, count) = acc {
                    *count += 1;
                }
            }
            Acc::Min(best) | Acc::Max(best) => {
                if let Some(id) = value {
                    let replace = match *best {
                        None => true,
                        Some(b) => {
                            let (x, y) = (term_value(self, env, id).ok(), term_value(self, env, b).ok());
                            let o = value::order(&env.arena, x, y);
                            if matches!(agg.kind, AggKind::Min) { o == Ordering::Less } else { o == Ordering::Greater }
                        }
                    };
                    if replace {
                        *best = Some(id);
                    }
                }
            }
            Acc::Sample(s) => {
                if s.is_none() {
                    *s = value;
                }
            }
            Acc::Concat(text, first) => {
                let piece = value.and_then(|id| term_value(self, env, id).ok()).and_then(Value::lit);
                match (text.as_mut(), piece) {
                    (Some(t), Some(l)) => {
                        if !*first {
                            if let Some(sep) = agg.separator {
                                if let Some(Value::Lit(s)) = self.plan_const(sep) {
                                    t.push_str(env.arena.text(s.lex));
                                }
                            } else {
                                t.push(' ');
                            }
                        }
                        t.push_str(env.arena.text(l.lex));
                        *first = false;
                    }
                    _ => *text = None,
                }
            }
        }
        env.arena.truncate(mark);
    }

    fn plan_const(&self, e: ExprId) -> Option<Value> {
        match self.plan.expr(e) {
            Expr::Const(_, v) => Some(v),
            _ => None,
        }
    }

    fn finish(&self, env: &mut Env, acc: &Acc, _agg: &Aggregate) -> Option<u32> {
        let mark = env.arena.mark();
        let v = match acc {
            Acc::Count(n) => Some(env.arena.number(Num::Int(*n))),
            Acc::Sum(s) => s.map(|n| env.arena.number(n)),
            Acc::Avg(s, count) => match (s, count) {
                (Some(_), 0) => Some(env.arena.number(Num::Int(0))),
                (Some(n), &c) => {
                    let kind = n.kind().max(value::NumKind::Dec);
                    Some(env.arena.number(Num::of_kind(kind, n.as_f64() / c as f64)))
                }
                (None, _) => None,
            },
            Acc::Min(id) | Acc::Max(id) | Acc::Sample(id) => {
                env.arena.truncate(mark);
                return *id;
            }
            Acc::Concat(text, _) => text.as_deref().map(|t| env.arena.string(t)),
        };
        let id = v.map(|v| value_id(self, env, v));
        env.arena.truncate(mark);
        id
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn order_by(&self, env: &mut Env, input: NodeId, keys: Range, scope: Range, active: Active, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let keys = &self.plan.order_keys[keys.indices()];
        let scope = self.plan.slots(scope);
        let (w, nk) = (scope.len(), keys.len());
        let mut rows: Vec<u32> = Vec::new();
        let mut sort_keys: Vec<u32> = Vec::new();
        let _ = self.eval(env, input, active, row, &mut |env, row| {
            rows.extend(scope.iter().map(|&s| row[s as usize]));
            for key in keys {
                sort_keys.push(self.expr_id(env, key.expr, row, active).unwrap_or(UNBOUND));
            }
            CONTINUE
        });
        let n = rows.len().checked_div(w).unwrap_or(sort_keys.len() / nk.max(1));
        let ranks = self.rank(env, &sort_keys);
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_by(|&x, &y| {
            for (j, key) in keys.iter().enumerate() {
                let (a, b) = (ranks[x as usize * nk + j], ranks[y as usize * nk + j]);
                let o = if key.descending { b.cmp(&a) } else { a.cmp(&b) };
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        });
        let mark = env.saved.len();
        env.saved.extend(scope.iter().map(|&s| row[s as usize]));
        for &i in &order {
            for (c, &s) in scope.iter().enumerate() {
                row[s as usize] = rows[i as usize * w + c];
            }
            let flow = k(env, row);
            if flow.is_break() {
                for (c, &s) in scope.iter().enumerate() {
                    row[s as usize] = env.saved[mark + c];
                }
                env.saved.truncate(mark);
                return flow;
            }
        }
        for (c, &s) in scope.iter().enumerate() {
            row[s as usize] = env.saved[mark + c];
        }
        env.saved.truncate(mark);
        CONTINUE
    }

    /// Replaces each id by its rank among the distinct ids in SPARQL order
    /// (equal values share a rank).
    fn rank(&self, env: &mut Env, ids: &[u32]) -> Vec<u32> {
        let mut distinct: Vec<u32> = ids.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        let mark = env.arena.mark();
        let values: Vec<Option<Value>> = distinct.iter().map(|&id| if id == UNBOUND { None } else { term_value(self, env, id).ok() }).collect();
        let mut by_order: Vec<usize> = (0..distinct.len()).collect();
        by_order.sort_by(|&a, &b| value::order(&env.arena, values[a], values[b]));
        let mut rank_of = vec![0u32; distinct.len()];
        let mut rank = 0u32;
        for (i, &d) in by_order.iter().enumerate() {
            if i > 0 && value::order(&env.arena, values[by_order[i - 1]], values[d]) != Ordering::Equal {
                rank += 1;
            }
            rank_of[d] = rank;
        }
        env.arena.truncate(mark);
        ids.iter().map(|id| rank_of[distinct.binary_search(id).expect("collected above")]).collect()
    }
}

/// Binds `slot` unless bound to a different value (`false`).
fn set(row: &mut [u32], slot: Slot, v: u32, newly: &mut Vec<Slot>) -> bool {
    let cell = &mut row[slot as usize];
    if *cell == UNBOUND {
        *cell = v;
        newly.push(slot);
        true
    } else {
        *cell == v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use crate::sparql::compile::compile;
    use crate::sparql::locals::LocalTerms;
    use crate::sparql::value::Arena;

    /// The count shape of the plan's (first) GROUP node, as
    /// (grouped by predicate, the pattern).
    fn shape(text: &str) -> Option<bool> {
        let query = s_parser::parse_query(text.as_bytes()).unwrap();
        let plan = compile(&Store::new(), &query, &mut LocalTerms::new(), &mut Arena::new()).unwrap();
        let (input, keys, aggregates) = plan.nodes.iter().find_map(|n| match *n {
            Node::Group { input, keys, aggregates } => Some((input, keys, aggregates)),
            _ => None,
        })?;
        count_shape(&plan, input, &plan.group_keys[keys.indices()], &plan.aggregates[aggregates.indices()]).map(|s| s.by_predicate)
    }

    #[test]
    fn single_pattern_counts_are_countable() {
        assert_eq!(shape("SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }"), Some(false));
        assert_eq!(shape("SELECT (COUNT(?o) AS ?n) (COUNT(DISTINCT *) AS ?d) WHERE { ?s <http://p> ?o }"), Some(false));
        assert_eq!(shape("SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?p"), Some(true));
    }

    #[test]
    fn other_aggregates_and_groupings_are_not() {
        for q in [
            "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o . ?o ?p ?z }",
            "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?s }",
            "SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE { ?s ?p ?o }",
            "SELECT (COUNT(?x) AS ?n) WHERE { ?s ?p ?o }",
            "SELECT (SUM(?o) AS ?n) WHERE { ?s ?p ?o }",
            "SELECT ?s (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?s",
            "SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?p ?s",
            "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o FILTER(?o != ?s) }",
        ] {
            assert_eq!(shape(q), None, "{q}");
        }
    }
}
