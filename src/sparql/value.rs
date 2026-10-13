//! Expression values.
//!
//! A [`Value`] is `Copy`: its text (IRIs, lexical forms, language tags,
//! datatype IRIs) lives in an [`Arena`] and the value holds [`Sp`] spans
//! into it. The arena starts with a frozen prefix (well-known datatype IRIs,
//! then the query's constants, decoded once at compile time); evaluating an
//! expression appends scratch text after it and truncates back to a mark
//! when done. Evaluation therefore allocates nothing per row once the arena
//! has grown to its working size.

use crate::term::encode::unescape_lexical;
use crate::term::{KeyBuf, TermRef};
use std::cmp::Ordering;
use std::fmt::Write as _;

/// SPARQL's expression type error (§17.3): FILTER treats it as false,
/// BIND leaves its variable unbound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sp {
    pub start: u32,
    pub end: u32,
}

impl Sp {
    pub fn len(self) -> usize {
        (self.end - self.start) as usize
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

const WELL_KNOWN: [&str; 9] = [
    crate::term::XSD_STRING,
    crate::term::RDF_LANG_STRING,
    crate::term::XSD_BOOLEAN,
    crate::term::XSD_INTEGER,
    crate::term::XSD_DECIMAL,
    crate::term::XSD_FLOAT,
    crate::term::XSD_DOUBLE,
    crate::term::XSD_DATE_TIME,
    "http://www.w3.org/2001/XMLSchema#dayTimeDuration",
];

const fn well_known(i: usize) -> Sp {
    let mut start = 0;
    let mut j = 0;
    while j < i {
        start += WELL_KNOWN[j].len();
        j += 1;
    }
    Sp { start: start as u32, end: (start + WELL_KNOWN[i].len()) as u32 }
}

pub const DT_STRING: Sp = well_known(0);
pub const DT_LANG_STRING: Sp = well_known(1);
pub const DT_BOOLEAN: Sp = well_known(2);
pub const DT_INTEGER: Sp = well_known(3);
pub const DT_DECIMAL: Sp = well_known(4);
pub const DT_FLOAT: Sp = well_known(5);
pub const DT_DOUBLE: Sp = well_known(6);
pub const DT_DATE_TIME: Sp = well_known(7);
pub const DT_DAY_TIME_DURATION: Sp = well_known(8);

#[derive(Clone, Copy, Debug)]
pub enum Value {
    Iri(Sp),
    Blank(Sp),
    /// An RDF 1.2 triple term; the span is its whole key.
    Triple(Sp),
    Lit(Lit),
}

/// A literal: unescaped lexical form, optional language tag, datatype IRI
/// (always present: `xsd:string` for simple literals) and its parsed value.
#[derive(Clone, Copy, Debug)]
pub struct Lit {
    pub lex: Sp,
    pub lang: Option<Sp>,
    pub dt: Sp,
    pub val: Val,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Val {
    /// A simple literal or `xsd:string`.
    Str,
    LangStr,
    Bool(bool),
    Int(i64),
    Dec(f64),
    Float(f64),
    Double(f64),
    DateTime(DateTime),
    /// A datatype without value semantics here.
    Other,
    /// A lexical form invalid for its (known) datatype.
    Invalid,
}

impl Val {
    pub fn is_numeric(self) -> bool {
        matches!(self, Val::Int(_) | Val::Dec(_) | Val::Float(_) | Val::Double(_))
    }

    fn is_string(self) -> bool {
        matches!(self, Val::Str | Val::LangStr)
    }
}

/// Numeric type promotion order (SPARQL §17.3: integer < decimal < float < double).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NumKind {
    Int,
    Dec,
    Float,
    Double,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Int(i64),
    Dec(f64),
    Float(f64),
    Double(f64),
}

impl Num {
    pub fn kind(self) -> NumKind {
        match self {
            Num::Int(_) => NumKind::Int,
            Num::Dec(_) => NumKind::Dec,
            Num::Float(_) => NumKind::Float,
            Num::Double(_) => NumKind::Double,
        }
    }

    pub fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Dec(f) | Num::Float(f) | Num::Double(f) => f,
        }
    }

