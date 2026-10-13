//! SPARQL expressions, solution modifiers and aggregates.

mod common;
use common::*;

const DATA: &str = r#"
ex:a ex:v 1 ; ex:s "apple" ; ex:l "chat"@fr ; ex:d 1.5 ; ex:b true ; ex:t "2020-03-04T05:06:07Z"^^xsd:dateTime .
ex:b ex:v 2 ; ex:s "Banana" ; ex:l "cat"@en ; ex:d 2.25 ; ex:b false .
ex:c ex:v 3 ; ex:s "cherry" ; ex:l "Katze"@de-DE ; ex:d 0.5 .
ex:d ex:v "x" .
ex:e ex:v 1.0e0 .
"#;

fn data() -> s_store::Store {
    store_with(DATA)
}

fn one(st: &s_store::Store, expr: &str) -> Option<String> {
    let rows = select(st, &format!("SELECT ?r WHERE {{ BIND({expr} AS ?r) }}"));
    assert_eq!(rows.len(), 1, "{expr}");
    rows[0][0].clone()
}

fn str_lit(s: &str) -> String {
    format!("\"{s}\"")
}

#[test]
fn arithmetic_and_numeric_promotion() {
    let st = data();
    assert_eq!(one(&st, "1 + 2").as_deref(), Some(int(3).as_str()));
    assert_eq!(one(&st, "7 - 10").as_deref(), Some(int(-3).as_str()));
    assert_eq!(one(&st, "6 * 7").as_deref(), Some(int(42).as_str()));
    assert_eq!(one(&st, "1 / 2").as_deref(), Some(dec("0.5").as_str()), "integer division yields decimal");
    assert_eq!(one(&st, "1.5 + 1").as_deref(), Some(dec("2.5").as_str()));
    assert_eq!(one(&st, "-(3)").as_deref(), Some(int(-3).as_str()));
    assert_eq!(one(&st, "1 / 0"), None, "division by zero is an error: unbound");
    assert_eq!(one(&st, "\"a\" + 1"), None);
    assert_eq!(one(&st, "2.0e0 * 2").as_deref(), Some("\"4.0E0\"^^<http://www.w3.org/2001/XMLSchema#double>"));
}

#[test]
fn comparisons_and_logic() {
    let st = data();
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:v ?v FILTER(?v >= 2) }"), rows(&[&["<http://ex/b>"], &["<http://ex/c>"]]));
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:v ?v FILTER(?v = 1) }"), rows(&[&["<http://ex/a>"], &["<http://ex/e>"]]), "numeric value equality across types");
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:v ?v FILTER(sameTerm(?v, 1)) }"), rows(&[&["<http://ex/a>"]]));
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:s ?s FILTER(?s < \"b\") }"), rows(&[&["<http://ex/a>"], &["<http://ex/b>"]]));
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:v ?v FILTER(?v > 1 && ?v < 3) }"), rows(&[&["<http://ex/b>"]]));
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:v ?v FILTER(?v = 1 || ?v = 3) }"), rows(&[&["<http://ex/a>"], &["<http://ex/c>"], &["<http://ex/e>"]]));
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:v ?v FILTER(!(?v > 1)) }"), rows(&[&["<http://ex/a>"], &["<http://ex/e>"]]), "errors filter out");
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:v ?v FILTER(?v IN (2, 3, \"x\")) }"), rows(&[&["<http://ex/b>"], &["<http://ex/c>"], &["<http://ex/d>"]]));
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:b ?b FILTER(?b) }"), rows(&[&["<http://ex/a>"]]));
    // Error || true is true; error && false is false.
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:v ?v FILTER(?v > 2 || ?v = \"x\") }"), rows(&[&["<http://ex/c>"], &["<http://ex/d>"]]));
}

#[test]
fn effective_boolean_values() {
    let st = data();
    assert_eq!(one(&st, "IF(\"\", 1, 2)").as_deref(), Some(int(2).as_str()));
    assert_eq!(one(&st, "IF(\"x\", 1, 2)").as_deref(), Some(int(1).as_str()));
    assert_eq!(one(&st, "IF(0.0, 1, 2)").as_deref(), Some(int(2).as_str()));
    assert_eq!(one(&st, "IF(ex:a, 1, 2)"), None, "IRIs have no EBV");
}

