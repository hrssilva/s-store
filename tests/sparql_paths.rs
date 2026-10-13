//! SPARQL property paths.

mod common;
use common::*;

const CHAIN: &str = r#"
ex:a ex:p ex:b . ex:b ex:p ex:c . ex:c ex:p ex:d . ex:d ex:p ex:b .
ex:a ex:q ex:x . ex:x ex:r ex:y .
ex:a ex:q ex:z . ex:z ex:r ex:y .
"#;

fn chain() -> s_store::Store {
    store_with(CHAIN)
}

fn iris(names: &[&str]) -> Vec<Vec<Option<String>>> {
    let owned: Vec<String> = names.iter().map(|n| format!("<http://ex/{n}>")).collect();
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let r: Vec<&[&str]> = refs.iter().map(std::slice::from_ref).collect();
    rows(&r)
}

#[test]
fn sequence_alternative_and_inverse_keep_duplicates() {
    let st = chain();
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:a ex:q/ex:r ?o }"), iris(&["y", "y"]), "sequences have bag semantics");
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:a ex:p|ex:q ?o }"), iris(&["b", "x", "z"]));
    assert_eq!(select(&st, "SELECT ?s WHERE { ex:b ^ex:p ?s }"), iris(&["a", "d"]));
    assert_eq!(select(&st, "SELECT ?s WHERE { ?s ex:q/ex:r ex:y }"), iris(&["a", "a"]));
}

#[test]
fn transitive_closures_are_distinct() {
    let st = chain();
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:a ex:p+ ?o }"), iris(&["b", "c", "d"]));
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:a ex:p* ?o }"), iris(&["a", "b", "c", "d"]));
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:a ex:p? ?o }"), iris(&["a", "b"]));
    assert_eq!(select(&st, "SELECT ?s WHERE { ?s ex:p+ ex:b }"), iris(&["a", "b", "c", "d"]));
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:a (ex:q/ex:r)+ ?o }"), iris(&["y"]));
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:y (^ex:r/^ex:q)* ?o }"), iris(&["a", "y"]));
}

#[test]
fn closures_with_both_ends_bound_or_free() {
    let st = chain();
    assert!(ask(&st, "ASK { ex:a ex:p+ ex:d }"));
    assert!(!ask(&st, "ASK { ex:d ex:p+ ex:a }"));
    assert!(ask(&st, "ASK { ex:nowhere ex:p* ex:nowhere }"), "zero-length paths match any term");
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:nowhere ex:p* ?o }"), iris(&["nowhere"]));
    let pairs = select(&st, "SELECT ?s ?o WHERE { ?s ex:p+ ?o }");
    assert_eq!(pairs.len(), 4 * 3, "a reaches b,c,d and each of b,c,d reaches b,c,d");
    let reflexive = select(&st, "SELECT ?s WHERE { ?s ex:r* ?s }");
    assert_eq!(reflexive.len(), 7, "every node of the graph (a, b, c, d, x, y, z) reaches itself: {reflexive:?}");
}

#[test]
fn negated_property_sets() {
    let st = chain();
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:a !ex:p ?o }"), iris(&["x", "z"]));
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:a !(ex:p|ex:q) ?o }"), iris(&[]));
    assert_eq!(select(&st, "SELECT ?s WHERE { ex:y !^ex:q ?s }"), iris(&["x", "z"]));
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:a (!ex:q)+ ?o }"), iris(&["b", "c", "d"]));
}

#[test]
fn paths_join_with_patterns() {
    let st = chain();
    assert_eq!(select(&st, "SELECT ?m WHERE { ex:a ex:q ?m . ?m ex:r+ ex:y }"), iris(&["x", "z"]));
}

#[test]
fn paths_inside_named_graphs() {
    let mut st = chain();
    load_ttl(&mut st, "ex:a ex:p ex:g .", s_store::GraphRef::Named("<http://ex/G>"));
    assert_eq!(select(&st, "SELECT ?o WHERE { GRAPH ex:G { ex:a ex:p+ ?o } }"), iris(&["g"]));
}
