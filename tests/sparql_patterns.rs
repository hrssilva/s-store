//! SPARQL graph patterns: BGPs, OPTIONAL, UNION, MINUS, FILTER scoping,
//! BIND, VALUES, GRAPH and datasets, subqueries, EXISTS.

mod common;
use common::*;
use s_store::GraphRef;

const PEOPLE: &str = r#"
ex:alice ex:name "Alice" ; ex:knows ex:bob, ex:carol ; ex:age 30 .
ex:bob   ex:name "Bob"   ; ex:knows ex:carol ; ex:age 25 ; ex:email "bob@ex" .
ex:carol ex:name "Carol" ; ex:age 35 .
ex:dave  ex:knows ex:alice .
"#;

fn people() -> s_store::Store {
    store_with(PEOPLE)
}

#[test]
fn single_pattern_with_constants() {
    let st = people();
    assert_eq!(select(&st, "SELECT ?n WHERE { ex:bob ex:name ?n }"), rows(&[&["\"Bob\""]]));
    assert_eq!(select(&st, "SELECT ?s WHERE { ?s ex:knows ex:carol }"), rows(&[&["<http://ex/alice>"], &["<http://ex/bob>"]]));
    assert_eq!(select(&st, "SELECT ?s WHERE { ?s ex:unknown ?o }"), rows(&[]));
    assert_eq!(select(&st, "SELECT ?s WHERE { ?s ex:knows ex:nobody }"), rows(&[]));
}

/// A pattern with one free position, after substitution, is an automaton
/// search with the bound positions as parameters; every position, a dense
/// predicate and an early stop must still come out right.
#[test]
fn patterns_with_one_free_position() {
    let st = people();
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:alice ex:knows ?o }"), rows(&[&["<http://ex/bob>"], &["<http://ex/carol>"]]));
    assert_eq!(select(&st, "SELECT ?p WHERE { ex:bob ?p ex:carol }"), rows(&[&["<http://ex/knows>"]]));
    assert_eq!(select(&st, "SELECT ?s WHERE { ?s ex:age 25 }"), rows(&[&["<http://ex/bob>"]]));
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:alice ex:knows ?o } LIMIT 1").len(), 1);
    assert!(ask(&st, "ASK { ?s ex:knows ex:alice }"));
    // Bound by an outer solution: one lookup per row (BIND forces a join).
    assert_eq!(
        select(&st, "SELECT ?x ?n WHERE { ?x ex:age ?a OPTIONAL { ?x ex:email ?e } BIND(1 AS ?one) OPTIONAL { ?x ex:name ?n FILTER(?one = 1) } }").len(),
        3
    );
    // A merged default graph keeps the generic path.
    let mut merged = people();
    load_ttl(&mut merged, "ex:alice ex:knows ex:bob, ex:erin .", GraphRef::Named("<http://g1>"));
    load_ttl(&mut merged, "ex:alice ex:knows ex:erin, ex:frank .", GraphRef::Named("<http://g2>"));
    assert_eq!(select(&merged, "SELECT ?o FROM <http://g1> FROM <http://g2> WHERE { ex:alice ex:knows ?o }").len(), 3, "bob, erin once, frank");
}

#[test]
fn star_and_chain_joins() {
    let st = people();
    assert_eq!(
        select(&st, "SELECT ?a ?b WHERE { ?x ex:knows ?y . ?x ex:name ?a . ?y ex:name ?b }"),
        rows(&[&["\"Alice\"", "\"Bob\""], &["\"Alice\"", "\"Carol\""], &["\"Bob\"", "\"Carol\""]])
    );
    assert_eq!(
        select(&st, "SELECT ?z WHERE { ex:dave ex:knows ?x . ?x ex:knows ?y . ?y ex:knows ?z }"),
        rows(&[&["<http://ex/carol>"]])
    );
}

#[test]
fn variable_predicates_join_with_other_positions() {
    let st = store_with("ex:p ex:label \"P\" . ex:a ex:p ex:b . ex:a ex:label \"A\" .");
    assert_eq!(select(&st, "SELECT ?l ?o WHERE { ?p ex:label ?l . ex:a ?p ?o }"), rows(&[&["\"P\"", "<http://ex/b>"]]));
    assert_eq!(
        select(&st, "SELECT ?p WHERE { ex:a ?p ?o }"),
        rows(&[&["<http://ex/label>"], &["<http://ex/p>"]])
    );
}