    pub fn of_kind(kind: NumKind, f: f64) -> Num {
        match kind {
            NumKind::Int => Num::Int(f as i64),
            NumKind::Dec => Num::Dec(f),
            NumKind::Float => Num::Float(f as f32 as f64),
            NumKind::Double => Num::Double(f),
        }
    }
}

impl Value {
    pub fn num(self) -> Option<Num> {
        match self {
            Value::Lit(Lit { val: Val::Int(i), .. }) => Some(Num::Int(i)),
            Value::Lit(Lit { val: Val::Dec(f), .. }) => Some(Num::Dec(f)),
            Value::Lit(Lit { val: Val::Float(f), .. }) => Some(Num::Float(f)),
            Value::Lit(Lit { val: Val::Double(f), .. }) => Some(Num::Double(f)),
            _ => None,
        }
    }

    pub fn lit(self) -> Option<Lit> {
        match self {
            Value::Lit(l) => Some(l),
            _ => None,
        }
    }

    /// A simple literal, `xsd:string` or language-tagged string.
    pub fn string(self) -> Option<Lit> {
        self.lit().filter(|l| l.val.is_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DateTime {
    pub year: i64,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: f64,
    /// Offset from UTC in minutes.
    pub tz: Option<i16>,
}

impl DateTime {
    pub fn parse(s: &str) -> Option<DateTime> {
        let (date, time) = s.split_once('T')?;
        let neg = date.starts_with('-');
        let date = date.strip_prefix('-').unwrap_or(date);
        let mut d = date.splitn(3, '-');
        let (y, m, dd) = (d.next()?, d.next()?, d.next()?);
        if y.len() < 4 || m.len() != 2 || dd.len() != 2 {
            return None;
        }
        let year: i64 = y.parse().ok()?;
        let (month, day): (u8, u8) = (m.parse().ok()?, dd.parse().ok()?);
        let (clock, tz) = if let Some(c) = time.strip_suffix('Z') {
            (c, Some(0))
        } else if let Some(i) = time.rfind(['+', '-']).filter(|&i| i >= 8) {
            let (sign, off) = (if &time[i..=i] == "-" { -1 } else { 1 }, &time[i + 1..]);
            let (h, m) = off.split_once(':')?;
            if h.len() != 2 || m.len() != 2 {
                return None;
            }
            let minutes = h.parse::<i16>().ok()? * 60 + m.parse::<i16>().ok()?;
            (&time[..i], Some(sign * minutes))
        } else {
            (time, None)
        };
        let mut c = clock.splitn(3, ':');
        let (h, mi, sec) = (c.next()?, c.next()?, c.next()?);
        if h.len() != 2 || mi.len() != 2 || sec.len() < 2 || !sec.as_bytes()[..2].iter().all(u8::is_ascii_digit) {
            return None;
        }
        let dt = DateTime { year: if neg { -year } else { year }, month, day, hour: h.parse().ok()?, minute: mi.parse().ok()?, second: sec.parse().ok()?, tz };
        let valid = (1..=12).contains(&dt.month) && (1..=31).contains(&dt.day) && (dt.hour < 24 || (dt.hour == 24 && dt.minute == 0 && dt.second == 0.0)) && dt.minute < 60 && dt.second < 60.0;
        valid.then_some(dt)
    }

    /// Seconds since 1970-01-01T00:00:00Z (local time when there is no
    /// timezone).
    pub fn epoch_seconds(&self) -> f64 {
        // Days from civil (Howard Hinnant's algorithm).
        let y = if self.month <= 2 { self.year - 1 } else { self.year };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(self.month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        let secs = days * 86_400 + i64::from(self.hour) * 3600 + i64::from(self.minute) * 60 - i64::from(self.tz.unwrap_or(0)) * 60;
        secs as f64 + self.second
    }

    /// Compares two date-times; `None` when only one has a timezone and the
    /// 14-hour window leaves the order indeterminate.
    pub fn compare(&self, other: &DateTime) -> Option<Ordering> {
        let (a, b) = (self.epoch_seconds(), other.epoch_seconds());
        if self.tz.is_some() == other.tz.is_some() {
            return a.partial_cmp(&b);
        }
        const WINDOW: f64 = 14.0 * 3600.0;
        if a + WINDOW < b - WINDOW || (self.tz.is_some() && a < b - WINDOW) || (other.tz.is_some() && a + WINDOW < b) {
            Some(Ordering::Less)
        } else if b + WINDOW < a - WINDOW || (self.tz.is_some() && a > b + WINDOW) || (other.tz.is_some() && a - WINDOW > b) {
            Some(Ordering::Greater)
        } else {
            None
        }
    }
}

const XSD_INTEGER_DERIVED: [&str; 12] = [
    "int",
    "long",
    "short",
    "byte",
    "nonNegativeInteger",
    "positiveInteger",
    "negativeInteger",
    "nonPositiveInteger",
    "unsignedLong",
    "unsignedInt",
    "unsignedShort",
    "unsignedByte",
];

fn parse_val(lex: &str, dt: &str) -> Val {
    let Some(local) = dt.strip_prefix(crate::term::XSD) else {
        return if dt == crate::term::RDF_LANG_STRING { Val::LangStr } else { Val::Other };
    };
    let parsed = match local {
        "string" => Some(Val::Str),
        "boolean" => match lex {
            "true" | "1" => Some(Val::Bool(true)),
            "false" | "0" => Some(Val::Bool(false)),
            _ => None,
        },
        "integer" => parse_integer(lex).map(Val::Int),
        "decimal" => parse_decimal(lex).map(Val::Dec),
        "float" => parse_double(lex).map(|f| Val::Float(f as f32 as f64)),
        "double" => parse_double(lex).map(Val::Double),
        "dateTime" => DateTime::parse(lex).map(Val::DateTime),
        l if XSD_INTEGER_DERIVED.contains(&l) => parse_integer(lex).map(Val::Int),
        _ => Some(Val::Other),
    };
    parsed.unwrap_or(Val::Invalid)
}

pub fn parse_integer(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.strip_prefix('+').unwrap_or(s).parse().ok()
}

pub fn parse_decimal(s: &str) -> Option<f64> {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    let ok = (!int.is_empty() || !frac.is_empty()) && int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) && !(body.ends_with('.') && int.is_empty());
    if !ok {
        return None;
    }
    s.parse().ok()
}

pub fn parse_double(s: &str) -> Option<f64> {
    match s {
        "INF" | "+INF" => Some(f64::INFINITY),
        "-INF" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ => {
            let body = s.strip_prefix(['+', '-']).unwrap_or(s);
            let mantissa = body.split(['e', 'E']).next().unwrap_or("");
            if mantissa.is_empty() || mantissa == "." || !body.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-')) {
                return None;
            }
            s.parse().ok()
        }
    }
}

pub struct Arena {
    text: String,
    frozen: usize,
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

impl Arena {
    pub fn new() -> Self {
        let mut text = String::with_capacity(512);
        WELL_KNOWN.iter().for_each(|s| text.push_str(s));
        let frozen = text.len();
        Self { text, frozen }
    }

    /// Everything pushed so far survives every later [`Self::truncate`].
    pub fn freeze(&mut self) {
        self.frozen = self.text.len();
    }

    pub fn mark(&self) -> usize {
        self.text.len()
    }

    pub fn truncate(&mut self, mark: usize) {
        self.text.truncate(mark.max(self.frozen));
    }

    pub fn text(&self, sp: Sp) -> &str {
        &self.text[sp.start as usize..sp.end as usize]
    }

    pub fn push(&mut self, s: &str) -> Sp {
        let start = self.text.len() as u32;
        self.text.push_str(s);
        Sp { start, end: self.text.len() as u32 }
    }

    /// Appends text produced by `f`, returning its span.
    pub fn push_with(&mut self, f: impl FnOnce(&mut String)) -> Sp {
        let start = self.text.len() as u32;
        f(&mut self.text);
        Sp { start, end: self.text.len() as u32 }
    }

    /// Appends the concatenation of spans already in the arena.
    pub fn concat(&mut self, spans: &[Sp]) -> Sp {
        let start = self.text.len() as u32;
        for sp in spans {
            self.text.extend_from_within(sp.start as usize..sp.end as usize);
        }
        Sp { start, end: self.text.len() as u32 }
    }

    /// A sub-span of an existing span (byte offsets within it).
    pub fn sub(&self, sp: Sp, from_byte: usize, to_byte: usize) -> Sp {
        Sp { start: sp.start + from_byte as u32, end: sp.start + to_byte as u32 }
    }

    /// Decodes a canonical key into a value.
    pub fn decode(&mut self, key: &str) -> Option<Value> {
        Some(match TermRef::parse(key)? {
            TermRef::Iri(i) => Value::Iri(self.push(i)),
            TermRef::Blank(b) => Value::Blank(self.push(b)),
            TermRef::Triple(_) => Value::Triple(self.push(key)),
            TermRef::Literal { lexical, lang, datatype } => {
                let mut scratch = String::new();
                let lex = self.push(unescape_lexical(lexical, &mut scratch));
                if let Some(lang) = lang {
                    let lang = self.push(lang);
                    Value::Lit(Lit { lex, lang: Some(lang), dt: DT_LANG_STRING, val: Val::LangStr })
                } else {
                    let dt_text = datatype.unwrap_or(crate::term::XSD_STRING);
                    let dt = WELL_KNOWN.iter().position(|w| *w == dt_text).map(well_known).unwrap_or_else(|| self.push(dt_text));
                    let val = parse_val(self.text(lex), dt_text);
                    Value::Lit(Lit { lex, lang: None, dt, val })
                }
            }
        })
    }

    /// Writes `v`'s canonical key into `out`.
    pub fn key<'k>(&self, v: Value, out: &'k mut KeyBuf) -> &'k str {
        match v {
            Value::Iri(sp) => out.iri(s_parser::ResolvedIri::Direct(self.text(sp).as_bytes()), None).unwrap_or(""),
            Value::Blank(sp) => out.blank_label(self.text(sp)),
            Value::Triple(sp) => out.raw(self.text(sp)),
            Value::Lit(l) => {
                let dt = (l.dt != DT_STRING && l.lang.is_none()).then(|| self.text(l.dt));
                out.literal_value(self.text(l.lex), l.lang.map(|t| self.text(t)), dt)
            }
        }
    }

    pub fn string(&mut self, s: &str) -> Value {
        let lex = self.push(s);
        Value::Lit(Lit { lex, lang: None, dt: DT_STRING, val: Val::Str })
    }

    /// A string literal over an existing span, keeping `like`'s language tag
    /// (string functions return their argument's "kind").
    pub fn string_like(&self, lex: Sp, like: Lit) -> Value {
        match like.lang {
            Some(tag) => Value::Lit(Lit { lex, lang: Some(tag), dt: DT_LANG_STRING, val: Val::LangStr }),
            None => Value::Lit(Lit { lex, lang: None, dt: like.dt, val: Val::Str }),
        }
    }

    pub fn boolean(&mut self, b: bool) -> Value {
        let lex = self.push(if b { "true" } else { "false" });
        Value::Lit(Lit { lex, lang: None, dt: DT_BOOLEAN, val: Val::Bool(b) })
    }

    pub fn number(&mut self, n: Num) -> Value {
        match n {
            Num::Int(i) => {
                let lex = self.push_with(|t| write!(t, "{i}").expect("String write"));
                Value::Lit(Lit { lex, lang: None, dt: DT_INTEGER, val: Val::Int(i) })
            }
            Num::Dec(f) => {
                let lex = self.push_with(|t| format_decimal(f, t));
                Value::Lit(Lit { lex, lang: None, dt: DT_DECIMAL, val: Val::Dec(f) })
            }
            Num::Float(f) => {
                let lex = self.push_with(|t| format_double(f, t));
                Value::Lit(Lit { lex, lang: None, dt: DT_FLOAT, val: Val::Float(f) })
            }
            Num::Double(f) => {
                let lex = self.push_with(|t| format_double(f, t));
                Value::Lit(Lit { lex, lang: None, dt: DT_DOUBLE, val: Val::Double(f) })
            }
        }
    }

    pub fn typed(&mut self, lex: &str, dt: &str) -> Value {
        let lex_sp = self.push(lex);
        let dt_sp = WELL_KNOWN.iter().position(|w| *w == dt).map(well_known).unwrap_or_else(|| self.push(dt));
        let val = parse_val(lex, dt);
        Value::Lit(Lit { lex: lex_sp, lang: None, dt: dt_sp, val })
    }
}

pub fn format_decimal(f: f64, out: &mut String) {
    let start = out.len();
    write!(out, "{f}").expect("String write");
    if !out[start..].contains('.') {
        out.push_str(".0");
    }
}

pub fn format_double(f: f64, out: &mut String) {
    if f.is_nan() {
        out.push_str("NaN");
    } else if f.is_infinite() {
        out.push_str(if f > 0.0 { "INF" } else { "-INF" });
    } else {
        let start = out.len();
        write!(out, "{f:E}").expect("String write");
        let e = start + out[start..].find('E').expect("exponent format");
        if !out[start..e].contains('.') {
            out.insert_str(e, ".0");
        }
    }
}

/// SPARQL `=` (RDFterm-equal extended with value equality): `Err` is a type
/// error.
pub fn equals(arena: &Arena, a: Value, b: Value) -> Result<bool, TypeError> {
    match (a, b) {
        (Value::Lit(x), Value::Lit(y)) => {
            if let (Some(p), Some(q)) = (a.num(), b.num()) {
                return Ok(p.as_f64() == q.as_f64());
            }
            match (x.val, y.val) {
                (Val::Str, Val::Str) => Ok(arena.text(x.lex) == arena.text(y.lex)),
                (Val::LangStr, Val::LangStr) => Ok(arena.text(x.lex) == arena.text(y.lex) && arena.text(x.lang.unwrap_or_default()) == arena.text(y.lang.unwrap_or_default())),
                (Val::Bool(p), Val::Bool(q)) => Ok(p == q),
                (Val::DateTime(p), Val::DateTime(q)) => p.compare(&q).map(|o| o == Ordering::Equal).ok_or(TypeError),
                _ if same_literal(arena, x, y) => Ok(true),
                _ => Err(TypeError),
            }
        }
        (Value::Lit(_), _) | (_, Value::Lit(_)) => Ok(false),
        _ => Ok(same_term(arena, a, b)),
    }
}

fn same_literal(arena: &Arena, x: Lit, y: Lit) -> bool {
    arena.text(x.lex) == arena.text(y.lex) && arena.text(x.dt) == arena.text(y.dt) && x.lang.map(|t| arena.text(t)) == y.lang.map(|t| arena.text(t))
}

/// `sameTerm`.
pub fn same_term(arena: &Arena, a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Iri(x), Value::Iri(y)) | (Value::Blank(x), Value::Blank(y)) | (Value::Triple(x), Value::Triple(y)) => arena.text(x) == arena.text(y),
        (Value::Lit(x), Value::Lit(y)) => same_literal(arena, x, y),
        _ => false,
    }
}

