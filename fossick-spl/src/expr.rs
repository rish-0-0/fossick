//! Expression AST shared by base searches, `where`, and `eval`, plus the
//! evaluator and the eval function library.

use crate::value::{cmp_values, fmt_num, glob_match, Record, Value};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Lit(Value),
    Field(String),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    /// Search-context bare term: case-insensitive substring of `_raw`
    /// (glob if it contains `*`).
    Term(String),
    /// Search-context `field=value`: case-insensitive glob on the field value.
    FieldGlob(String, String),
}

pub struct Ctx {
    pub now_ms: u64,
}

thread_local! {
    /// `match`/`replace`/`like` compile their pattern per call; this cache
    /// makes literal patterns (the overwhelmingly common case) compile once.
    static RE_CACHE: RefCell<HashMap<String, Result<regex::Regex, String>>> = RefCell::new(HashMap::new());
}

fn cached_regex<T>(pat: &str, f: impl FnOnce(&regex::Regex) -> T) -> Result<T, String> {
    RE_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let entry =
            c.entry(pat.to_string()).or_insert_with(|| regex::Regex::new(pat).map_err(|e| format!("bad regex: {e}")));
        match entry {
            Ok(re) => Ok(f(re)),
            Err(e) => Err(e.clone()),
        }
    })
}

/// SQL LIKE (`%`, `_`) → anchored regex.
fn like_to_regex(pat: &str) -> String {
    let mut re = String::from("^(?s)");
    for c in pat.chars() {
        match c {
            '%' => re.push_str(".*"),
            '_' => re.push('.'),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    re
}

pub fn eval(e: &Expr, rec: &Record, ctx: &Ctx) -> Result<Value, String> {
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Field(f) => rec.get_or_null(f),
        Expr::Not(x) => Value::Bool(!eval(x, rec, ctx)?.truthy()),
        Expr::Neg(x) => match eval(x, rec, ctx)?.as_num() {
            Some(n) => Value::Num(-n),
            None => Value::Null,
        },
        Expr::And(a, b) => Value::Bool(eval(a, rec, ctx)?.truthy() && eval(b, rec, ctx)?.truthy()),
        Expr::Or(a, b) => Value::Bool(eval(a, rec, ctx)?.truthy() || eval(b, rec, ctx)?.truthy()),
        Expr::Cmp(op, a, b) => {
            let (a, b) = (eval(a, rec, ctx)?, eval(b, rec, ctx)?);
            if a.is_null() || b.is_null() {
                return Ok(Value::Bool(false)); // null never compares true
            }
            let ord = cmp_values(&a, &b);
            Value::Bool(match op {
                CmpOp::Eq => ord == Ordering::Equal,
                CmpOp::Ne => ord != Ordering::Equal,
                CmpOp::Lt => ord == Ordering::Less,
                CmpOp::Le => ord != Ordering::Greater,
                CmpOp::Gt => ord == Ordering::Greater,
                CmpOp::Ge => ord != Ordering::Less,
            })
        }
        Expr::Arith(op, a, b) => {
            let (a, b) = (eval(a, rec, ctx)?, eval(b, rec, ctx)?);
            if *op == ArithOp::Concat {
                let mut s = a.render().unwrap_or_default();
                s.push_str(&b.render().unwrap_or_default());
                return Ok(Value::Str(s));
            }
            match (a.as_num(), b.as_num()) {
                (Some(x), Some(y)) => match op {
                    ArithOp::Add => Value::Num(x + y),
                    ArithOp::Sub => Value::Num(x - y),
                    ArithOp::Mul => Value::Num(x * y),
                    ArithOp::Div if y != 0.0 => Value::Num(x / y),
                    ArithOp::Mod if y != 0.0 => Value::Num(x % y),
                    _ => Value::Null, // divide by zero
                },
                _ => Value::Null,
            }
        }
        Expr::Call(name, args) => call(name, args, rec, ctx)?,
        Expr::Term(t) => {
            let raw = rec.get("_raw").and_then(Value::render).unwrap_or_default();
            Value::Bool(term_match(t, &raw))
        }
        Expr::FieldGlob(f, pat) => match rec.get(f).and_then(Value::render) {
            Some(v) => Value::Bool(glob_match(pat, &v)),
            None => Value::Bool(false),
        },
    })
}

/// Bare search term vs raw text: `*` makes it a glob over the whole line,
/// otherwise a case-insensitive substring test.
pub fn term_match(term: &str, raw: &str) -> bool {
    if term.contains('*') {
        glob_match(&format!("*{term}*"), raw)
    } else {
        contains_ci(raw.as_bytes(), term.as_bytes())
    }
}

fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

/// (min arity, max arity) per function; parse-time validation lives here so
/// typos fail the query instead of silently returning null.
pub fn fn_arity(name: &str) -> Option<(usize, usize)> {
    Some(match name {
        "if" => (3, 3),
        "case" => (2, usize::MAX),
        "coalesce" => (1, usize::MAX),
        "isnull" | "isnotnull" => (1, 1),
        "null" | "now" | "pi" | "random" => (0, 0),
        "len" | "lower" | "upper" | "trim" | "ltrim" | "rtrim" => (1, 1),
        "substr" => (2, 3),
        "replace" => (3, 3),
        "tonumber" => (1, 2),
        "tostring" => (1, 2),
        "typeof" => (1, 1),
        "abs" | "ceil" | "ceiling" | "floor" | "sqrt" | "exp" | "ln" => (1, 1),
        "round" => (1, 2),
        "log" => (1, 2),
        "pow" => (2, 2),
        "min" | "max" => (1, usize::MAX),
        "like" | "match" => (2, 2),
        "in" => (2, usize::MAX),
        _ => return None,
    })
}

fn call(name: &str, args: &[Expr], rec: &Record, ctx: &Ctx) -> Result<Value, String> {
    // Lazy-argument functions first.
    match name {
        "if" => {
            let cond = eval(&args[0], rec, ctx)?.truthy();
            return eval(&args[if cond { 1 } else { 2 }], rec, ctx);
        }
        "case" => {
            for pair in args.chunks(2) {
                match pair {
                    [c, v] => {
                        if eval(c, rec, ctx)?.truthy() {
                            return eval(v, rec, ctx);
                        }
                    }
                    [_] => return Err("case() needs condition/value pairs".into()),
                    _ => unreachable!(),
                }
            }
            return Ok(Value::Null);
        }
        "coalesce" => {
            for a in args {
                let v = eval(a, rec, ctx)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            return Ok(Value::Null);
        }
        _ => {}
    }
    let v: Vec<Value> = args.iter().map(|a| eval(a, rec, ctx)).collect::<Result<_, _>>()?;
    let num = |i: usize| v[i].as_num();
    let s = |i: usize| v[i].render().unwrap_or_default();
    let num_fn = |x: Option<f64>| x.map(Value::Num).unwrap_or(Value::Null);
    Ok(match name {
        "isnull" => Value::Bool(v[0].is_null()),
        "isnotnull" => Value::Bool(!v[0].is_null()),
        "null" => Value::Null,
        "now" => Value::Num((ctx.now_ms / 1000) as f64),
        "pi" => Value::Num(std::f64::consts::PI),
        "random" => Value::Num(((ctx.now_ms ^ ctx.now_ms.rotate_left(31)) % (1 << 31)) as f64),
        "len" => match &v[0] {
            Value::Null => Value::Null,
            x => Value::Num(x.render().unwrap_or_default().chars().count() as f64),
        },
        "lower" => Value::Str(s(0).to_lowercase()),
        "upper" => Value::Str(s(0).to_uppercase()),
        "trim" => Value::Str(s(0).trim().to_string()),
        "ltrim" => Value::Str(s(0).trim_start().to_string()),
        "rtrim" => Value::Str(s(0).trim_end().to_string()),
        "substr" => {
            let text: Vec<char> = s(0).chars().collect();
            let start = num(1).unwrap_or(1.0) as i64;
            // 1-based; negative counts from the end (Splunk semantics).
            let from = if start > 0 { (start - 1) as usize } else { text.len().saturating_sub((-start) as usize) }
                .min(text.len());
            let n = v.get(2).and_then(Value::as_num).map(|n| n.max(0.0) as usize).unwrap_or(usize::MAX);
            Value::Str(text[from..].iter().take(n).collect())
        }
        "replace" => {
            let (text, rep) = (s(0), s(2));
            cached_regex(&s(1), |re| Value::Str(re.replace_all(&text, rep.as_str()).into_owned()))?
        }
        "tonumber" => match v.get(1).and_then(Value::as_num) {
            Some(base) => num_fn(i64::from_str_radix(s(0).trim(), base as u32).ok().map(|n| n as f64)),
            None => num_fn(num(0)),
        },
        "tostring" => match v.get(1).and_then(Value::render).as_deref() {
            Some("hex") => {
                num_fn(num(0)).as_num().map(|n| Value::Str(format!("0x{:x}", n as i64))).unwrap_or(Value::Null)
            }
            Some("commas") => match num(0) {
                Some(n) => Value::Str(group_commas(n)),
                None => Value::Str(s(0)),
            },
            Some("duration") => match num(0) {
                Some(n) => {
                    let t = n.max(0.0) as u64;
                    Value::Str(format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60))
                }
                None => Value::Null,
            },
            _ => match &v[0] {
                Value::Null => Value::Null,
                x => Value::Str(x.render().unwrap_or_default()),
            },
        },
        "typeof" => Value::Str(
            match &v[0] {
                Value::Null => "Invalid",
                Value::Bool(_) => "Boolean",
                Value::Num(_) => "Number",
                Value::Str(x) if x.trim().parse::<f64>().is_ok() => "Number",
                Value::Str(_) => "String",
            }
            .to_string(),
        ),
        "abs" => num_fn(num(0).map(f64::abs)),
        "ceil" | "ceiling" => num_fn(num(0).map(f64::ceil)),
        "floor" => num_fn(num(0).map(f64::floor)),
        "sqrt" => num_fn(num(0).map(f64::sqrt).filter(|n| !n.is_nan())),
        "exp" => num_fn(num(0).map(f64::exp)),
        "ln" => num_fn(num(0).map(f64::ln).filter(|n| n.is_finite())),
        "log" => {
            let base = v.get(1).and_then(Value::as_num).unwrap_or(10.0);
            num_fn(num(0).map(|n| n.log(base)).filter(|n| n.is_finite()))
        }
        "round" => {
            let digits = v.get(1).and_then(Value::as_num).unwrap_or(0.0) as i32;
            let mul = 10f64.powi(digits);
            num_fn(num(0).map(|n| (n * mul).round() / mul))
        }
        "pow" => num_fn(match (num(0), num(1)) {
            (Some(x), Some(y)) => Some(x.powf(y)),
            _ => None,
        }),
        "min" | "max" => {
            let want = if name == "min" { Ordering::Less } else { Ordering::Greater };
            v.iter()
                .filter(|x| !x.is_null())
                .cloned()
                .reduce(|a, b| if cmp_values(&b, &a) == want { b } else { a })
                .unwrap_or(Value::Null)
        }
        "like" => cached_regex(&like_to_regex(&s(1)), |re| Value::Bool(re.is_match(&s(0))))?,
        "match" => cached_regex(&s(1), |re| Value::Bool(re.is_match(&s(0))))?,
        "in" => {
            let needle = &v[0];
            Value::Bool(v[1..].iter().any(|x| cmp_values(needle, x) == Ordering::Equal))
        }
        _ => return Err(format!("unknown function: {name}")),
    })
}