/// A BGP joined per left solution is compiled once and rerun with each
/// row's bindings as parameters: predicate variables (dense ids), variables
/// bound to terms that cannot occur (unknown predicates, query-computed
/// terms) and variables in two positions must behave as substitution would.
#[test]
fn bgps_rerun_with_row_bindings_as_parameters() {
    let st = store_with("ex:p ex:label \"P\" . ex:a ex:p ex:b . ex:b ex:p ex:c . ex:a ex:label \"A\" . ex:c ex:q ex:q .");
    // ?p bound per row (VALUES forces a join), used as a predicate.
    assert_eq!(
        select(&st, "SELECT ?p ?o WHERE { VALUES ?p { ex:p ex:label ex:nope } ex:a ?p ?o }"),
        rows(&[&["<http://ex/label>", "\"A\""], &["<http://ex/p>", "<http://ex/b>"]])
    );
    // Bound to a string BIND computed: no stored term matches it.
    assert_eq!(select(&st, "SELECT ?s WHERE { BIND(CONCAT(\"A\", \"\") AS ?l) ?s ex:label ?l }"), rows(&[&["<http://ex/a>"]]));
    assert_eq!(select(&st, "SELECT ?s WHERE { BIND(CONCAT(\"x\", \"y\") AS ?l) ?s ex:label ?l }"), rows(&[]));
    // One variable as predicate and object, bound or free.
    assert_eq!(select(&st, "SELECT ?x WHERE { ?s ?x ?x }"), rows(&[&["<http://ex/q>"]]));
    assert_eq!(select(&st, "SELECT ?s WHERE { VALUES ?x { ex:q ex:p } ?s ?x ?x }"), rows(&[&["<http://ex/c>"]]));
    // A two-pattern BGP rerun for many rows, each with a different binding.
    assert_eq!(
        select(&st, "SELECT ?x ?z WHERE { VALUES ?x { ex:a ex:b ex:c } ?x ex:p ?y . ?y ex:p ?z }"),
        rows(&[&["<http://ex/a>", "<http://ex/c>"]])
    );
}

#[test]
fn repeated_variables_within_a_pattern() {
    let st = store_with("ex:a ex:p ex:a . ex:a ex:p ex:b .");
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:p ?x }"), rows(&[&["<http://ex/a>"]]));
}

#[test]
fn select_star_projects_in_order_of_appearance() {
    let st = people();
    assert_eq!(variables(&st, "SELECT * WHERE { ?s ex:age ?age }"), vec!["s", "age"]);
    assert_eq!(select(&st, "SELECT * WHERE { ex:carol ex:age ?age }"), rows(&[&[&int(35)]]));
}

#[test]
fn optional_keeps_unmatched_rows() {
    let st = people();
    assert_eq!(
        select(&st, "SELECT ?n ?e WHERE { ?s ex:name ?n OPTIONAL { ?s ex:email ?e } }"),
        rows(&[&["\"Alice\"", "-"], &["\"Bob\"", "\"bob@ex\""], &["\"Carol\"", "-"]])
    );
}

#[test]
fn optional_filter_is_a_join_condition() {
    let st = people();
    assert_eq!(
        select(&st, "SELECT ?n ?o WHERE { ?s ex:name ?n OPTIONAL { ?s ex:knows ?o FILTER(?o != ex:bob) } }"),
        rows(&[&["\"Alice\"", "<http://ex/carol>"], &["\"Bob\"", "<http://ex/carol>"], &["\"Carol\"", "-"]])
    );
}

#[test]
fn nested_optional_with_unbound_shared_variable() {
    // The classic non-well-designed case: ?v is bound only in the inner
    // OPTIONAL's right side and in the outer right side.
    let st = store_with("ex:a ex:p 1 . ex:a ex:q 2 . ex:b ex:p 1 .");
    let q = "SELECT ?s ?v WHERE { ?s ex:p 1 OPTIONAL { ?s ex:q ?v } }";
    assert_eq!(select(&st, q), rows(&[&["<http://ex/a>", &int(2)], &["<http://ex/b>", "-"]]));
}

#[test]
fn union_concatenates() {
    let st = people();
    assert_eq!(
        select(&st, "SELECT ?x WHERE { { ex:alice ex:knows ?x } UNION { ?x ex:knows ex:alice } }"),
        rows(&[&["<http://ex/bob>"], &["<http://ex/carol>"], &["<http://ex/dave>"]])
    );
    assert_eq!(select(&st, "SELECT ?x ?y WHERE { { ex:bob ex:email ?x } UNION { ex:carol ex:age ?y } }"), rows(&[&["\"bob@ex\"", "-"], &["-", &int(35)]]));
}