#[test]
fn term_functions() {
    let st = data();
    assert_eq!(one(&st, "STR(ex:a)").as_deref(), Some("\"http://ex/a\""));
    assert_eq!(one(&st, "LANG(\"x\"@en)").as_deref(), Some("\"en\""));
    assert_eq!(one(&st, "DATATYPE(1)").as_deref(), Some("<http://www.w3.org/2001/XMLSchema#integer>"));
    assert_eq!(one(&st, "DATATYPE(\"x\"@en)").as_deref(), Some("<http://www.w3.org/1999/02/22-rdf-syntax-ns#langString>"));
    assert_eq!(one(&st, "IRI(\"http://x/y\")").as_deref(), Some("<http://x/y>"));
    assert_eq!(one(&st, "STRDT(\"5\", xsd:integer)").as_deref(), Some(int(5).as_str()));
    assert_eq!(one(&st, "STRLANG(\"x\", \"EN\")").as_deref(), Some("\"x\"@en"));
    assert_eq!(one(&st, "isIRI(ex:a)").as_deref(), Some(boolean(true).as_str()));
    assert_eq!(one(&st, "isLiteral(1)").as_deref(), Some(boolean(true).as_str()));
    assert_eq!(one(&st, "isNumeric(\"1\")").as_deref(), Some(boolean(false).as_str()));
    assert_eq!(one(&st, "COALESCE(?unbound, 1/0, 7)").as_deref(), Some(int(7).as_str()));
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:l ?l FILTER(langMatches(lang(?l), \"de\")) }"), rows(&[&["<http://ex/c>"]]));
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:l ?l FILTER(langMatches(lang(?l), \"*\")) }").len(), 3);
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:v ?v FILTER(isNumeric(?v)) }").len(), 4);
}

#[test]
fn string_functions() {
    let st = data();
    assert_eq!(one(&st, "STRLEN(\"chat\")").as_deref(), Some(int(4).as_str()));
    assert_eq!(one(&st, "UCASE(\"chat\"@fr)").as_deref(), Some("\"CHAT\"@fr"));
    assert_eq!(one(&st, "LCASE(\"ABC\")").as_deref(), Some(str_lit("abc").as_str()));
    assert_eq!(one(&st, "SUBSTR(\"foobar\", 4)").as_deref(), Some(str_lit("bar").as_str()));
    assert_eq!(one(&st, "SUBSTR(\"foobar\", 2, 3)").as_deref(), Some(str_lit("oob").as_str()));
    assert_eq!(one(&st, "CONCAT(\"a\", \"b\", \"c\")").as_deref(), Some(str_lit("abc").as_str()));
    assert_eq!(one(&st, "CONCAT(\"a\"@en, \"b\"@en)").as_deref(), Some("\"ab\"@en"));
    assert_eq!(one(&st, "CONCAT(\"a\"@en, \"b\")").as_deref(), Some(str_lit("ab").as_str()));
    assert_eq!(one(&st, "CONTAINS(\"foobar\", \"oba\")").as_deref(), Some(boolean(true).as_str()));
    assert_eq!(one(&st, "STRSTARTS(\"foobar\", \"foo\")").as_deref(), Some(boolean(true).as_str()));
    assert_eq!(one(&st, "STRENDS(\"foobar\", \"foo\")").as_deref(), Some(boolean(false).as_str()));
    assert_eq!(one(&st, "STRBEFORE(\"abc\", \"b\")").as_deref(), Some(str_lit("a").as_str()));
    assert_eq!(one(&st, "STRAFTER(\"abc\", \"b\")").as_deref(), Some(str_lit("c").as_str()));
    assert_eq!(one(&st, "STRAFTER(\"abc\", \"z\")").as_deref(), Some(str_lit("").as_str()));
    assert_eq!(one(&st, "ENCODE_FOR_URI(\"a b/c\")").as_deref(), Some(str_lit("a%20b%2Fc").as_str()));
    assert_eq!(one(&st, "REPLACE(\"abcabc\", \"b\", \"X\")").as_deref(), Some(str_lit("aXcaXc").as_str()));
    assert_eq!(one(&st, "STR(\"say \\\"hi\\\"\")").as_deref(), Some("\"say \\\"hi\\\"\""));
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:s ?s FILTER(REGEX(?s, \"^b\", \"i\")) }"), rows(&[&["<http://ex/b>"]]));
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ex:s ?s FILTER(REGEX(?s, \"rr\")) }"), rows(&[&["<http://ex/c>"]]));
}

