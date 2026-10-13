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
use super::expr::{term_value, value_id};
use super::hash::RowSet;
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

impl<S: BlockStorage> Ctx<'_, S> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn group(&self, env: &mut Env, input: NodeId, keys: Range, aggregates: Range, active: Active, row: &mut [u32], k: &mut Sink<'_>) -> Flow {
        let keys = &self.plan.group_keys[keys.indices()];
        let aggs = &self.plan.aggregates[aggregates.indices()];
        let input_scope = self.plan.scope(input);
        let mut groups = RowSet::new(keys.len());
        let mut accs: Vec<Acc> = Vec::new();
        let mut distinct: Vec<RowSet> = aggs.iter().map(|a| RowSet::new(if a.expr.is_some() { 2 } else { 1 + input_scope.len() })).collect();
        let mut key = vec![UNBOUND; keys.len()];
        let mut distinct_key: Vec<u32> = Vec::new();
        let _ = self.eval(env, input, active, row, &mut |env, row| {
            for (slot, gk) in key.iter_mut().zip(keys) {
                *slot = self.expr_id(env, gk.expr, row, active).unwrap_or(UNBOUND);
            }
            let (g, new) = groups.insert(&key);
            if new {
                accs.extend(aggs.iter().map(|a| Acc::new(a.kind)));
            }
            for (j, agg) in aggs.iter().enumerate() {
                let value = match agg.expr {
                    None => Some(UNBOUND),
                    Some(e) => self.expr_id(env, e, row, active),
                };
                if agg.distinct {
                    distinct_key.clear();
                    distinct_key.push(g);
                    match (agg.expr, value) {
                        (None, _) => distinct_key.extend(input_scope.iter().map(|&s| row[s as usize])),
                        (Some(_), Some(v)) => distinct_key.push(v),
                        (Some(_), None) => distinct_key.push(UNBOUND),
                    }
                    if !distinct[j].insert(&distinct_key).1 {
                        continue;
                    }
                }
                self.accumulate(env, &mut accs[g as usize * aggs.len() + j], agg, value);
            }
            CONTINUE
        });
        if groups.is_empty() && keys.is_empty() {
            groups.insert(&[]);
            accs.extend(aggs.iter().map(|a| Acc::new(a.kind)));
        }
        let mut newly: Vec<Slot> = Vec::with_capacity(keys.len() + aggs.len());
        for g in 0..groups.len() as u32 {
            newly.clear();
            let mut compatible = true;
            for (i, gk) in keys.iter().enumerate() {
                let v = groups.row(g)[i];
                if let (Some(slot), true) = (gk.slot, v != UNBOUND) {
                    compatible &= set(row, slot, v, &mut newly);
                }
            }
            for (j, agg) in aggs.iter().enumerate() {
                if let Some(v) = self.finish(env, &accs[g as usize * aggs.len() + j], agg) {
                    compatible &= set(row, agg.out, v, &mut newly);
                }
            }
            let flow = if compatible { k(env, row) } else { CONTINUE };
            newly.iter().for_each(|&s| row[s as usize] = UNBOUND);
            flow?;
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
