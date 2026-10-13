//! Expression evaluation (§17), with [`TypeError`] as SPARQL's type error.
//!
//! Values reference text in the arena; every top-level evaluation truncates
//! the arena back to where it started, so per-row evaluation reuses the
//! same bytes.

use super::exec::{Active, Ctx, Env};
use super::locals::LocalTerms;
use super::plan::*;
use super::value::{self, Arena, TypeError, DT_DAY_TIME_DURATION, DT_STRING, Lit, Num, NumKind, Sp, Val, Value};
use s_graph::BlockStorage;
use s_parser::sparql::expr::Builtin;
use std::cmp::Ordering;
use std::fmt::Write as _;
use std::ops::ControlFlow;
use std::rc::Rc;

type R = Result<Value, TypeError>;

/// Decodes a term id into the arena.
pub fn term_value<S: BlockStorage>(ctx: &Ctx<'_, S>, env: &mut Env, id: u32) -> R {
    if id == UNBOUND {
        return Err(TypeError);
    }
    let Env { arena, locals, key, .. } = env;
    if LocalTerms::is_local(id) {
        return arena.decode(locals.key(id).ok_or(TypeError)?).ok_or(TypeError);
    }
    if !ctx.store.dictionary().key_into(s_dict::TermId(id), key) {
        return Err(TypeError);
    }
    arena.decode(std::str::from_utf8(key).map_err(|_| TypeError)?).ok_or(TypeError)
}

/// The term id of a value: its dictionary id, or a query-local one.
pub fn value_id<S: BlockStorage>(ctx: &Ctx<'_, S>, env: &mut Env, v: Value) -> u32 {
    let Env { arena, locals, keybuf, .. } = env;
    let key = arena.key(v, keybuf);
    locals.id_for(ctx.store.dictionary(), key)
}

fn num_op(a: Num, b: Num, op: fn(f64, f64) -> f64, int_op: fn(i64, i64) -> Option<i64>) -> Result<Num, TypeError> {
    let kind = a.kind().max(b.kind());
    match (kind, a, b) {
        (NumKind::Int, Num::Int(x), Num::Int(y)) => int_op(x, y).map(Num::Int).ok_or(TypeError),
        _ => Ok(Num::of_kind(kind, op(a.as_f64(), b.as_f64()))),
    }
}

fn divide(a: Num, b: Num) -> Result<Num, TypeError> {
    let kind = a.kind().max(b.kind()).max(NumKind::Dec);
    let d = b.as_f64();
    if d == 0.0 && kind == NumKind::Dec {
        return Err(TypeError);
    }
    Ok(Num::of_kind(kind, a.as_f64() / d))
}

/// Two string arguments are compatible (§17.4.3.1.1): the second is simple or
/// `xsd:string`, or both carry the same language tag.
fn compatible(arena: &Arena, a: Lit, b: Lit) -> bool {
    match (a.lang, b.lang) {
        (_, None) => true,
        (Some(x), Some(y)) => arena.text(x) == arena.text(y),
        (None, Some(_)) => false,
    }
}

fn simple(lex: Sp) -> Value {
    Value::Lit(Lit { lex, lang: None, dt: DT_STRING, val: Val::Str })
}