#[test]
fn numeric_functions_and_casts() {
    let st = data();
    assert_eq!(one(&st, "ABS(-2)").as_deref(), Some(int(2).as_str()));
    assert_eq!(one(&st, "ROUND(2.5)").as_deref(), Some(dec("3.0").as_str()));
    assert_eq!(one(&st, "CEIL(1.2)").as_deref(), Some(dec("2.0").as_str()));
    assert_eq!(one(&st, "FLOOR(-1.2)").as_deref(), Some(dec("-2.0").as_str()));
    assert_eq!(one(&st, "xsd:integer(\"42\")").as_deref(), Some(int(42).as_str()));
    assert_eq!(one(&st, "xsd:integer(\"4.2\")"), None);
    assert_eq!(one(&st, "xsd:decimal(1)").as_deref(), Some(dec("1.0").as_str()));
    assert_eq!(one(&st, "xsd:string(12)").as_deref(), Some(str_lit("12").as_str()));
    assert_eq!(one(&st, "xsd:boolean(\"1\")").as_deref(), Some(boolean(true).as_str()));
    assert_eq!(one(&st, "xsd:double(\"1\")").as_deref(), Some("\"1.0E0\"^^<http://www.w3.org/2001/XMLSchema#double>"));
}

#[test]
fn date_time_functions() {
    let st = data();
    let r = select(&st, "SELECT (YEAR(?t) AS ?y) (MONTH(?t) AS ?m) (DAY(?t) AS ?d) (HOURS(?t) AS ?h) (MINUTES(?t) AS ?mi) (SECONDS(?t) AS ?s) (TZ(?t) AS ?tz) WHERE { ex:a ex:t ?t }");
    assert_eq!(r, rows(&[&[&int(2020), &int(3), &int(4), &int(5), &int(6), &dec("7.0"), "\"Z\""]]));
    assert_eq!(one(&st, "\"2020-01-01T00:00:00Z\"^^xsd:dateTime < \"2020-01-01T00:00:01Z\"^^xsd:dateTime").as_deref(), Some(boolean(true).as_str()));
}

#[test]
fn order_by_limit_offset() {
    let st = data();
    assert_eq!(
        select_ordered(&st, "SELECT ?x WHERE { ?x ex:d ?d } ORDER BY ?d"),
        ordered_rows(&[&["<http://ex/c>"], &["<http://ex/a>"], &["<http://ex/b>"]])
    );
    assert_eq!(
        select_ordered(&st, "SELECT ?x WHERE { ?x ex:d ?d } ORDER BY DESC(?d) LIMIT 2"),
        ordered_rows(&[&["<http://ex/b>"], &["<http://ex/a>"]])
    );
    assert_eq!(select_ordered(&st, "SELECT ?x WHERE { ?x ex:d ?d } ORDER BY ?d OFFSET 1 LIMIT 1"), ordered_rows(&[&["<http://ex/a>"]]));
    // Unbound sorts first; IRIs before literals.
    assert_eq!(
        select_ordered(&st, "SELECT ?x ?b WHERE { ?x ex:v ?v OPTIONAL { ?x ex:b ?b } FILTER(?x != ex:e && ?x != ex:d) } ORDER BY ?b ?x"),
        ordered_rows(&[&["<http://ex/c>", "-"], &["<http://ex/b>", &boolean(false)], &["<http://ex/a>", &boolean(true)]])
    );
    assert_eq!(select(&st, "SELECT ?x WHERE { ?x ?p ?o } LIMIT 0"), rows(&[]));
}

