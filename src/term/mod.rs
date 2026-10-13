//! RDF terms as dictionary keys.
//!
//! The dictionary key of a term is its canonical N-Triples serialization
//! (RDF 1.2 canonical form): `<iri>`, `_:label`, `"lexical"`,
//! `"lexical"@lang`, `"lexical"^^<datatype>`, and `<<( s p o )>>` for triple
//! terms. One encoding serves every purpose:
//!
//! - N-Triples input that is already canonical (the common case: no escapes,
//!   no control characters) is interned straight from the parser's borrowed
//!   bytes, with no copy ([`encode::nt_fast_key`]).
//! - A key decodes back to a [`TermRef`] by slicing, with no allocation
//!   ([`TermRef::parse`]), and serializes to N-Triples as itself.
//! - The dictionary's lexical order clusters IRIs (`<`), literals (`"`) and
//!   blank nodes (`_`) into separate key ranges.
//!
//! Canonicalization (escapes, `xsd:string`, lowercase language tags) happens
//! once, at the boundary where terms enter the store or a query.

pub mod encode;
pub mod iri;

pub use encode::{BlankScope, KeyBuf, nt_fast_key};

pub const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
pub const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
pub const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
pub const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
pub const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
pub const XSD_FLOAT: &str = "http://www.w3.org/2001/XMLSchema#float";
pub const XSD_DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";
pub const XSD_DATE_TIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
pub const RDF_LANG_STRING: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// A borrowed view of a canonical term key. Literal lexical forms stay in
/// their canonical *escaped* form; use [`encode::unescape_lexical`] when the
/// value itself is needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermRef<'a> {
    Iri(&'a str),
    Blank(&'a str),
    Literal { lexical: &'a str, lang: Option<&'a str>, datatype: Option<&'a str> },
    /// An RDF 1.2 triple term; the field is the text between `<<(` and `)>>`,
    /// three keys separated by single spaces (see [`TermRef::triple_parts`]).
    Triple(&'a str),
}

impl<'a> TermRef<'a> {
    /// Slices a canonical key into its parts. `None` if `key` is not a key
    /// this module produces.
    pub fn parse(key: &'a str) -> Option<Self> {
        let bytes = key.as_bytes();
        match bytes.first()? {
            b'<' if key.starts_with("<<(") => key.strip_prefix("<<( ")?.strip_suffix(" )>>").map(TermRef::Triple),
            b'<' => key.strip_prefix('<')?.strip_suffix('>').map(TermRef::Iri),
            b'_' => key.strip_prefix("_:").map(TermRef::Blank),
            b'"' => {
                // Neither a language tag nor a datatype IRIREF can contain
                // `"`, so the closing quote is the last one in the key.
                let close = key.rfind('"').filter(|&i| i > 0)?;
                let lexical = &key[1..close];
                let suffix = &key[close + 1..];
                if suffix.is_empty() {
                    Some(TermRef::Literal { lexical, lang: None, datatype: None })
                } else if let Some(lang) = suffix.strip_prefix('@') {
                    Some(TermRef::Literal { lexical, lang: Some(lang), datatype: None })
                } else {
                    let dt = suffix.strip_prefix("^^<")?.strip_suffix('>')?;
                    Some(TermRef::Literal { lexical, lang: None, datatype: Some(dt) })
                }
            }
            _ => None,
        }
    }

    /// Splits a triple term's inner text into its subject, predicate and
    /// object keys.
    pub fn triple_parts(inner: &'a str) -> Option<[&'a str; 3]> {
        let (s, rest) = split_key(inner)?;
        let (p, rest) = split_key(rest.strip_prefix(' ')?)?;
        let (o, rest) = split_key(rest.strip_prefix(' ')?)?;
        rest.is_empty().then_some([s, p, o])
    }

    pub fn is_literal(self) -> bool {
        matches!(self, TermRef::Literal { .. })
    }

    /// The literal's datatype IRI, with RDF 1.1's implicit ones made explicit
    /// (`xsd:string` for simple literals, `rdf:langString` for tagged ones).
    pub fn datatype(self) -> Option<&'a str> {
        match self {
            TermRef::Literal { datatype: Some(dt), .. } => Some(dt),
            TermRef::Literal { lang: Some(_), .. } => Some(RDF_LANG_STRING),
            TermRef::Literal { .. } => Some(XSD_STRING),
            _ => None,
        }
    }

    /// Appends this term's canonical key to `out`. The inverse of
    /// [`TermRef::parse`] (lexical forms are copied as-is: they are already
    /// canonical in a parsed key).
    pub fn write_key(self, out: &mut String) {
        match self {
            TermRef::Iri(iri) => {
                out.push('<');
                out.push_str(iri);
                out.push('>');
            }
            TermRef::Blank(label) => {
                out.push_str("_:");
                out.push_str(label);
            }
            TermRef::Literal { lexical, lang, datatype } => {
                out.push('"');
                out.push_str(lexical);
                out.push('"');
                if let Some(lang) = lang {
                    out.push('@');
                    out.push_str(lang);
                } else if let Some(dt) = datatype.filter(|&dt| dt != XSD_STRING) {
                    out.push_str("^^<");
                    out.push_str(dt);
                    out.push('>');
                }
            }
            TermRef::Triple(inner) => {
                out.push_str("<<( ");
                out.push_str(inner);
                out.push_str(" )>>");
            }
        }
    }
}

/// Splits the first key off `s` (keys inside a triple term are separated by
/// one space; literals may contain spaces, nested triple terms may too).
fn split_key(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let end = match bytes.first()? {
        b'<' if s.starts_with("<<(") => {
            let mut depth = 0usize;
            let mut i = 0;
            loop {
                if s[i..].starts_with("<<(") {
                    depth += 1;
                    i += 3;
                } else if s[i..].starts_with(")>>") {
                    depth -= 1;
                    i += 3;
                    if depth == 0 {
                        break i;
                    }
                } else if bytes[i] == b'"' {
                    i = literal_end(bytes, i)?;
                } else {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
            }
        }
        b'<' => s.find('>')? + 1,
        b'_' => s.find(' ').unwrap_or(s.len()),
        b'"' => literal_end(bytes, 0)?,
        _ => return None,
    };
    Some(s.split_at(end))
}

/// End (exclusive) of the literal key starting at `start`, suffix included.
fn literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    loop {
        match *bytes.get(i)? {
            b'\\' => i += 2,
            b'"' => break,
            _ => i += 1,
        }
    }
    i += 1;
    match bytes.get(i) {
        Some(b'@') => Some(i + bytes[i..].iter().position(|&b| b == b' ').unwrap_or(bytes.len() - i)),
        Some(b'^') => Some(i + bytes[i..].iter().position(|&b| b == b'>')? + 1),
        _ => Some(i),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_key_kind() {
        assert_eq!(TermRef::parse("<http://a/b>"), Some(TermRef::Iri("http://a/b")));
        assert_eq!(TermRef::parse("_:b1_x"), Some(TermRef::Blank("b1_x")));
        assert_eq!(TermRef::parse("\"hi\""), Some(TermRef::Literal { lexical: "hi", lang: None, datatype: None }));
        assert_eq!(TermRef::parse("\"hi\"@en-gb"), Some(TermRef::Literal { lexical: "hi", lang: Some("en-gb"), datatype: None }));
        assert_eq!(
            TermRef::parse("\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>"),
            Some(TermRef::Literal { lexical: "1", lang: None, datatype: Some(XSD_INTEGER) })
        );
        assert_eq!(TermRef::parse("<<( <a> <b> \"c d\" )>>"), Some(TermRef::Triple("<a> <b> \"c d\"")));
    }

    #[test]
    fn literal_lexical_may_contain_escaped_quotes_and_suffix_like_text() {
        let key = r#""say \"x\"@en""#;
        assert_eq!(TermRef::parse(key), Some(TermRef::Literal { lexical: r#"say \"x\"@en"#, lang: None, datatype: None }));
    }

    #[test]
    fn rejects_malformed_keys() {
        for key in ["", "x", "<a", "\"", "\"a\"^^x", "_x"] {
            assert_eq!(TermRef::parse(key), None, "{key:?}");
        }
    }

    #[test]
    fn write_key_round_trips() {
        for key in ["<http://a>", "_:b0_q", "\"a b\"", "\"a\"@en", "\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>", "<<( <a> <b> <c> )>>"] {
            let mut out = String::new();
            TermRef::parse(key).unwrap().write_key(&mut out);
            assert_eq!(out, key);
        }
    }

    #[test]
    fn xsd_string_datatype_is_written_as_a_simple_literal() {
        let mut out = String::new();
        TermRef::Literal { lexical: "a", lang: None, datatype: Some(XSD_STRING) }.write_key(&mut out);
        assert_eq!(out, "\"a\"");
    }

    #[test]
    fn implicit_datatypes() {
        assert_eq!(TermRef::parse("\"a\"").unwrap().datatype(), Some(XSD_STRING));
        assert_eq!(TermRef::parse("\"a\"@en").unwrap().datatype(), Some(RDF_LANG_STRING));
        assert_eq!(TermRef::parse("<a>").unwrap().datatype(), None);
    }

    #[test]
    fn splits_triple_parts_including_nested_and_spaced_literals() {
        assert_eq!(TermRef::triple_parts("<a> <b> \"c d\"@en"), Some(["<a>", "<b>", "\"c d\"@en"]));
        assert_eq!(TermRef::triple_parts("_:x <b> \"q\\\" )>>\""), Some(["_:x", "<b>", "\"q\\\" )>>\""]));
        assert_eq!(
            TermRef::triple_parts("<<( <a> <b> <c> )>> <p> \"1\"^^<http://t>"),
            Some(["<<( <a> <b> <c> )>>", "<p>", "\"1\"^^<http://t>"])
        );
        assert_eq!(TermRef::triple_parts("<a> <b>"), None);
    }
}
