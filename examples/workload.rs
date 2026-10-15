//! Loads an N-Triples file and times store_bench-like queries against it
//! (placeholders bound from the command line). A profiling aid:
//! `workload <file.nt> [Q,..] [P1=<iri> ...]`.
use s_parser::FileStream;
use s_store::Store;
use s_store::load::{Format, LoadOptions, load};
use s_store::sparql::QueryOutput;
use std::time::Instant;

const QUERIES: &[(&str, &str)] = &[
    ("D1", "SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?p ORDER BY DESC(?n)"),
    ("D2", "SELECT ?c (COUNT(*) AS ?n) WHERE { ?s $TYPE ?c } GROUP BY ?c ORDER BY DESC(?n) LIMIT 10"),
    ("D3", "SELECT ?s (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?s ORDER BY DESC(?n) LIMIT 1"),
    ("D4", "SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ?p ?o FILTER(isLiteral(?o)) } GROUP BY ?p ORDER BY DESC(?n) LIMIT 1"),
    ("D5", "SELECT ?p (COUNT(*) AS ?n) WHERE { ?a ?p ?b . ?b $TYPE ?c } GROUP BY ?p ORDER BY DESC(?n) LIMIT 2"),
    ("Q2", "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }"),
    ("Q3", "SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE { ?s ?p ?o }"),
    ("Q4", "SELECT ?p ?o WHERE { $S ?p ?o }"),
    ("Q7", "SELECT (COUNT(*) AS ?n) WHERE { ?s $P1 ?o }"),
    ("Q8", "SELECT (COUNT(*) AS ?n) WHERE { ?s $TYPE $C1 }"),
    ("Q9", "SELECT (COUNT(*) AS ?n) WHERE { ?s $TYPE $C1 ; $PL ?v }"),
    ("Q10", "SELECT (COUNT(*) AS ?n) WHERE { ?s $P1 ?a ; $P2 ?b }"),
    ("Q11", "SELECT (COUNT(*) AS ?n) WHERE { ?a $L ?b . ?b $TYPE $C1 }"),
    ("Q13", "SELECT (COUNT(*) AS ?n) WHERE { ?s $TYPE $C1 OPTIONAL { ?s $PL ?v } }"),
    ("Q14", "SELECT (COUNT(*) AS ?n) WHERE { ?s $TYPE $C1 FILTER NOT EXISTS { ?s $PL ?v } }"),
    ("Q15", "SELECT (COUNT(*) AS ?n) WHERE { ?s $TYPE $C1 MINUS { ?s $PL ?v } }"),
    ("Q16", "SELECT (COUNT(*) AS ?n) WHERE { { ?s $P1 ?o } UNION { ?s $P2 ?o } }"),
    ("Q17", "SELECT (COUNT(*) AS ?n) WHERE { ?s $PL ?v FILTER(REGEX(STR(?v), \"[a-z]\", \"i\")) }"),
    ("Q18", "SELECT (COUNT(*) AS ?n) WHERE { ?s $PL ?v FILTER(STRLEN(STR(?v)) > 20) }"),
    ("Q19", "SELECT (COUNT(DISTINCT ?v) AS ?n) WHERE { ?s $PL ?v }"),
    ("Q20", "SELECT ?s ?v WHERE { ?s $PL ?v } ORDER BY ?v LIMIT 10"),
    ("Q21", "SELECT ?c (COUNT(*) AS ?n) WHERE { ?s $TYPE ?c } GROUP BY ?c HAVING (COUNT(*) > 100)"),
    ("X1", "SELECT (COUNT(*) AS ?n) WHERE { ?a $L ?b FILTER EXISTS { ?b $TYPE $C1 } }"),
    ("X2", "SELECT (COUNT(*) AS ?n) WHERE { ?a $L ?b FILTER NOT EXISTS { ?b $TYPE $C1 } }"),
    ("X3", "SELECT (COUNT(*) AS ?n) WHERE { ?s $TYPE $C1 MINUS { ?s $TYPE $C2 } }"),
    ("X4", "SELECT (COUNT(*) AS ?n) WHERE { ?s $P2 ?o FILTER EXISTS { ?s $TYPE $C1 } }"),
    ("Q24", "SELECT (COUNT(*) AS ?n) WHERE { ?x <http://www.w3.org/2000/01/rdf-schema#subClassOf>* ?y }"),
];

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("file.nt");
    let only: Option<Vec<String>> = args.next().filter(|a| a != "all").map(|s| s.split(',').map(str::to_string).collect());
    let mut binds = vec![("$TYPE".to_string(), "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>".to_string())];
    binds.extend(args.filter_map(|a| a.split_once('=').map(|(k, v)| (format!("${k}"), v.to_string()))));
    binds.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));

    let source = unsafe { FileStream::open(&path, 4 << 20).expect("open") };
    let mut store: Store = Store::with_config(s_store::store::StoreConfig { block_capacity: std::env::var("BLOCK").ok().and_then(|b| b.parse().ok()).unwrap_or(s_graph::DEFAULT_BLOCK_CAPACITY) });
    let t0 = Instant::now();
    load(&mut store, source, Format::NTriples, &LoadOptions::default()).expect("load");
    store.shrink_to_fit();
    eprintln!("load {:.2?}, {} bytes", t0.elapsed(), store.size_bytes());
    for (name, template) in QUERIES {
        if only.as_ref().is_some_and(|o| !o.iter().any(|q| q == name)) {
            continue;
        }
        let mut q = template.to_string();
        for (k, v) in &binds {
            q = q.replace(k.as_str(), v);
        }
        let repeat: usize = std::env::var("REPEAT").ok().and_then(|r| r.parse().ok()).unwrap_or(1);
        for _ in 1..repeat {
            store.query(&q).expect("query");
        }
        let t = Instant::now();
        let out = store.query(&q).expect("query");
        let elapsed = t.elapsed();
        let size = match &out {
            QueryOutput::Solutions(r) if r.len() == 1 && r.variables().len() == 1 => r.key(&store, r.row(0)[0]).unwrap_or_default(),
            QueryOutput::Solutions(r) => format!("{} rows", r.len()),
            other => format!("{other:?}").chars().take(40).collect(),
        };
        println!("{name:<4} {:>10.2} ms  {size}", elapsed.as_secs_f64() * 1e3);
    }
}