/// SPARQL `<`-family ordering for the operand types where it is defined.
pub fn compare(arena: &Arena, a: Value, b: Value) -> Result<Ordering, TypeError> {
    if let (Some(p), Some(q)) = (a.num(), b.num()) {
        return p.as_f64().partial_cmp(&q.as_f64()).ok_or(TypeError);
    }
    match (a, b) {
        (Value::Lit(x), Value::Lit(y)) => match (x.val, y.val) {
            (Val::Str, Val::Str) => Ok(arena.text(x.lex).cmp(arena.text(y.lex))),
            (Val::Bool(p), Val::Bool(q)) => Ok(p.cmp(&q)),
            (Val::DateTime(p), Val::DateTime(q)) => p.compare(&q).ok_or(TypeError),
            _ => Err(TypeError),
        },
        _ => Err(TypeError),
    }
}

/// The ORDER BY total order: unbound, blank nodes, IRIs, triple terms, then
/// literals (by `<` where defined, otherwise by datatype, lexical form and
/// language tag).
pub fn order(arena: &Arena, a: Option<Value>, b: Option<Value>) -> Ordering {
    fn rank(v: Option<Value>) -> u8 {
        match v {
            None => 0,
            Some(Value::Blank(_)) => 1,
            Some(Value::Iri(_)) => 2,
            Some(Value::Triple(_)) => 3,
            Some(Value::Lit(_)) => 4,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Some(Value::Lit(x)), Some(Value::Lit(y))) => {
            if let Ok(o) = compare(arena, a.expect("some"), b.expect("some"))
                && (o != Ordering::Equal || same_literal(arena, x, y))
            {
                return o;
            }
            // Values of different kinds: numbers, then everything else by
            // datatype; ties by lexical form and language tag.
            let numeric = |l: Lit| !l.val.is_numeric();
            numeric(x)
                .cmp(&numeric(y))
                .then_with(|| arena.text(x.dt).cmp(arena.text(y.dt)))
                .then_with(|| arena.text(x.lex).cmp(arena.text(y.lex)))
                .then_with(|| x.lang.map(|t| arena.text(t)).cmp(&y.lang.map(|t| arena.text(t))))
        }
        (Some(Value::Iri(x)), Some(Value::Iri(y))) | (Some(Value::Blank(x)), Some(Value::Blank(y))) | (Some(Value::Triple(x)), Some(Value::Triple(y))) => arena.text(x).cmp(arena.text(y)),
        _ => Ordering::Equal,
    }
}

