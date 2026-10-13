#![allow(dead_code)]

use s_store::load::{Format, LoadOptions, load_slice};
use s_store::sparql::QueryOutput;
use s_store::{GraphRef, Store};

pub const PREFIXES: &str = "PREFIX ex: <http://ex/>\nPREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\nPREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>\n";

pub fn store_with(ttl: &str) -> Store {
    let mut store = Store::new();
    load_ttl(&mut store, ttl, GraphRef::Default);
    store
}

pub fn load_ttl(store: &mut Store, ttl: &str, graph: GraphRef<'_>) {
    let doc = format!("@prefix ex: <http://ex/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n{ttl}");
    load_slice(store, doc.as_bytes(), Format::Turtle, &LoadOptions { graph, ..Default::default() }).expect("test data loads");
}

/// Runs a SELECT and returns its rows as keys (`None` for unbound), sorted,
/// with columns in projection order. `PREFIXES` is prepended.
pub fn select(store: &Store, query: &str) -> Vec<Vec<Option<String>>> {
    let mut rows = select_ordered(store, query);
    rows.sort();
    rows
}

/// Like [`select`], keeping the result order.
pub fn select_ordered(store: &Store, query: &str) -> Vec<Vec<Option<String>>> {
    let text = format!("{PREFIXES}{query}");
    match store.query(&text).unwrap_or_else(|e| panic!("query failed: {e}\n{text}")) {
        QueryOutput::Solutions(s) => (0..s.len()).map(|i| s.row(i).iter().map(|&id| s.key(store, id)).collect()).collect(),
        other => panic!("expected solutions, got {other:?}"),
    }
}

pub fn variables(store: &Store, query: &str) -> Vec<String> {
    match store.query(&format!("{PREFIXES}{query}")).unwrap() {
        QueryOutput::Solutions(s) => s.variables().to_vec(),
        other => panic!("expected solutions, got {other:?}"),
    }
}

pub fn ask(store: &Store, query: &str) -> bool {
    match store.query(&format!("{PREFIXES}{query}")).unwrap() {
        QueryOutput::Boolean(b) => b,
        other => panic!("expected a boolean, got {other:?}"),
    }
}

/// Runs a CONSTRUCT/DESCRIBE and returns `s p o` lines, sorted.
pub fn construct(store: &Store, query: &str) -> Vec<String> {
    match store.query(&format!("{PREFIXES}{query}")).unwrap() {
        QueryOutput::Graph(g) => {
            let mut v: Vec<String> = (0..g.len())
                .map(|i| {
                    let [s, p, o] = g.triple(i);
                    format!("{} {} {}", g.key(store, s).unwrap(), g.key(store, p).unwrap(), g.key(store, o).unwrap())
                })
                .collect();
            v.sort();
            v
        }
        other => panic!("expected a graph, got {other:?}"),
    }
}

/// Builds expected rows from `"-"` (unbound) or key strings, sorted.
pub fn rows(expected: &[&[&str]]) -> Vec<Vec<Option<String>>> {
    let mut v: Vec<Vec<Option<String>>> = expected.iter().map(|r| r.iter().map(|&k| (k != "-").then(|| k.to_string())).collect()).collect();
    v.sort();
    v
}

pub fn ordered_rows(expected: &[&[&str]]) -> Vec<Vec<Option<String>>> {
    expected.iter().map(|r| r.iter().map(|&k| (k != "-").then(|| k.to_string())).collect()).collect()
}

pub fn int(v: i64) -> String {
    format!("\"{v}\"^^<http://www.w3.org/2001/XMLSchema#integer>")
}

pub fn dec(v: &str) -> String {
    format!("\"{v}\"^^<http://www.w3.org/2001/XMLSchema#decimal>")
}

pub fn boolean(v: bool) -> String {
    format!("\"{v}\"^^<http://www.w3.org/2001/XMLSchema#boolean>")
}