/// 1234567.5 → "1,234,567.5"
fn group_commas(n: f64) -> String {
    let raw = fmt_num(n);
    let (sign, rest) = raw.strip_prefix('-').map(|r| ("-", r)).unwrap_or(("", &raw));
    let (int, frac) = rest.split_once('.').map(|(i, f)| (i, Some(f))).unwrap_or((rest, None));
    let mut out = String::from(sign);
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTX: Ctx = Ctx { now_ms: 1_789_473_600_000 };

    fn rec(pairs: &[(&str, Value)]) -> Record {
        Record(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    fn ev(e: &Expr, r: &Record) -> Value {
        eval(e, r, &CTX).unwrap()
    }

    #[test]
    fn comparisons_are_numeric_when_possible() {
        let r = rec(&[("n", Value::str("9")), ("m", Value::str("10"))]);
        let lt = Expr::Cmp(CmpOp::Lt, Box::new(Expr::Field("n".into())), Box::new(Expr::Field("m".into())));
        assert_eq!(ev(&lt, &r), Value::Bool(true));
        // Null never compares true.
        let vs_null =
            Expr::Cmp(CmpOp::Ne, Box::new(Expr::Field("missing".into())), Box::new(Expr::Lit(Value::Num(1.0))));
        assert_eq!(ev(&vs_null, &r), Value::Bool(false));
    }

    #[test]
    fn arithmetic_and_concat() {
        let r = rec(&[("a", Value::Num(7.0)), ("b", Value::str("3"))]);
        let div = Expr::Arith(ArithOp::Div, Box::new(Expr::Field("a".into())), Box::new(Expr::Field("b".into())));
        assert_eq!(ev(&div, &r), Value::Num(7.0 / 3.0));
        let by_zero =
            Expr::Arith(ArithOp::Div, Box::new(Expr::Field("a".into())), Box::new(Expr::Lit(Value::Num(0.0))));
        assert_eq!(ev(&by_zero, &r), Value::Null);
        let cat = Expr::Arith(ArithOp::Concat, Box::new(Expr::Field("a".into())), Box::new(Expr::Lit(Value::str("x"))));
        assert_eq!(ev(&cat, &r), Value::str("7x"));
    }

    #[test]
    fn term_matching() {
        assert!(term_match("error", "big ERROR here"));
        assert!(!term_match("error", "all fine"));
        assert!(term_match("err*here", "big ERROR here"));
        assert!(!term_match("", "anything")); // empty term matches nothing
    }

    #[test]
    fn field_glob() {
        let r = rec(&[("host", Value::str("WEB01"))]);
        assert_eq!(ev(&Expr::FieldGlob("host".into(), "web*".into()), &r), Value::Bool(true));
        assert_eq!(ev(&Expr::FieldGlob("host".into(), "web01".into()), &r), Value::Bool(true));
        assert_eq!(ev(&Expr::FieldGlob("nope".into(), "*".into()), &r), Value::Bool(false));
    }

    #[test]
    fn functions() {
        let r = rec(&[("x", Value::str("hello")), ("n", Value::str("2.6"))]);
        let f = |name: &str, args: Vec<Expr>| ev(&Expr::Call(name.into(), args), &r);
        let fld = |n: &str| Expr::Field(n.into());
        let lit = |v: Value| Expr::Lit(v);
        assert_eq!(f("len", vec![fld("x")]), Value::Num(5.0));
        assert_eq!(f("upper", vec![fld("x")]), Value::str("HELLO"));
        assert_eq!(f("round", vec![fld("n")]), Value::Num(3.0));
        assert_eq!(f("round", vec![lit(Value::Num(2.567)), lit(Value::Num(2.0))]), Value::Num(2.57));
        assert_eq!(f("substr", vec![fld("x"), lit(Value::Num(2.0)), lit(Value::Num(3.0))]), Value::str("ell"));
        assert_eq!(f("substr", vec![fld("x"), lit(Value::Num(-3.0))]), Value::str("llo"));
        assert_eq!(f("coalesce", vec![fld("missing"), fld("x")]), Value::str("hello"));
        assert_eq!(f("isnull", vec![fld("missing")]), Value::Bool(true));
        assert_eq!(f("if", vec![lit(Value::Bool(false)), lit(Value::str("a")), lit(Value::str("b"))]), Value::str("b"));
        assert_eq!(f("like", vec![fld("x"), lit(Value::str("hel%"))]), Value::Bool(true));
        assert_eq!(f("match", vec![fld("x"), lit(Value::str("^h.*o$"))]), Value::Bool(true));
        assert_eq!(f("tonumber", vec![lit(Value::str("ff")), lit(Value::Num(16.0))]), Value::Num(255.0));
        assert_eq!(f("tostring", vec![lit(Value::Num(1234567.0)), lit(Value::str("commas"))]), Value::str("1,234,567"));
        assert_eq!(f("tostring", vec![lit(Value::Num(3661.0)), lit(Value::str("duration"))]), Value::str("01:01:01"));
        assert_eq!(f("in", vec![fld("x"), lit(Value::str("hi")), lit(Value::str("hello"))]), Value::Bool(true));
        assert_eq!(f("min", vec![lit(Value::str("9")), lit(Value::str("10"))]), Value::str("9"));
        assert_eq!(
            f(
                "case",
                vec![lit(Value::Bool(false)), lit(Value::str("a")), lit(Value::Bool(true)), lit(Value::str("b"))]
            ),
            Value::str("b")
        );
        assert_eq!(f("now", vec![]), Value::Num(1_789_473_600.0));
        assert_eq!(f("typeof", vec![fld("n")]), Value::str("Number"));
    }

    #[test]
    fn bad_regex_is_an_error() {
        let r = Record::default();
        let e = Expr::Call("match".into(), vec![Expr::Lit(Value::str("x")), Expr::Lit(Value::str("["))]);
        assert!(eval(&e, &r, &CTX).is_err());
    }

    #[test]
    fn commas_grouping() {
        assert_eq!(group_commas(1234567.5), "1,234,567.5");
        assert_eq!(group_commas(-1000.0), "-1,000");
        assert_eq!(group_commas(42.0), "42");
    }
}
