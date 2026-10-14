//! Compares s-graph's `join_run_minus` with the store's own MINUS /
//! NOT EXISTS evaluation on the same default-graph shapes.
use s_graph::graph::{JoinQuery, Term, TriplePattern, join_run, join_run_minus};
use s_parser::FileStream;
use s_store::Store;
use s_store::graph::GraphName;
use s_store::load::{Format, LoadOptions, load};
use s_store::sparql::QueryOutput;
use std::time::Instant;

const TYPE: &str = "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>";

fn best<T>(mut f: impl FnMut() -> T) -> (f64, T) {
    let mut out = f();
    let mut best = f64::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        out = f();
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
    }
    (best, out)
}

fn main() {
    let path = std::env::args().nth(1).expect("file.nt");
    let source = unsafe { FileStream::open(&path, 4 << 20).expect("open") };
    let mut store = Store::new();
    load(&mut store, source, Format::NTriples, &LoadOptions::default()).expect("load");
    store.shrink_to_fit();
    let id = |k: &str| store.dictionary().lookup(k).expect(k).0;
    let pred = |k: &str| store.index().predicates().get(s_dict::TermId(id(k))).expect(k);
    let tree = store.index().partition(GraphName::Default).unwrap().tree();
    let (ty, link, c0, c1) = (pred(TYPE), pred("<http://ex/link>"), id("<http://ex/C0>"), id("<http://ex/C1>"));
    let tp = |s: Term, p: u32, o: Term| TriplePattern { subject: s, predicate: p, object: o };
    let (v0, v1) = (Term::Var(0), Term::Var(1));

    let cases: [(&str, String, JoinQuery, Vec<TriplePattern>); 3] = [
        (
            "X3 one var",
            format!("SELECT (COUNT(*) AS ?n) WHERE {{ ?s {TYPE} <http://ex/C0> MINUS {{ ?s {TYPE} <http://ex/C1> }} }}"),
            JoinQuery { num_vars: 1, patterns: vec![tp(v0, ty, Term::Const(c0))] },
            vec![tp(v0, ty, Term::Const(c1))],
        ),
        (
            "X2 fan-out",
            format!("SELECT (COUNT(*) AS ?n) WHERE {{ ?a <http://ex/link> ?b FILTER NOT EXISTS {{ ?b {TYPE} <http://ex/C0> }} }}"),
            JoinQuery { num_vars: 2, patterns: vec![tp(v0, link, v1)] },
            vec![tp(v1, ty, Term::Const(c0))],
        ),
        (
            "two vars",
            "SELECT (COUNT(*) AS ?n) WHERE { ?a <http://ex/link> ?b MINUS { ?b <http://ex/link> ?a } }".to_string(),
            JoinQuery { num_vars: 2, patterns: vec![tp(v0, link, v1)] },
            vec![tp(v1, link, v0)],
        ),
    ];
    for (name, sparql, query, minus) in cases {
        let (store_ms, n) = best(|| match store.query(&sparql).expect("query") {
            QueryOutput::Solutions(r) => r.key(&store, r.row(0)[0]).unwrap_or_default(),
            _ => unreachable!(),
        });
        let (pos_ms, pos) = best(|| join_run(&query, tree).len());
        let (native_ms, kept) = best(|| join_run_minus(&query, &minus, tree).len());
        println!("{name:<11} store {store_ms:>8.2} ms ({n})  s-graph {native_ms:>8.2} ms ({kept}; positive join alone {pos_ms:.2} ms, {pos} rows)");
    }
}
