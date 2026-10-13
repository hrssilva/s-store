//! Canonical key encoding from `s-parser`'s borrowed terms.
//!
//! Every syntax reaches the same key for the same RDF term: escapes are
//! decoded and re-escaped canonically (RDF 1.2 canonical N-Triples: `\t \b
//! \n \r \f \" \\` as ECHARs, other C0 controls and DEL as `\u00XX`, all else
//! raw UTF-8), `xsd:string` literals become simple literals, language tags
//! are lowercased, IRIs have UCHARs and `PN_LOCAL` escapes decoded and are
//! resolved against a base when relative.
//!
//! [`KeyBuf`] reuses its buffers across calls, so encoding allocates nothing
//! after warm-up; [`nt_fast_key`] skips encoding entirely for N-Triples terms
//! that are already canonical.

use super::iri;
use super::XSD_STRING;
use s_parser::{BlankId, ResolvedIri, ResolvedSuffix, ResolvedTerm, TermType};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    InvalidUtf8,
    InvalidEscape,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::InvalidUtf8 => f.write_str("term is not valid UTF-8"),
            EncodeError::InvalidEscape => f.write_str("invalid escape sequence in term"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Blank node labels are document-scoped: the same label in two loads is two
/// nodes. A load's scope number is prefixed to its labels: `_:b{scope}_{label}`
/// for a written label, `_:b{scope}-{n}` for a parser-generated one. The
/// scope's digits end at the first `_`/`-`, and a written label can't start
/// with `-`, so distinct (scope, label) pairs always give distinct keys, and
/// every key is still a valid N-Triples blank node label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlankScope(pub u64);

/// Returns `term`'s own source text when it already is its canonical key:
/// an IRI without escapes, or a literal without escapes or control
/// characters, an uppercase language tag, or an explicit `xsd:string`.
/// Blank nodes always need scoping (`None`).
pub fn nt_fast_key(term: s_parser::Term<'_>) -> Option<&str> {
    let canonical = match term.kind() {
        TermType::Iri => !term.value().contains(&b'\\'),
        TermType::Literal => clean_lexical(term.value()),
        TermType::LanguageLiteral => clean_lexical(term.value()) && !term.suffix().iter().any(u8::is_ascii_uppercase),
        TermType::TypedLiteral => {
            let suffix = term.suffix();
            clean_lexical(term.value()) && !suffix.contains(&b'\\') && &suffix[3..suffix.len() - 1] != XSD_STRING.as_bytes()
        }
        TermType::BlankNode | TermType::DefaultGraph => false,
    };
    if canonical { std::str::from_utf8(term.text()).ok() } else { None }
}

fn clean_lexical(value: &[u8]) -> bool {
    !value.iter().any(|&b| b == b'\\' || b < 0x20 || b == 0x7f)
}

/// Reusable output and scratch buffers for canonical keys.
#[derive(Default)]
pub struct KeyBuf {
    key: Vec<u8>,
    iri: Vec<u8>,
    resolved: String,
}

impl KeyBuf {
    pub fn new() -> Self {
        Self::default()
    }

    /// The key most recently produced.
    pub fn as_str(&self) -> &str {
        // Every producer validates before returning, and failed ones clear.
        std::str::from_utf8(&self.key).unwrap_or("")
    }

    pub fn resolved(&mut self, term: ResolvedTerm<'_>, scope: BlankScope, base: Option<&str>) -> Result<&str, EncodeError> {
        match term {
            ResolvedTerm::Iri(i) => self.iri(i, base),
            ResolvedTerm::BlankNode(b) => self.blank(scope, b),
            ResolvedTerm::Literal { lexical, suffix } => self.literal(lexical, suffix, base),
        }
    }

    pub fn iri(&mut self, iri: ResolvedIri<'_>, base: Option<&str>) -> Result<&str, EncodeError> {
        self.key.clear();
        let r = push_iri(&mut self.key, &mut self.iri, &mut self.resolved, iri, base);
        self.finish(r)
    }

    /// `raw_lexical` is the lexical form as written (escapes retained, any
    /// quoting style).
    pub fn literal(&mut self, raw_lexical: &[u8], suffix: ResolvedSuffix<'_>, base: Option<&str>) -> Result<&str, EncodeError> {
        self.key.clear();
        self.key.push(b'"');
        let mut r = push_lexical(&mut self.key, raw_lexical);
        self.key.push(b'"');
        if r.is_ok() {
            match suffix {
                ResolvedSuffix::None => {}
                ResolvedSuffix::Lang(tag) => {
                    self.key.push(b'@');
                    self.key.extend(tag.iter().map(u8::to_ascii_lowercase));
                }
                ResolvedSuffix::Datatype(dt) => {
                    let mark = self.key.len();
                    self.key.extend_from_slice(b"^^");
                    r = push_iri(&mut self.key, &mut self.iri, &mut self.resolved, dt, base);
                    if self.key[mark + 3..self.key.len() - 1] == *XSD_STRING.as_bytes() {
                        self.key.truncate(mark);
                    }
                }
            }
        }
        self.finish(r)
    }

    /// A literal from an already-unescaped value (computed in a query).
    pub fn literal_value(&mut self, value: &str, lang: Option<&str>, datatype: Option<&str>) -> &str {
        self.key.clear();
        self.key.push(b'"');
        for c in value.chars() {
            push_escaped_char(&mut self.key, c);
        }
        self.key.push(b'"');
        if let Some(lang) = lang {
            self.key.push(b'@');
            self.key.extend(lang.bytes().map(|b| b.to_ascii_lowercase()));
        } else if let Some(dt) = datatype.filter(|&dt| dt != XSD_STRING) {
            self.key.extend_from_slice(b"^^<");
            self.key.extend_from_slice(dt.as_bytes());
            self.key.push(b'>');
        }
        self.as_str()
    }

    pub fn blank(&mut self, scope: BlankScope, id: BlankId<'_>) -> Result<&str, EncodeError> {
        self.key.clear();
        let r = write_blank(&mut self.key, scope, id);
        self.finish(r)
    }

    /// The key of a blank node with this (already scoped) label.
    pub fn blank_label(&mut self, label: &str) -> &str {
        self.key.clear();
        self.key.extend_from_slice(b"_:");
        self.key.extend_from_slice(label.as_bytes());
        self.as_str()
    }

    /// Copies an existing key.
    pub fn raw(&mut self, key: &str) -> &str {
        self.key.clear();
        self.key.extend_from_slice(key.as_bytes());
        self.as_str()
    }

    /// The key of an RDF 1.2 triple term over three existing keys.
    pub fn triple(&mut self, s: &str, p: &str, o: &str) -> &str {
        self.key.clear();
        for part in ["<<( ", s, " ", p, " ", o, " )>>"] {
            self.key.extend_from_slice(part.as_bytes());
        }
        self.as_str()
    }

    fn finish(&mut self, r: Result<(), EncodeError>) -> Result<&str, EncodeError> {
        if let Err(e) = r {
            self.key.clear();
            return Err(e);
        }
        std::str::from_utf8(&self.key).map_err(|_| EncodeError::InvalidUtf8)
    }
}

fn write_blank(out: &mut Vec<u8>, scope: BlankScope, id: BlankId<'_>) -> Result<(), EncodeError> {
    use std::io::Write;
    match id {
        BlankId::Named(label) => {
            write!(out, "_:b{}_", scope.0).expect("Vec write");
            out.extend_from_slice(label);
        }
        BlankId::Fresh(n) => write!(out, "_:b{}-{n}", scope.0).expect("Vec write"),
    }
    Ok(())
}

/// Appends `<iri>` with escapes decoded and, if relative, resolved.
fn push_iri(out: &mut Vec<u8>, scratch: &mut Vec<u8>, resolved: &mut String, iri: ResolvedIri<'_>, base: Option<&str>) -> Result<(), EncodeError> {
    scratch.clear();
    let (first, local) = iri.parts();
    push_uchar_decoded(scratch, first)?;
    if !local.is_empty() {
        push_pn_local(scratch, local);
    }
    let text = std::str::from_utf8(scratch).map_err(|_| EncodeError::InvalidUtf8)?;
    let text = match base {
        Some(base) if !iri::has_scheme(text) => {
            resolved.clear();
            iri::resolve_into(base, text, resolved);
            resolved.as_str()
        }
        _ => text,
    };
    out.push(b'<');
    for c in text.chars() {
        if matches!(c, '\0'..=' ' | '<' | '>' | '"' | '{' | '}' | '|' | '^' | '`' | '\\') {
            push_uchar(out, c);
        } else {
            let mut b = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        }
    }
    out.push(b'>');
    Ok(())
}

fn push_uchar_decoded(out: &mut Vec<u8>, raw: &[u8]) -> Result<(), EncodeError> {
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\\' {
            let (c, len) = decode_escape(&raw[i..], false)?;
            let mut b = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            i += len;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    Ok(())
}

/// `PN_LOCAL_ESC`: a backslash before one of these characters is dropped.
fn push_pn_local(out: &mut Vec<u8>, local: &[u8]) {
    const ESCAPABLE: &[u8] = b"_~.-!$&'()*+,;=/?#@%";
    let mut i = 0;
    while i < local.len() {
        if local[i] == b'\\' && local.get(i + 1).is_some_and(|b| ESCAPABLE.contains(b)) {
            i += 1;
        }
        out.push(local[i]);
        i += 1;
    }
}

/// Decodes the escape at the start of `s` (which begins with `\`), returning
/// the character and the escape's length. `echar` allows `\t`-style escapes.
fn decode_escape(s: &[u8], echar: bool) -> Result<(char, usize), EncodeError> {
    let hex = |digits: &[u8]| -> Result<char, EncodeError> {
        let text = std::str::from_utf8(digits).map_err(|_| EncodeError::InvalidEscape)?;
        let v = u32::from_str_radix(text, 16).map_err(|_| EncodeError::InvalidEscape)?;
        char::from_u32(v).ok_or(EncodeError::InvalidEscape)
    };
    match s.get(1) {
        Some(b'u') if s.len() >= 6 => Ok((hex(&s[2..6])?, 6)),
        Some(b'U') if s.len() >= 10 => Ok((hex(&s[2..10])?, 10)),
        Some(&b) if echar => {
            let c = match b {
                b't' => '\t',
                b'b' => '\u{8}',
                b'n' => '\n',
                b'r' => '\r',
                b'f' => '\u{c}',
                b'"' => '"',
                b'\'' => '\'',
                b'\\' => '\\',
                _ => return Err(EncodeError::InvalidEscape),
            };
            Ok((c, 2))
        }
        _ => Err(EncodeError::InvalidEscape),
    }
}

/// Decodes a written lexical form and appends it canonically escaped.
fn push_lexical(out: &mut Vec<u8>, raw: &[u8]) -> Result<(), EncodeError> {
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        if b == b'\\' {
            let (c, len) = decode_escape(&raw[i..], true)?;
            push_escaped_char(out, c);
            i += len;
        } else if b < 0x80 {
            push_escaped_char(out, b as char);
            i += 1;
        } else {
            // Multi-byte UTF-8 passes through; validated once at the end.
            out.push(b);
            i += 1;
        }
    }
    Ok(())
}