#[test]
fn minus_removes_compatible_solutions() {
    let st = people();
    assert_eq!(
        select(&st, "SELECT ?s WHERE { ?s ex:name ?n MINUS { ?s ex:email ?e } }"),
        rows(&[&["<http://ex/alice>"], &["<http://ex/carol>"]])
    );
    // No shared variables: nothing is removed.
    assert_eq!(select(&st, "SELECT ?s WHERE { ?s ex:age 25 MINUS { ?x ex:email ?e } }"), rows(&[&["<http://ex/bob>"]]));
}

#[test]
fn filter_scope_is_the_group() {
    let st = people();
    assert_eq!(select(&st, "SELECT ?s WHERE { FILTER(?a > 26) ?s ex:age ?a }"), rows(&[&["<http://ex/alice>"], &["<http://ex/carol>"]]));
    // A filter in an inner group cannot see the outer group's variables.
    assert_eq!(select(&st, "SELECT ?s WHERE { ?s ex:age ?a { FILTER(bound(?a)) } }"), rows(&[]));
}

#[test]
fn bind_extends_and_joins() {
    let st = people();
    assert_eq!(
        select(&st, "SELECT ?s ?next WHERE { ?s ex:age ?a BIND(?a + 1 AS ?next) FILTER(?next > 30) }"),
        rows(&[&["<http://ex/alice>", &int(31)], &["<http://ex/carol>", &int(36)]])
    );
    // A computed term that also exists in the data joins with it.
    assert_eq!(select(&st, "SELECT ?s WHERE { BIND(ex:carol AS ?o) ?s ex:knows ?o }"), rows(&[&["<http://ex/alice>"], &["<http://ex/bob>"]]));
}

#[test]
fn values_tables_join() {
    let st = people();
    assert_eq!(
        select(&st, "SELECT ?s ?n WHERE { VALUES ?s { ex:alice ex:carol ex:nobody } ?s ex:name ?n }"),
        rows(&[&["<http://ex/alice>", "\"Alice\""], &["<http://ex/carol>", "\"Carol\""]])
    );
    assert_eq!(
        select(&st, "SELECT ?s ?a WHERE { ?s ex:age ?a } VALUES (?s ?a) { (ex:bob UNDEF) (UNDEF 35) }"),
        rows(&[&["<http://ex/bob>", &int(25)], &["<http://ex/carol>", &int(35)]])
    );
}

#[test]
fn graph_patterns_and_named_graph_enumeration() {
    let mut st = people();
    load_ttl(&mut st, "ex:x ex:p ex:y .", GraphRef::Named("<http://ex/g1>"));
    load_ttl(&mut st, "ex:x ex:p ex:z . ex:z ex:p ex:w .", GraphRef::Named("<http://ex/g2>"));
    assert_eq!(select(&st, "SELECT ?g ?o WHERE { GRAPH ?g { ex:x ex:p ?o } }"), rows(&[&["<http://ex/g1>", "<http://ex/y>"], &["<http://ex/g2>", "<http://ex/z>"]]));
    assert_eq!(select(&st, "SELECT ?o WHERE { GRAPH ex:g2 { ?s ex:p ?o } }"), rows(&[&["<http://ex/w>"], &["<http://ex/z>"]]));
    // The default graph is not the union of the named graphs.
    assert_eq!(select(&st, "SELECT ?o WHERE { ex:x ex:p ?o }"), rows(&[]));
    // A join must stay within one graph.
    assert_eq!(select(&st, "SELECT ?g ?w WHERE { GRAPH ?g { ex:x ex:p ?z . ?z ex:p ?w } }"), rows(&[&["<http://ex/g2>", "<http://ex/w>"]]));
    // Graph variable bound beforehand.
    assert_eq!(select(&st, "SELECT ?o WHERE { VALUES ?g { ex:g1 } GRAPH ?g { ex:x ex:p ?o } }"), rows(&[&["<http://ex/y>"]]));
    assert_eq!(select(&st, "SELECT ?g WHERE { GRAPH ?g { } }"), rows(&[&["<http://ex/g1>"], &["<http://ex/g2>"]]));
}

