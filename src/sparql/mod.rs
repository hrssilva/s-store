//! SPARQL 1.1 query evaluation.
//!
//! `s-parser` parses the query into its algebra; [`compile`] maps that onto a
//! [`plan::Plan`] over row slots and term ids; [`exec`] evaluates it,
//! delegating triple patterns, joins and paths to s-graph ([`patterns`]).
//! See each module for the details:
//!
//! - [`plan`]: the plan IR and the row/id model.
//! - [`compile`]: scoping, path rewriting, join-mode analysis.
//! - [`exec`]: the push-based evaluator.
//! - [`patterns`]: BGPs as s-graph native joins and scans; paths as automata.
//! - [`expr`], [`value`]: expressions over arena-backed values.
//! - [`group`]: GROUP BY/aggregates and ORDER BY.
//!
//! Results are term ids; [`Solutions::key`] and [`Triples::key`] resolve them
//! to canonical keys, including terms the query computed.

pub mod compile;
pub mod exec;
pub mod expr;
pub mod group;
pub mod hash;
pub mod locals;
pub mod path;
pub mod patterns;
pub mod plan;
pub mod value;

use crate::error::Result;
use crate::store::Store;
use exec::{Active, CONTINUE, Ctx, Env};
use hash::RowSet;
use locals::LocalTerms;
use plan::{Form, PTerm, Plan, TTerm, UNBOUND};
use s_dict::TermId;
use s_graph::BlockStorage;
use std::ops::ControlFlow;
use value::Arena;

/// A query's result.
#[derive(Debug)]
pub enum QueryOutput {
    Solutions(Solutions),
    Boolean(bool),
    Graph(Triples),
}

/// SELECT results: rows of term ids ([`UNBOUND`] for unbound), one column
/// per projected variable.
#[derive(Debug)]
pub struct Solutions {
    variables: Vec<String>,
    ids: Vec<u32>,
    rows: usize,
    locals: LocalTerms,
}

impl Solutions {
    pub fn variables(&self) -> &[String] {
        &self.variables
    }

    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn row(&self, i: usize) -> &[u32] {
        let w = self.variables.len();
        &self.ids[i * w..(i + 1) * w]
    }

    /// The canonical key of a result id; `None` for [`UNBOUND`].
    pub fn key<S: BlockStorage>(&self, store: &Store<S>, id: u32) -> Option<String> {
        resolve(&self.locals, store, id)
    }
}

/// CONSTRUCT/DESCRIBE results: distinct triples of term ids.
#[derive(Debug)]
pub struct Triples {
    ids: Vec<[u32; 3]>,
    locals: LocalTerms,
}

impl Triples {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn triple(&self, i: usize) -> [u32; 3] {
        self.ids[i]
    }

    pub fn key<S: BlockStorage>(&self, store: &Store<S>, id: u32) -> Option<String> {
        resolve(&self.locals, store, id)
    }
}

fn resolve<S: BlockStorage>(locals: &LocalTerms, store: &Store<S>, id: u32) -> Option<String> {
    if id == UNBOUND {
        return None;
    }
    match locals.key(id) {
        Some(k) => Some(k.to_owned()),
        None => store.key(TermId(id)),
    }
}

impl<S: BlockStorage> Store<S> {
    /// Parses, compiles and evaluates a SPARQL 1.1 query.
    pub fn query(&self, sparql: &str) -> Result<QueryOutput> {
        let query = s_parser::parse_query(sparql.as_bytes())?;
        let mut locals = LocalTerms::new();
        let mut arena = Arena::new();
        let plan = compile::compile(self, &query, &mut locals, &mut arena)?;
        let mut env = Env::new(arena, locals);
        let output = run(self, &plan, &mut env)?;
        let locals = env.into_locals();
        Ok(match output {
            Raw::Solutions(variables, ids, rows) => QueryOutput::Solutions(Solutions { variables, ids, rows, locals }),
            Raw::Boolean(b) => QueryOutput::Boolean(b),
            Raw::Graph(ids) => QueryOutput::Graph(Triples { ids, locals }),
        })
    }