fn push_escaped_char(out: &mut Vec<u8>, c: char) {
    let esc: &[u8] = match c {
        '\t' => b"\\t",
        '\u{8}' => b"\\b",
        '\n' => b"\\n",
        '\r' => b"\\r",
        '\u{c}' => b"\\f",
        '"' => b"\\\"",
        '\\' => b"\\\\",
        '\0'..='\u{1f}' | '\u{7f}' => return push_uchar(out, c),
        _ => {
            let mut b = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            return;
        }
    };
    out.extend_from_slice(esc);
}

fn push_uchar(out: &mut Vec<u8>, c: char) {
    use std::io::Write;
    write!(out, "\\u{:04X}", c as u32).expect("Vec write");
}

/// The value of a canonical (escaped) lexical form: borrowed when it has no
/// escapes, otherwise decoded into `buf`.
pub fn unescape_lexical<'a>(lexical: &'a str, buf: &'a mut String) -> &'a str {
    if !lexical.contains('\\') {
        return lexical;
    }
    buf.clear();
    let bytes = lexical.as_bytes();
    let mut i = 0;
    let mut run = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            buf.push_str(&lexical[run..i]);
            // Keys are produced by this module, so escapes are well-formed.
            let (c, len) = decode_escape(&bytes[i..], true).unwrap_or(('\u{fffd}', 2));
            buf.push(c);
            i += len;
            run = i;
        } else {
            i += 1;
        }
    }
    buf.push_str(&lexical[run..]);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::TermRef;
    use s_parser::{NtTokenizer, QuadReader, SliceStream};

    const XSD_INT: &[u8] = b"http://www.w3.org/2001/XMLSchema#integer";

    fn object_fast_key(line: &str) -> Option<String> {
        let input = format!("<s:s> <p:p> {line} .\n");
        let mut reader = QuadReader::new(SliceStream::new(input.as_bytes(), 1024).unwrap(), NtTokenizer);
        let quad = reader.next().unwrap().unwrap();
        nt_fast_key(quad.object()).map(str::to_owned)
    }

    #[test]
    fn fast_path_borrows_canonical_nt_terms() {
        assert_eq!(object_fast_key("<http://a/b>").as_deref(), Some("<http://a/b>"));
        assert_eq!(object_fast_key("\"plain ü\"").as_deref(), Some("\"plain ü\""));
        assert_eq!(object_fast_key("\"x\"@en-gb").as_deref(), Some("\"x\"@en-gb"));
        assert_eq!(object_fast_key("\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>").as_deref(), Some("\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>"));
    }

    #[test]
    fn fast_path_declines_non_canonical_terms() {
        for line in ["<http://a/\\u0062>", "\"a\\nb\"", "\"x\"@EN", "\"x\"^^<http://www.w3.org/2001/XMLSchema#string>", "_:b0", "\"tab\there\""] {
            assert_eq!(object_fast_key(line), None, "{line}");
        }
    }

    #[test]
    fn literals_are_canonically_escaped() {
        let mut k = KeyBuf::new();
        assert_eq!(k.literal(br#"a\u0041\t\"\'"#, ResolvedSuffix::None, None).unwrap(), r#""aA\t\"'""#);
        assert_eq!(k.literal(b"line\nbreak \x01 \x7f", ResolvedSuffix::None, None).unwrap(), "\"line\\nbreak \\u0001 \\u007F\"");
        assert_eq!(k.literal(b"\\U0001F600", ResolvedSuffix::None, None).unwrap(), "\"\u{1F600}\"");
        assert_eq!(k.literal(b"back\\\\slash", ResolvedSuffix::None, None).unwrap(), r#""back\\slash""#);
    }

    #[test]
    fn literal_suffixes_are_canonical() {
        let mut k = KeyBuf::new();
        assert_eq!(k.literal(b"x", ResolvedSuffix::Lang(b"EN-Gb"), None).unwrap(), "\"x\"@en-gb");
        let string = ResolvedSuffix::Datatype(ResolvedIri::Direct(XSD_STRING.as_bytes()));
        assert_eq!(k.literal(b"x", string, None).unwrap(), "\"x\"");
        let int = ResolvedSuffix::Datatype(ResolvedIri::Joined(b"http://www.w3.org/2001/XMLSchema#", b"integer"));
        assert_eq!(k.literal(b"42", int, None).unwrap(), "\"42\"^^<http://www.w3.org/2001/XMLSchema#integer>");
        let _ = XSD_INT;
    }

    #[test]
    fn invalid_escapes_and_utf8_are_rejected() {
        let mut k = KeyBuf::new();
        assert_eq!(k.literal(b"\\q", ResolvedSuffix::None, None), Err(EncodeError::InvalidEscape));
        assert_eq!(k.literal(b"\\u00", ResolvedSuffix::None, None), Err(EncodeError::InvalidEscape));
        assert_eq!(k.literal(b"\\uD800", ResolvedSuffix::None, None), Err(EncodeError::InvalidEscape));
        assert_eq!(k.literal(b"\xff", ResolvedSuffix::None, None), Err(EncodeError::InvalidUtf8));
        assert_eq!(k.as_str(), "");
    }

    #[test]
    fn iris_decode_escapes_and_resolve_against_base() {
        let mut k = KeyBuf::new();
        assert_eq!(k.iri(ResolvedIri::Direct(b"http://a/\\u0062"), None).unwrap(), "<http://a/b>");
        assert_eq!(k.iri(ResolvedIri::Joined(b"http://ex/", b"a\\.b\\-c"), None).unwrap(), "<http://ex/a.b-c>");
        assert_eq!(k.iri(ResolvedIri::Direct(b"c/d"), Some("http://a/b/x")).unwrap(), "<http://a/b/c/d>");
        assert_eq!(k.iri(ResolvedIri::Direct(b"urn:x"), Some("http://a/")).unwrap(), "<urn:x>");
        assert_eq!(k.iri(ResolvedIri::Direct(b"rel"), None).unwrap(), "<rel>");
        assert_eq!(k.iri(ResolvedIri::Direct(b"http://a/\\u0020"), None).unwrap(), "<http://a/\\u0020>");
    }

    #[test]
    fn blank_nodes_are_scoped_injectively() {
        let mut k = KeyBuf::new();
        assert_eq!(k.blank(BlankScope(1), BlankId::Named(b"2_x")).unwrap(), "_:b1_2_x");
        assert_eq!(k.blank(BlankScope(12), BlankId::Named(b"x")).unwrap(), "_:b12_x");
        assert_eq!(k.blank(BlankScope(1), BlankId::Fresh(5)).unwrap(), "_:b1-5");
        assert_eq!(k.blank(BlankScope(1), BlankId::Named(b"5")).unwrap(), "_:b1_5");
    }

    #[test]
    fn triple_terms_wrap_component_keys() {
        let mut k = KeyBuf::new();
        assert_eq!(k.triple("<a>", "<b>", "\"c\""), "<<( <a> <b> \"c\" )>>");
        assert_eq!(TermRef::parse(k.as_str()), Some(TermRef::Triple("<a> <b> \"c\"")));
    }

    #[test]
    fn computed_literal_values_are_escaped() {
        let mut k = KeyBuf::new();
        assert_eq!(k.literal_value("a\"b\n", None, None), "\"a\\\"b\\n\"");
        assert_eq!(k.literal_value("x", Some("EN"), None), "\"x\"@en");
        assert_eq!(k.literal_value("3", None, Some("http://www.w3.org/2001/XMLSchema#integer")), "\"3\"^^<http://www.w3.org/2001/XMLSchema#integer>");
    }

    #[test]
    fn unescape_borrows_when_possible() {
        let mut buf = String::new();
        assert_eq!(unescape_lexical("plain", &mut buf), "plain");
        assert!(buf.is_empty());
        assert_eq!(unescape_lexical(r#"a\"b\nc\\"#, &mut buf), "a\"b\nc\\");
        assert_eq!(unescape_lexical("\\u0001x", &mut buf), "\u{1}x");
    }
}