#[test]
fn from_clauses_define_the_dataset() {
    let mut st = s_store::Store::new();
    load_ttl(&mut st, "ex:x ex:p ex:y .", GraphRef::Named("<http://ex/g1>"));
    load_ttl(&mut st, "ex:y ex:p ex:z .", GraphRef::Named("<http://ex/g2>"));
    // FROM merges graphs into the default graph: joins cross them.
    assert_eq!(select(&st, "SELECT ?z FROM ex:g1 FROM ex:g2 WHERE { ex:x ex:p ?y . ?y ex:p ?z }"), rows(&[&["<http://ex/z>"]]));
    // FROM NAMED restricts the named graphs.
    assert_eq!(select(&st, "SELECT ?g FROM NAMED ex:g2 WHERE { GRAPH ?g { ?s ?p ?o } }"), rows(&[&["<http://ex/g2>"]]));
    // A duplicate triple across merged graphs is one triple.
    load_ttl(&mut st, "ex:x ex:p ex:y .", GraphRef::Named("<http://ex/g2>"));
    assert_eq!(select(&st, "SELECT ?y FROM ex:g1 FROM ex:g2 WHERE { ex:x ex:p ?y }"), rows(&[&["<http://ex/y>"]]));
}

#[test]
fn subqueries_have_their_own_scope() {
    let st = people();
    assert_eq!(
        select(&st, "SELECT ?s ?n WHERE { ?s ex:name ?n { SELECT ?s WHERE { ?s ex:age ?n } } }"),
        rows(&[&["<http://ex/alice>", "\"Alice\""], &["<http://ex/bob>", "\"Bob\""], &["<http://ex/carol>", "\"Carol\""]]),
        "the inner ?n is a different variable"
    );
    assert_eq!(
        select(&st, "SELECT ?s WHERE { ?s ex:name ?n { SELECT ?s WHERE { ?s ex:age ?a } ORDER BY DESC(?a) LIMIT 1 } }"),
        rows(&[&["<http://ex/carol>"]])
    );
}

#[test]
fn exists_and_not_exists_substitute_the_current_row() {
    let st = people();
    assert_eq!(
        select(&st, "SELECT ?s WHERE { ?s ex:name ?n FILTER EXISTS { ?s ex:knows ?o } }"),
        rows(&[&["<http://ex/alice>"], &["<http://ex/bob>"]])
    );
    assert_eq!(select(&st, "SELECT ?s WHERE { ?s ex:name ?n FILTER NOT EXISTS { ?x ex:knows ?s } }"), rows(&[]));
    assert_eq!(select(&st, "SELECT ?s WHERE { ?s ex:knows ?o FILTER NOT EXISTS { ?o ex:knows ?z } }"), rows(&[&["<http://ex/alice>"], &["<http://ex/bob>"]]));
}

/// Fully bound EXISTS / NOT EXISTS / MINUS / join patterns are checked in
/// batches; these exercise batch boundaries, early stops and graphs.
#[test]
fn batched_existence_checks_match_row_by_row_semantics() {
    let mut ttl = String::new();
    for i in 0..5000 {
        ttl.push_str(&format!("ex:n{i} ex:next ex:n{} .\n", i + 1));
        if i % 3 == 0 {
            ttl.push_str(&format!("ex:n{} ex:back ex:n{i} .\n", i + 1));
        }
    }
    let st = store_with(&ttl);
    let count = |q: &str| select(&st, &format!("SELECT (COUNT(*) AS ?n) WHERE {{ {q} }}"))[0][0].clone().unwrap();
    assert_eq!(count("?a ex:next ?b FILTER EXISTS { ?b ex:back ?a }"), int(1667));
    assert_eq!(count("?a ex:next ?b FILTER NOT EXISTS { ?b ex:back ?a }"), int(3333));
    assert_eq!(count("?a ex:next ?b MINUS { ?b ex:back ?a }"), int(3333));
    assert_eq!(count("?a ex:next ?b OPTIONAL { ?a ex:none ?z } ?b ex:back ?a"), int(1667));
    assert_eq!(count("?a ex:next ?b FILTER NOT EXISTS { ?b ex:back ?a . ?a ex:next ex:n1 }"), int(4999));
    assert_eq!(count("?a ex:next ?b FILTER EXISTS { ?b ex:back ?a . ?a ex:next \"not in the data\" }"), int(0));
    assert_eq!(count("?a ex:next ?b FILTER EXISTS { ?b ex:unknown ?a }"), int(0));
    assert_eq!(
        select_ordered(&st, "SELECT ?a WHERE { VALUES ?a { ex:n0 ex:n1 ex:n3 ex:n6 } ?a ex:next ?b FILTER EXISTS { ?b ex:back ?a } } LIMIT 2"),
        ordered_rows(&[&["<http://ex/n0>"], &["<http://ex/n3>"]]),
        "order is kept and LIMIT stops"
    );
    assert!(ask(&st, "ASK { ?a ex:next ?b FILTER EXISTS { ?b ex:back ?a } }"));
}