impl<S: BlockStorage> Ctx<'_, S> {
    /// FILTER semantics: true iff the effective boolean value is true.
    pub fn ebv(&self, env: &mut Env, e: ExprId, row: &mut [u32], active: Active) -> bool {
        let mark = env.arena.mark();
        let result = self.value(env, e, row, active).and_then(|v| value::ebv(&env.arena, v));
        env.arena.truncate(mark);
        result == Ok(true)
    }

    /// BIND semantics: the term id of the value, `None` on error.
    pub fn expr_id(&self, env: &mut Env, e: ExprId, row: &mut [u32], active: Active) -> Option<u32> {
        match self.plan.expr(e) {
            Expr::Var(s) => Some(row[s as usize]).filter(|&id| id != UNBOUND),
            Expr::Const(id, _) => Some(id),
            _ => {
                let mark = env.arena.mark();
                let id = self.value(env, e, row, active).ok().map(|v| value_id(self, env, v));
                env.arena.truncate(mark);
                id
            }
        }
    }

    fn ebv_of(&self, env: &mut Env, e: ExprId, row: &mut [u32], active: Active) -> Result<bool, TypeError> {
        let v = self.value(env, e, row, active)?;
        value::ebv(&env.arena, v)
    }

    fn num_of(&self, env: &mut Env, e: ExprId, row: &mut [u32], active: Active) -> Result<Num, TypeError> {
        self.value(env, e, row, active)?.num().ok_or(TypeError)
    }

    fn string_of(&self, env: &mut Env, e: ExprId, row: &mut [u32], active: Active) -> Result<Lit, TypeError> {
        self.value(env, e, row, active)?.string().ok_or(TypeError)
    }

    pub fn value(&self, env: &mut Env, e: ExprId, row: &mut [u32], active: Active) -> R {
        match self.plan.expr(e) {
            Expr::Var(s) => term_value(self, env, row[s as usize]),
            Expr::Const(_, v) => Ok(v),
            Expr::Or(a, b) => {
                let (x, y) = (self.ebv_of(env, a, row, active), self.ebv_of(env, b, row, active));
                let r = match (x, y) {
                    (Ok(true), _) | (_, Ok(true)) => Ok(true),
                    (Ok(false), Ok(false)) => Ok(false),
                    _ => Err(TypeError),
                };
                Ok(env.arena.boolean(r?))
            }
            Expr::And(a, b) => {
                let (x, y) = (self.ebv_of(env, a, row, active), self.ebv_of(env, b, row, active));
                let r = match (x, y) {
                    (Ok(false), _) | (_, Ok(false)) => Ok(false),
                    (Ok(true), Ok(true)) => Ok(true),
                    _ => Err(TypeError),
                };
                Ok(env.arena.boolean(r?))
            }
            Expr::Not(a) => {
                let b = self.ebv_of(env, a, row, active)?;
                Ok(env.arena.boolean(!b))
            }
            Expr::Eq(a, b) | Expr::Ne(a, b) => {
                let (x, y) = (self.value(env, a, row, active)?, self.value(env, b, row, active)?);
                let eq = value::equals(&env.arena, x, y)?;
                Ok(env.arena.boolean(eq == matches!(self.plan.expr(e), Expr::Eq(..))))
            }
            Expr::Lt(a, b) | Expr::Le(a, b) | Expr::Gt(a, b) | Expr::Ge(a, b) => {
                let (x, y) = (self.value(env, a, row, active)?, self.value(env, b, row, active)?);
                let o = value::compare(&env.arena, x, y)?;
                let r = match self.plan.expr(e) {
                    Expr::Lt(..) => o == Ordering::Less,
                    Expr::Le(..) => o != Ordering::Greater,
                    Expr::Gt(..) => o == Ordering::Greater,
                    _ => o != Ordering::Less,
                };
                Ok(env.arena.boolean(r))
            }
            Expr::In(a, list) | Expr::NotIn(a, list) => {
                let x = self.value(env, a, row, active)?;
                let mut error = false;
                let mut found = false;
                for &item in self.plan.expr_list(list) {
                    match self.value(env, item, row, active).and_then(|y| value::equals(&env.arena, x, y)) {
                        Ok(true) => {
                            found = true;
                            break;
                        }
                        Ok(false) => {}
                        Err(TypeError) => error = true,
                    }
                }
                if !found && error {
                    return Err(TypeError);
                }
                Ok(env.arena.boolean(found == matches!(self.plan.expr(e), Expr::In(..))))
            }
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
                let (x, y) = (self.num_of(env, a, row, active)?, self.num_of(env, b, row, active)?);
                let n = match self.plan.expr(e) {
                    Expr::Add(..) => num_op(x, y, |p, q| p + q, i64::checked_add)?,
                    Expr::Sub(..) => num_op(x, y, |p, q| p - q, i64::checked_sub)?,
                    Expr::Mul(..) => num_op(x, y, |p, q| p * q, i64::checked_mul)?,
                    _ => divide(x, y)?,
                };
                Ok(env.arena.number(n))
            }
            Expr::Plus(a) => {
                let n = self.num_of(env, a, row, active)?;
                Ok(env.arena.number(n))
            }
            Expr::Neg(a) => {
                let n = match self.num_of(env, a, row, active)? {
                    Num::Int(i) => Num::Int(i.checked_neg().ok_or(TypeError)?),
                    other => Num::of_kind(other.kind(), -other.as_f64()),
                };
                Ok(env.arena.number(n))
            }
            Expr::Call(builtin, args) => self.call(env, builtin, self.plan.expr_list(args), row, active),
            Expr::Cast(cast, a) => {
                let v = self.value(env, a, row, active)?;
                cast_value(&mut env.arena, cast, v)
            }
            Expr::Exists(n) | Expr::NotExists(n) => {
                let found = self.eval(env, n, active, row, &mut |_, _| ControlFlow::Break(())).is_break();
                Ok(env.arena.boolean(found == matches!(self.plan.expr(e), Expr::Exists(_))))
            }
        }
    }

    fn call(&self, env: &mut Env, builtin: Builtin, args: &[ExprId], row: &mut [u32], active: Active) -> R {
        let arg = |i: usize| args.get(i).copied().ok_or(TypeError);
        match builtin {
            Builtin::Bound => match self.plan.expr(arg(0)?) {
                Expr::Var(s) => Ok(env.arena.boolean(row[s as usize] != UNBOUND)),
                _ => Err(TypeError),
            },
            Builtin::If => {
                let c = self.ebv_of(env, arg(0)?, row, active)?;
                self.value(env, arg(if c { 1 } else { 2 })?, row, active)
            }
            Builtin::Coalesce => args.iter().find_map(|&a| self.value(env, a, row, active).ok()).ok_or(TypeError),
            Builtin::SameTerm => {
                let (x, y) = (self.value(env, arg(0)?, row, active)?, self.value(env, arg(1)?, row, active)?);
                Ok(env.arena.boolean(value::same_term(&env.arena, x, y)))
            }
            Builtin::IsIri | Builtin::IsBlank | Builtin::IsLiteral | Builtin::IsNumeric => {
                let v = self.value(env, arg(0)?, row, active)?;
                let r = match builtin {
                    Builtin::IsIri => matches!(v, Value::Iri(_)),
                    Builtin::IsBlank => matches!(v, Value::Blank(_)),
                    Builtin::IsLiteral => matches!(v, Value::Lit(_)),
                    _ => v.num().is_some(),
                };
                Ok(env.arena.boolean(r))
            }
            Builtin::Str => match self.value(env, arg(0)?, row, active)? {
                Value::Iri(sp) => Ok(simple(sp)),
                Value::Lit(l) => Ok(simple(l.lex)),
                _ => Err(TypeError),
            },
            Builtin::Lang => match self.value(env, arg(0)?, row, active)? {
                Value::Lit(l) => Ok(match l.lang {
                    Some(t) => simple(t),
                    None => env.arena.string(""),
                }),
                _ => Err(TypeError),
            },
            Builtin::Datatype => match self.value(env, arg(0)?, row, active)? {
                Value::Lit(l) => Ok(Value::Iri(l.dt)),
                _ => Err(TypeError),
            },
            Builtin::LangMatches => {
                let (tag, range) = (self.string_of(env, arg(0)?, row, active)?, self.string_of(env, arg(1)?, row, active)?);
                let (tag, range) = (env.arena.text(tag.lex), env.arena.text(range.lex));
                let r = if range == "*" {
                    !tag.is_empty()
                } else {
                    tag.eq_ignore_ascii_case(range) || (tag.len() > range.len() && tag[..range.len()].eq_ignore_ascii_case(range) && tag.as_bytes()[range.len()] == b'-')
                };
                Ok(env.arena.boolean(r))
            }
            Builtin::Iri => match self.value(env, arg(0)?, row, active)? {
                Value::Iri(sp) => Ok(Value::Iri(sp)),
                Value::Lit(l) if l.val == Val::Str => Ok(Value::Iri(l.lex)),
                _ => Err(TypeError),
            },
            Builtin::BNode => {
                env.fresh += 1;
                let n = env.fresh;
                match args.first() {
                    None => Ok(Value::Blank(env.arena.push_with(|t| write!(t, "q{n}").expect("String write")))),
                    Some(&a) => {
                        let l = self.string_of(env, a, row, active)?;
                        if l.lang.is_some() {
                            return Err(TypeError);
                        }
                        let h = super::hash::hash_bytes(env.arena.text(l.lex).as_bytes());
                        Ok(Value::Blank(env.arena.push_with(|t| write!(t, "qs{h:x}").expect("String write"))))
                    }
                }
            }
            Builtin::Rand => {
                let r = (env.next_random() >> 11) as f64 / (1u64 << 53) as f64;
                Ok(env.arena.number(Num::Double(r)))
            }
            Builtin::Abs | Builtin::Ceil | Builtin::Floor | Builtin::Round => {
                let n = self.num_of(env, arg(0)?, row, active)?;
                let r = match n {
                    Num::Int(i) => Num::Int(if builtin == Builtin::Abs { i.checked_abs().ok_or(TypeError)? } else { i }),
                    other => {
                        let f = other.as_f64();
                        let g = match builtin {
                            Builtin::Abs => f.abs(),
                            Builtin::Ceil => f.ceil(),
                            Builtin::Floor => f.floor(),
                            _ => (f + 0.5).floor(),
                        };
                        Num::of_kind(other.kind(), g)
                    }
                };
                Ok(env.arena.number(r))
            }
            Builtin::StrLen => {
                let l = self.string_of(env, arg(0)?, row, active)?;
                let n = env.arena.text(l.lex).chars().count() as i64;
                Ok(env.arena.number(Num::Int(n)))
            }
            Builtin::UCase | Builtin::LCase => {
                let l = self.string_of(env, arg(0)?, row, active)?;
                env.scratch.clear();
                env.scratch.push_str(env.arena.text(l.lex));
                let upper = builtin == Builtin::UCase;
                let scratch = &env.scratch;
                let lex = env.arena.push_with(|t| if upper { t.push_str(&scratch.to_uppercase()) } else { t.push_str(&scratch.to_lowercase()) });
                Ok(env.arena.string_like(lex, l))
            }
            Builtin::Substr => {
                let l = self.string_of(env, arg(0)?, row, active)?;
                let start = self.num_of(env, arg(1)?, row, active)?.as_f64().round();
                let len = match args.get(2) {
                    Some(&a) => Some(self.num_of(env, a, row, active)?.as_f64().round()),
                    None => None,
                };
                if start.is_nan() || len.is_some_and(f64::is_nan) {
                    return Ok(env.arena.string_like(Sp { start: l.lex.start, end: l.lex.start }, l));
                }
                let text = env.arena.text(l.lex);
                let from = start.max(1.0);
                let to = match len {
                    Some(n) => (start + n).max(from),
                    None => f64::INFINITY,
                };
                let mut chars = text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len()));
                let begin = chars.by_ref().nth(from as usize - 1).unwrap_or(text.len());
                let end = if to.is_infinite() { text.len() } else { text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).nth((to - 1.0) as usize).unwrap_or(text.len()) };
                let sp = env.arena.sub(l.lex, begin, end.max(begin));
                Ok(env.arena.string_like(sp, l))
            }
            Builtin::Concat => {
                let mut lang: Option<Option<Sp>> = None;
                let mut parts: [Sp; 8] = [Sp::default(); 8];
                let mut spill = Vec::new();
                for (i, &a) in args.iter().enumerate() {
                    let l = self.string_of(env, a, row, active)?;
                    lang = Some(match lang {
                        None => l.lang,
                        Some(Some(t)) if l.lang.is_some_and(|u| env.arena.text(u) == env.arena.text(t)) => Some(t),
                        Some(_) => None,
                    });
                    if i < parts.len() { parts[i] = l.lex } else { spill.push(l.lex) }
                }
                let spans: Vec<Sp> = parts[..args.len().min(8)].iter().copied().chain(spill).collect();
                let lex = env.arena.concat(&spans);
                Ok(match lang.flatten() {
                    Some(t) => Value::Lit(Lit { lex, lang: Some(t), dt: value::DT_LANG_STRING, val: Val::LangStr }),
                    None => simple(lex),
                })
            }
            Builtin::Contains | Builtin::StrStarts | Builtin::StrEnds | Builtin::StrBefore | Builtin::StrAfter => {
                let (a, b) = (self.string_of(env, arg(0)?, row, active)?, self.string_of(env, arg(1)?, row, active)?);
                if !compatible(&env.arena, a, b) {
                    return Err(TypeError);
                }
                let (x, y) = (env.arena.text(a.lex), env.arena.text(b.lex));
                match builtin {
                    Builtin::Contains => Ok(env.arena.boolean(x.contains(y))),
                    Builtin::StrStarts => Ok(env.arena.boolean(x.starts_with(y))),
                    Builtin::StrEnds => Ok(env.arena.boolean(x.ends_with(y))),
                    _ => Ok(match x.find(y) {
                        None => env.arena.string(""),
                        Some(i) => {
                            let (from, to) = if builtin == Builtin::StrBefore { (0, i) } else { (i + y.len(), x.len()) };
                            let sp = env.arena.sub(a.lex, from, to);
                            if y.is_empty() && builtin == Builtin::StrBefore { simple(sp) } else { env.arena.string_like(sp, a) }
                        }
                    }),
                }
            }
            Builtin::EncodeForUri => {
                let l = self.string_of(env, arg(0)?, row, active)?;
                env.scratch.clear();
                env.scratch.push_str(env.arena.text(l.lex));
                let scratch = &env.scratch;
                let lex = env.arena.push_with(|t| {
                    for b in scratch.bytes() {
                        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                            t.push(b as char);
                        } else {
                            write!(t, "%{b:02X}").expect("String write");
                        }
                    }
                });
                Ok(simple(lex))
            }
            Builtin::StrLang => {
                let (s, tag) = (self.string_of(env, arg(0)?, row, active)?, self.string_of(env, arg(1)?, row, active)?);
                if s.lang.is_some() || tag.lang.is_some() || tag.lex.is_empty() {
                    return Err(TypeError);
                }
                env.scratch.clear();
                env.scratch.push_str(env.arena.text(tag.lex));
                let scratch = &env.scratch;
                let lang = env.arena.push_with(|t| t.push_str(&scratch.to_ascii_lowercase()));
                Ok(Value::Lit(Lit { lex: s.lex, lang: Some(lang), dt: value::DT_LANG_STRING, val: Val::LangStr }))
            }
            Builtin::StrDt => {
                let s = self.string_of(env, arg(0)?, row, active)?;
                let Value::Iri(dt) = self.value(env, arg(1)?, row, active)? else { return Err(TypeError) };
                if s.lang.is_some() {
                    return Err(TypeError);
                }
                env.scratch.clear();
                env.scratch.push_str(env.arena.text(s.lex));
                env.scratch.push('\0');
                env.scratch.push_str(env.arena.text(dt));
                let (lex, dt) = env.scratch.split_once('\0').expect("separator pushed");
                let (lex, dt) = (lex.to_owned(), dt.to_owned());
                Ok(env.arena.typed(&lex, &dt))
            }
            Builtin::Year | Builtin::Month | Builtin::Day | Builtin::Hours | Builtin::Minutes | Builtin::Seconds | Builtin::Timezone | Builtin::Tz => {
                let Value::Lit(Lit { val: Val::DateTime(d), .. }) = self.value(env, arg(0)?, row, active)? else { return Err(TypeError) };
                match builtin {
                    Builtin::Year => Ok(env.arena.number(Num::Int(d.year))),
                    Builtin::Month => Ok(env.arena.number(Num::Int(d.month.into()))),
                    Builtin::Day => Ok(env.arena.number(Num::Int(d.day.into()))),
                    Builtin::Hours => Ok(env.arena.number(Num::Int(d.hour.into()))),
                    Builtin::Minutes => Ok(env.arena.number(Num::Int(d.minute.into()))),
                    Builtin::Seconds => Ok(env.arena.number(Num::Dec(d.second))),
                    Builtin::Tz => Ok(match d.tz {
                        None => env.arena.string(""),
                        Some(0) => env.arena.string("Z"),
                        Some(m) => {
                            let lex = env.arena.push_with(|t| write!(t, "{}{:02}:{:02}", if m < 0 { '-' } else { '+' }, m.abs() / 60, m.abs() % 60).expect("String write"));
                            simple(lex)
                        }
                    }),
                    _ => {
                        let m = d.tz.ok_or(TypeError)?;
                        let lex = env.arena.push_with(|t| {
                            if m < 0 {
                                t.push('-');
                            }
                            t.push_str("PT");
                            let (h, mm) = (m.abs() / 60, m.abs() % 60);
                            if h > 0 {
                                write!(t, "{h}H").expect("String write");
                            }
                            if mm > 0 {
                                write!(t, "{mm}M").expect("String write");
                            }
                            if m == 0 {
                                t.push_str("0S");
                            }
                        });
                        Ok(Value::Lit(Lit { lex, lang: None, dt: DT_DAY_TIME_DURATION, val: Val::Other }))
                    }
                }
            }
            Builtin::Now => {
                let now = env.now.clone();
                Ok(env.arena.typed(&now, crate::term::XSD_DATE_TIME))
            }
            Builtin::Uuid | Builtin::StrUuid => {
                let (a, b) = (env.next_random(), env.next_random());
                let b = (b & !0xF000_0000_0000_0000 & 0xFFFF_FFFF_FFFF_3FFF) | 0x8000_0000_0000_0000;
                let a = (a & 0xFFFF_FFFF_FFFF_0FFF) | 0x4000;
                let text = format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", a >> 32, (a >> 16) & 0xFFFF, a & 0xFFFF, b >> 48, b & 0xFFFF_FFFF_FFFF);
                Ok(if builtin == Builtin::Uuid { Value::Iri(env.arena.push_with(|t| write!(t, "urn:uuid:{text}").expect("String write"))) } else { env.arena.string(&text) })
            }
            Builtin::Regex | Builtin::Replace => {
                let text = self.string_of(env, arg(0)?, row, active)?;
                let pattern = self.string_of(env, arg(1)?, row, active)?;
                let (replacement, flags_at) = if builtin == Builtin::Replace { (Some(self.string_of(env, arg(2)?, row, active)?), 3) } else { (None, 2) };
                let flags = match args.get(flags_at) {
                    Some(&f) => Some(self.string_of(env, f, row, active)?),
                    None => None,
                };
                let flags = flags.map(|f| env.arena.text(f.lex).to_owned()).unwrap_or_default();
                let pattern = env.arena.text(pattern.lex).to_owned();
                let regex = regex_for(env, &pattern, &flags).ok_or(TypeError)?;
                match replacement {
                    None => Ok(env.arena.boolean(regex.is_match(env.arena.text(text.lex)))),
                    Some(r) => {
                        let replaced = regex.replace_all(env.arena.text(text.lex), env.arena.text(r.lex)).into_owned();
                        let lex = env.arena.push(&replaced);
                        Ok(env.arena.string_like(lex, text))
                    }
                }
            }
            Builtin::Md5 | Builtin::Sha1 | Builtin::Sha256 | Builtin::Sha384 | Builtin::Sha512 => Err(TypeError),
        }
    }
}