/// Effective boolean value (§17.2.2).
pub fn ebv(arena: &Arena, v: Value) -> Result<bool, TypeError> {
    match v {
        Value::Lit(l) => match l.val {
            Val::Bool(b) => Ok(b),
            Val::Str => Ok(!l.lex.is_empty()),
            Val::Int(i) => Ok(i != 0),
            Val::Dec(f) | Val::Float(f) | Val::Double(f) => Ok(f != 0.0 && !f.is_nan()),
            Val::Invalid if is_numeric_or_boolean_dt(arena.text(l.dt)) => Ok(false),
            _ => Err(TypeError),
        },
        _ => Err(TypeError),
    }
}

fn is_numeric_or_boolean_dt(dt: &str) -> bool {
    matches!(dt.strip_prefix(crate::term::XSD), Some("boolean" | "integer" | "decimal" | "float" | "double")) || dt.strip_prefix(crate::term::XSD).is_some_and(|l| XSD_INTEGER_DERIVED.contains(&l))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(arena: &mut Arena, key: &str) -> Value {
        arena.decode(key).unwrap()
    }

    #[test]
    fn decodes_literals_with_values() {
        let mut a = Arena::new();
        let v = decode(&mut a, "\"42\"^^<http://www.w3.org/2001/XMLSchema#integer>");
        assert_eq!(v.num(), Some(Num::Int(42)));
        let v = decode(&mut a, "\"7\"^^<http://www.w3.org/2001/XMLSchema#unsignedByte>");
        assert_eq!(v.num(), Some(Num::Int(7)));
        assert_eq!(a.text(v.lit().unwrap().dt), "http://www.w3.org/2001/XMLSchema#unsignedByte");
        let v = decode(&mut a, "\"a\\nb\"@en");
        let l = v.lit().unwrap();
        assert_eq!((a.text(l.lex), a.text(l.lang.unwrap()), l.val), ("a\nb", "en", Val::LangStr));
        let v = decode(&mut a, "\"x\"^^<http://www.w3.org/2001/XMLSchema#integer>");
        assert_eq!(v.lit().unwrap().val, Val::Invalid);
        let v = decode(&mut a, "\"1.5e1\"^^<http://www.w3.org/2001/XMLSchema#double>");
        assert_eq!(v.num(), Some(Num::Double(15.0)));
        assert!(matches!(decode(&mut a, "<http://a>"), Value::Iri(_)));
        assert!(matches!(decode(&mut a, "_:b"), Value::Blank(_)));
    }

    #[test]
    fn keys_round_trip_through_values() {
        let mut a = Arena::new();
        let mut k = KeyBuf::new();
        for key in ["<http://a>", "_:b1_x", "\"q\\\"\"", "\"x\"@en", "\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>", "\"z\"^^<http://t>"] {
            let v = decode(&mut a, key);
            assert_eq!(a.key(v, &mut k), key);
        }
    }

    #[test]
    fn number_formatting_is_canonical() {
        let mut a = Arena::new();
        let mut k = KeyBuf::new();
        let mut key = |n| {
            let v = a.number(n);
            a.key(v, &mut k).to_owned()
        };
        assert_eq!(key(Num::Int(-3)), "\"-3\"^^<http://www.w3.org/2001/XMLSchema#integer>");
        assert_eq!(key(Num::Dec(2.0)), "\"2.0\"^^<http://www.w3.org/2001/XMLSchema#decimal>");
        assert_eq!(key(Num::Dec(0.25)), "\"0.25\"^^<http://www.w3.org/2001/XMLSchema#decimal>");
        assert_eq!(key(Num::Double(4.0)), "\"4.0E0\"^^<http://www.w3.org/2001/XMLSchema#double>");
        assert_eq!(key(Num::Double(1500.0)), "\"1.5E3\"^^<http://www.w3.org/2001/XMLSchema#double>");
        assert_eq!(key(Num::Double(f64::NEG_INFINITY)), "\"-INF\"^^<http://www.w3.org/2001/XMLSchema#double>");
    }

    #[test]
    fn lexical_validation() {
        assert_eq!(parse_integer("+12"), Some(12));
        assert_eq!(parse_integer("1.0"), None);
        assert_eq!(parse_integer(""), None);
        assert_eq!(parse_decimal("-1.50"), Some(-1.5));
        assert_eq!(parse_decimal(".5"), Some(0.5));
        assert_eq!(parse_decimal("1e5"), None);
        assert_eq!(parse_double("1e5"), Some(100000.0));
        assert_eq!(parse_double("abc"), None);
        assert!(parse_double("NaN").unwrap().is_nan());
    }

    #[test]
    fn date_times_parse_and_compare() {
        let a = DateTime::parse("2020-03-04T05:06:07.5Z").unwrap();
        assert_eq!((a.year, a.month, a.day, a.hour, a.minute, a.second, a.tz), (2020, 3, 4, 5, 6, 7.5, Some(0)));
        let b = DateTime::parse("2020-03-04T07:06:07.5+02:00").unwrap();
        assert_eq!(a.compare(&b), Some(Ordering::Equal));
        let local = DateTime::parse("2020-03-04T05:06:07").unwrap();
        assert_eq!(a.compare(&local), None, "indeterminate within 14 hours");
        let far = DateTime::parse("2021-01-01T00:00:00").unwrap();
        assert_eq!(a.compare(&far), Some(Ordering::Less));
        assert_eq!(DateTime::parse("1970-01-01T00:00:00Z").unwrap().epoch_seconds(), 0.0);
        assert!(DateTime::parse("2020-13-01T00:00:00").is_none());
        assert!(DateTime::parse("not a date").is_none());
    }

    #[test]
    fn equality_and_ordering() {
        let mut a = Arena::new();
        let one = decode(&mut a, "\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>");
        let one_d = decode(&mut a, "\"1.0\"^^<http://www.w3.org/2001/XMLSchema#decimal>");
        let x = decode(&mut a, "\"x\"");
        let iri = decode(&mut a, "<http://a>");
        assert_eq!(equals(&a, one, one_d), Ok(true));
        assert!(!same_term(&a, one, one_d));
        assert_eq!(equals(&a, one, x), Err(TypeError));
        assert_eq!(equals(&a, one, iri), Ok(false));
        assert_eq!(equals(&a, x, x), Ok(true));
        assert_eq!(compare(&a, one, x), Err(TypeError));
        assert_eq!(order(&a, None, Some(iri)), Ordering::Less);
        assert_eq!(order(&a, Some(iri), Some(x)), Ordering::Less);
        assert_eq!(order(&a, Some(one), Some(x)), Ordering::Less, "numbers before other literals");
    }

    #[test]
    fn effective_boolean_value() {
        let mut a = Arena::new();
        let cases = [("\"\"", Ok(false)), ("\"a\"", Ok(true)), ("\"0\"^^<http://www.w3.org/2001/XMLSchema#integer>", Ok(false)), ("\"x\"^^<http://www.w3.org/2001/XMLSchema#boolean>", Ok(false)), ("<http://a>", Err(TypeError)), ("\"a\"@en", Err(TypeError))];
        for (key, expected) in cases {
            let v = decode(&mut a, key);
            assert_eq!(ebv(&a, v), expected, "{key}");
        }
    }

    #[test]
    fn arena_truncation_keeps_the_frozen_prefix() {
        let mut a = Arena::new();
        let c = a.string("const");
        a.freeze();
        let m = a.mark();
        a.string("scratch");
        a.truncate(m);
        a.truncate(0);
        assert_eq!(a.text(c.lit().unwrap().lex), "const");
        assert_eq!(a.mark(), m);
    }
}