#[test]
fn batched_existence_checks_use_the_active_graph() {
    let mut st = s_store::Store::new();
    load_ttl(&mut st, "ex:a ex:p ex:b . ex:c ex:p ex:d .", GraphRef::Named("<http://ex/g1>"));
    load_ttl(&mut st, "ex:a ex:p ex:b . ex:b ex:q ex:a .", GraphRef::Named("<http://ex/g2>"));
    load_ttl(&mut st, "ex:d ex:q ex:c .", GraphRef::Named("<http://ex/g3>"));
    assert_eq!(
        select(&st, "SELECT ?g ?s WHERE { GRAPH ?g { ?s ex:p ?o FILTER EXISTS { ?o ex:q ?s } } }"),
        rows(&[&["<http://ex/g2>", "<http://ex/a>"]])
    );
    assert_eq!(
        select(&st, "SELECT ?s FROM ex:g1 FROM ex:g3 WHERE { ?s ex:p ?o FILTER EXISTS { ?o ex:q ?s } }"),
        rows(&[&["<http://ex/c>"]]),
        "a merged default graph holds a triple if any of its graphs does"
    );
}

/// OPTIONAL / EXISTS / NOT EXISTS / MINUS sharing one variable run as a
/// native join plus an id set; these pin their bag semantics.
const SHARED: &str = r#"
ex:a ex:p ex:o1, ex:o2 ; ex:q ex:v1, ex:v2 .
ex:b ex:p ex:o1, ex:o3 .
ex:c ex:p ex:o4 ; ex:q ex:v3 .
ex:d ex:q ex:v4 .
"#;

#[test]
fn set_joins_keep_bag_semantics() {
    let st = store_with(SHARED);
    assert_eq!(
        select(&st, "SELECT ?s ?o ?v WHERE { ?s ex:p ?o OPTIONAL { ?s ex:q ?v } }"),
        rows(&[
            &["<http://ex/a>", "<http://ex/o1>", "<http://ex/v1>"],
            &["<http://ex/a>", "<http://ex/o1>", "<http://ex/v2>"],
            &["<http://ex/a>", "<http://ex/o2>", "<http://ex/v1>"],
            &["<http://ex/a>", "<http://ex/o2>", "<http://ex/v2>"],
            &["<http://ex/b>", "<http://ex/o1>", "-"],
            &["<http://ex/b>", "<http://ex/o3>", "-"],
            &["<http://ex/c>", "<http://ex/o4>", "<http://ex/v3>"],
        ])
    );
    let left = |s: &str, o: &str| vec![Some(format!("<http://ex/{s}>")), Some(format!("<http://ex/{o}>"))];
    let matched = {
        let mut v = vec![left("a", "o1"), left("a", "o2"), left("c", "o4")];
        v.sort();
        v
    };
    let unmatched = vec![left("b", "o1"), left("b", "o3")];
    assert_eq!(select(&st, "SELECT ?s ?o WHERE { ?s ex:p ?o FILTER EXISTS { ?s ex:q ?v } }"), matched, "each left row once");
    assert_eq!(select(&st, "SELECT ?s ?o WHERE { ?s ex:p ?o FILTER NOT EXISTS { ?s ex:q ?v } }"), unmatched);
    assert_eq!(select(&st, "SELECT ?s ?o WHERE { ?s ex:p ?o MINUS { ?s ex:q ?v } }"), unmatched);
    assert_eq!(select(&st, "SELECT ?s ?o WHERE { ?s ex:p ?o FILTER NOT EXISTS { ?s ex:q ?v . ?v ex:q ?w } }").len(), 5);
    assert_eq!(select_ordered(&st, "SELECT ?s WHERE { ?s ex:p ?o OPTIONAL { ?s ex:q ?v } } LIMIT 3").len(), 3);
    assert!(ask(&st, "ASK { ?s ex:p ?o FILTER NOT EXISTS { ?s ex:q ?v } }"));
}