    /// Compiles a query and returns its plan, for inspection.
    pub fn explain(&self, sparql: &str) -> Result<Plan> {
        let query = s_parser::parse_query(sparql.as_bytes())?;
        compile::compile(self, &query, &mut LocalTerms::new(), &mut Arena::new())
    }
}

enum Raw {
    Solutions(Vec<String>, Vec<u32>, usize),
    Boolean(bool),
    Graph(Vec<[u32; 3]>),
}

fn run<S: BlockStorage>(store: &Store<S>, plan: &Plan, env: &mut Env) -> Result<Raw> {
    let ctx = Ctx::new(store, plan);
    let mut row = vec![UNBOUND; plan.width as usize];
    match &plan.form {
        Form::Select(columns) => {
            let mut ids = Vec::new();
            let mut rows = 0;
            let _ = ctx.eval(env, plan.root, Active::Default, &mut row, &mut |_, row| {
                ids.extend(columns.iter().map(|&(_, slot)| row[slot as usize]));
                rows += 1;
                CONTINUE
            });
            Ok(Raw::Solutions(columns.iter().map(|(n, _)| n.clone()).collect(), ids, rows))
        }
        Form::Ask => Ok(Raw::Boolean(ctx.eval(env, plan.root, Active::Default, &mut row, &mut |_, _| ControlFlow::Break(())).is_break())),
        Form::Construct(template) => {
            let mut out = RowSet::new(3);
            let mut solution = 0u64;
            let mut triple = [0u32; 3];
            let _ = ctx.eval(env, plan.root, Active::Default, &mut row, &mut |env, row| {
                solution += 1;
                'triples: for t in template {
                    for (i, term) in t.iter().enumerate() {
                        triple[i] = match *term {
                            TTerm::Const(id) => id,
                            TTerm::Var(slot) if row[slot as usize] != UNBOUND => row[slot as usize],
                            TTerm::Var(_) => continue 'triples,
                            TTerm::Blank(n) => env.locals.intern(&format!("_:c{solution}_{n}")),
                        };
                    }
                    if valid_triple(&ctx, env, triple) {
                        out.insert(&triple);
                    }
                }
                CONTINUE
            });
            Ok(Raw::Graph((0..out.len() as u32).map(|i| out.row(i).try_into().expect("width 3")).collect()))
        }
        Form::Describe(targets) => {
            let mut resources: Vec<u32> = targets.iter().filter_map(|t| match t {
                PTerm::Const(id) => Some(*id),
                PTerm::Var(_) => None,
            }).collect();
            let vars: Vec<u32> = targets.iter().filter_map(|t| match t {
                PTerm::Var(s) => Some(*s),
                PTerm::Const(_) => None,
            }).collect();
            if !vars.is_empty() {
                let _ = ctx.eval(env, plan.root, Active::Default, &mut row, &mut |_, row| {
                    resources.extend(vars.iter().map(|&s| row[s as usize]).filter(|&v| v != UNBOUND));
                    CONTINUE
                });
            }
            resources.sort_unstable();
            resources.dedup();
            let view = ctx.view(Active::Default);
            let mut out = Vec::new();
            for r in resources.into_iter().filter(|&r| !LocalTerms::is_local(r)) {
                out.extend(view.scan([Some(r), None, None]).map(|(s, p, o)| [s, ctx.predicate_term(p), o]));
            }
            out.sort_unstable();
            out.dedup();
            Ok(Raw::Graph(out))
        }
    }
}

/// CONSTRUCT output must be RDF: no literal subject, an IRI predicate.
fn valid_triple<S: BlockStorage>(ctx: &Ctx<'_, S>, env: &mut Env, [s, p, _]: [u32; 3]) -> bool {
    let prefix = |env: &mut Env, id: u32| -> [u8; 2] {
        let key: &[u8] = if LocalTerms::is_local(id) {
            env.locals.key(id).map_or(b"", str::as_bytes)
        } else if ctx.store.dictionary().key_into(TermId(id), &mut env.key) {
            &env.key
        } else {
            b""
        };
        [key.first().copied().unwrap_or(0), key.get(1).copied().unwrap_or(0)]
    };
    let (s, p) = (prefix(env, s), prefix(env, p));
    !matches!(s[0], b'"' | 0) && p[0] == b'<' && p[1] != b'<'
}