#[test]
fn distinct_and_reduced() {
    let st = data();
    assert_eq!(select(&st, "SELECT DISTINCT ?p WHERE { ?x ?p ?o }").len(), 6);
    assert_eq!(select(&st, "SELECT ?p WHERE { ?x ?p ?o }").len(), 17);
    assert!(select(&st, "SELECT REDUCED ?p WHERE { ?x ?p ?o }").len() >= 6);
}

#[test]
fn projection_expressions() {
    let st = data();
    assert_eq!(select(&st, "SELECT (?v * 10 AS ?w) WHERE { ex:b ex:v ?v }"), rows(&[&[&int(20)]]));
}

#[test]
fn aggregates() {
    let st = data();
    assert_eq!(select(&st, "SELECT (COUNT(*) AS ?n) WHERE { ?x ex:v ?v }"), rows(&[&[&int(5)]]));
    assert_eq!(select(&st, "SELECT (COUNT(DISTINCT ?p) AS ?n) WHERE { ?x ?p ?o }"), rows(&[&[&int(6)]]));
    assert_eq!(select(&st, "SELECT (SUM(?d) AS ?s) WHERE { ?x ex:d ?d }"), rows(&[&[&dec("4.25")]]));
    assert_eq!(select(&st, "SELECT (AVG(?v) AS ?a) WHERE { ?x ex:v ?v FILTER(isNumeric(?v) && ?x != ex:e) }"), rows(&[&[&dec("2.0")]]));
    assert_eq!(select(&st, "SELECT (MIN(?d) AS ?lo) (MAX(?d) AS ?hi) WHERE { ?x ex:d ?d }"), rows(&[&[&dec("0.5"), &dec("2.25")]]));
    assert_eq!(select(&st, "SELECT (GROUP_CONCAT(?s; SEPARATOR=\",\") AS ?c) WHERE { ?x ex:s ?s FILTER(?x != ex:c) }").len(), 1);
    assert_eq!(select(&st, "SELECT (SAMPLE(?v) AS ?s) WHERE { ex:a ex:v ?v }"), rows(&[&[&int(1)]]));
    // An aggregate over no rows still yields one group.
    assert_eq!(select(&st, "SELECT (COUNT(*) AS ?n) WHERE { ?x ex:none ?v }"), rows(&[&[&int(0)]]));
    // SUM over an error yields an unbound result.
    assert_eq!(select(&st, "SELECT (SUM(?v) AS ?s) WHERE { ?x ex:v ?v }"), rows(&[&["-"]]));
}

#[test]
fn group_by_and_having() {
    let st = store_with(
        r#"
ex:o1 ex:customer ex:ann ; ex:total 10 .
ex:o2 ex:customer ex:ann ; ex:total 5 .
ex:o3 ex:customer ex:ben ; ex:total 7 .
ex:o4 ex:customer ex:cat .
"#,
    );
    assert_eq!(
        select(&st, "SELECT ?c (SUM(?t) AS ?sum) (COUNT(?t) AS ?n) WHERE { ?o ex:customer ?c OPTIONAL { ?o ex:total ?t } } GROUP BY ?c"),
        rows(&[&["<http://ex/ann>", &int(15), &int(2)], &["<http://ex/ben>", &int(7), &int(1)], &["<http://ex/cat>", &int(0), &int(0)]])
    );
    assert_eq!(
        select(&st, "SELECT ?c WHERE { ?o ex:customer ?c ; ex:total ?t } GROUP BY ?c HAVING (SUM(?t) > 8)"),
        rows(&[&["<http://ex/ann>"]])
    );
    assert_eq!(
        select_ordered(&st, "SELECT ?c (COUNT(*) AS ?n) WHERE { ?o ex:customer ?c } GROUP BY ?c ORDER BY DESC(?n) ?c LIMIT 2"),
        ordered_rows(&[&["<http://ex/ann>", &int(2)], &["<http://ex/ben>", &int(1)]])
    );
    assert_eq!(
        select(&st, "SELECT ?k (COUNT(*) AS ?n) WHERE { ?o ex:total ?t } GROUP BY (?t > 6 AS ?k)"),
        rows(&[&[&boolean(false), &int(1)], &[&boolean(true), &int(2)]])
    );
}