#[test]
fn set_joins_reset_between_evaluations() {
    let mut st = store_with(SHARED);
    load_ttl(&mut st, "ex:a ex:p ex:o1 . ex:b ex:p ex:o1 ; ex:q ex:v9 .", GraphRef::Named("<http://ex/g1>"));
    load_ttl(&mut st, "ex:a ex:p ex:o1 ; ex:q ex:v8 . ex:b ex:p ex:o1 .", GraphRef::Named("<http://ex/g2>"));
    assert_eq!(
        select(&st, "SELECT ?g ?s ?v WHERE { GRAPH ?g { ?s ex:p ?o OPTIONAL { ?s ex:q ?v } } }"),
        rows(&[
            &["<http://ex/g1>", "<http://ex/a>", "-"],
            &["<http://ex/g1>", "<http://ex/b>", "<http://ex/v9>"],
            &["<http://ex/g2>", "<http://ex/a>", "<http://ex/v8>"],
            &["<http://ex/g2>", "<http://ex/b>", "-"],
        ]),
        "each graph gets its own id set"
    );
    assert_eq!(
        select(&st, "SELECT ?g ?s WHERE { GRAPH ?g { ?s ex:p ?o MINUS { ?s ex:q ?v } } }"),
        rows(&[&["<http://ex/g1>", "<http://ex/a>"], &["<http://ex/g2>", "<http://ex/b>"]])
    );
    // Re-evaluated once per outer row, with ?o substituted.
    load_ttl(&mut st, "ex:r ex:t ex:o1, ex:o4 .", GraphRef::Default);
    assert_eq!(
        select(&st, "SELECT ?x ?s ?v WHERE { ex:r ex:t ?x { ?s ex:p ?x OPTIONAL { ?s ex:q ?v } } }"),
        rows(&[
            &["<http://ex/o1>", "<http://ex/a>", "<http://ex/v1>"],
            &["<http://ex/o1>", "<http://ex/a>", "<http://ex/v2>"],
            &["<http://ex/o1>", "<http://ex/b>", "-"],
            &["<http://ex/o4>", "<http://ex/c>", "<http://ex/v3>"],
        ])
    );
}

#[test]
fn ask_queries() {
    let st = people();
    assert!(ask(&st, "ASK { ex:alice ex:knows ex:bob }"));
    assert!(!ask(&st, "ASK { ex:bob ex:knows ex:alice }"));
    assert!(ask(&st, "ASK { }"));
}

#[test]
fn construct_and_describe() {
    let st = people();
    assert_eq!(
        construct(&st, "CONSTRUCT { ?y ex:knownBy ?x } WHERE { ?x ex:knows ?y FILTER(?x = ex:alice) }"),
        vec!["<http://ex/bob> <http://ex/knownBy> <http://ex/alice>", "<http://ex/carol> <http://ex/knownBy> <http://ex/alice>"]
    );
    // Unbound or invalid template triples are skipped; duplicates collapse.
    assert_eq!(construct(&st, "CONSTRUCT { ?s ex:mail ?e } WHERE { ?s ex:name ?n OPTIONAL { ?s ex:email ?e } }"), vec!["<http://ex/bob> <http://ex/mail> \"bob@ex\""]);
    let blanks = construct(&st, "CONSTRUCT { [] ex:about ?s } WHERE { ?s ex:age ?a }");
    assert_eq!(blanks.len(), 3, "a fresh blank node per solution");
    assert_eq!(construct(&st, "DESCRIBE ex:carol").len(), 2);
}

#[test]
fn property_graph_data_is_queryable_but_kept_out_of_rdf_answers() {
    use s_store::property::Owner;
    let mut st = people();
    st.properties_mut().set(Owner::Edge("<http://ex/alice>", "<http://ex/knows>", "<http://ex/bob>"), "<http://ex/since>", "\"2020\"").unwrap();
    load_ttl(&mut st, "ex:x ex:p ex:y .", GraphRef::Named("<http://ex/g1>"));
    assert_eq!(select(&st, "SELECT ?g WHERE { GRAPH ?g { } }"), rows(&[&["<http://ex/g1>"]]));
    assert_eq!(
        select(&st, "SELECT ?since WHERE { GRAPH <urn:x-s-store:properties> { ?edge ex:since ?since } }"),
        rows(&[&["\"2020\""]])
    );
}