fn regex_for(env: &mut Env, pattern: &str, flags: &str) -> Option<Rc<regex::Regex>> {
    let key = (pattern.to_owned(), flags.to_owned());
    if let Some(r) = env.regexes.get(&key) {
        return r.clone();
    }
    let mut builder = if flags.contains('q') { regex::RegexBuilder::new(&regex::escape(pattern)) } else { regex::RegexBuilder::new(pattern) };
    for f in flags.chars() {
        match f {
            'i' => builder.case_insensitive(true),
            's' => builder.dot_matches_new_line(true),
            'm' => builder.multi_line(true),
            'x' => builder.ignore_whitespace(true),
            'q' => &mut builder,
            _ => return None,
        };
    }
    let compiled = builder.build().ok().map(Rc::new);
    env.regexes.insert(key, compiled.clone());
    compiled
}

fn cast_value(arena: &mut Arena, cast: Cast, v: Value) -> R {
    let lit = match v {
        Value::Iri(sp) if cast == Cast::String => return Ok(simple(sp)),
        Value::Lit(l) if l.lang.is_none() => l,
        _ => return Err(TypeError),
    };
    let text = arena.text(lit.lex).trim().to_owned();
    let from_string = matches!(lit.val, Val::Str);
    match cast {
        Cast::String => Ok(simple(lit.lex)),
        Cast::Boolean => {
            let b = match lit.val {
                Val::Bool(b) => b,
                Val::Int(i) => i != 0,
                Val::Dec(f) | Val::Float(f) | Val::Double(f) => f != 0.0 && !f.is_nan(),
                Val::Str => match text.as_str() {
                    "true" | "1" => true,
                    "false" | "0" => false,
                    _ => return Err(TypeError),
                },
                _ => return Err(TypeError),
            };
            Ok(arena.boolean(b))
        }
        Cast::Integer => {
            let i = match lit.val {
                Val::Int(i) => i,
                Val::Dec(f) | Val::Float(f) | Val::Double(f) if f.is_finite() => f.trunc() as i64,
                Val::Bool(b) => i64::from(b),
                Val::Str => value::parse_integer(&text).ok_or(TypeError)?,
                _ => return Err(TypeError),
            };
            Ok(arena.number(Num::Int(i)))
        }
        Cast::Decimal => {
            let f = match lit.val {
                Val::Int(i) => i as f64,
                Val::Dec(f) | Val::Float(f) | Val::Double(f) if f.is_finite() => f,
                Val::Bool(b) => f64::from(u8::from(b)),
                Val::Str => value::parse_decimal(&text).ok_or(TypeError)?,
                _ => return Err(TypeError),
            };
            Ok(arena.number(Num::Dec(f)))
        }
        Cast::Float | Cast::Double => {
            let f = match lit.val {
                Val::Int(i) => i as f64,
                Val::Dec(f) | Val::Float(f) | Val::Double(f) => f,
                Val::Bool(b) => f64::from(u8::from(b)),
                Val::Str => value::parse_double(&text).ok_or(TypeError)?,
                _ => return Err(TypeError),
            };
            Ok(arena.number(if cast == Cast::Float { Num::Float(f as f32 as f64) } else { Num::Double(f) }))
        }
        Cast::DateTime => match lit.val {
            Val::DateTime(_) => Ok(Value::Lit(lit)),
            Val::Str if from_string => {
                let v = arena.typed(&text, crate::term::XSD_DATE_TIME);
                if matches!(v, Value::Lit(Lit { val: Val::DateTime(_), .. })) { Ok(v) } else { Err(TypeError) }
            }
            _ => Err(TypeError),
        },
    }
}

