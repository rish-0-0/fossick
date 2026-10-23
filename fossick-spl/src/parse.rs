//! SPL parsing: pipeline splitting, the base-search grammar (bare terms,
//! field=value globs, boolean logic), the eval/where expression grammar, and
//! per-command argument parsing.

use crate::exec::{Agg, AggFunc, Cmd};
use crate::expr::{fn_arity, ArithOp, CmpOp, Expr};
use crate::value::Value;
use regex::Regex;

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    Str(String),    // "double-quoted"
    FieldQ(String), // 'single-quoted' (eval field names)
    Num(f64),
    Sym(&'static str),
}

impl Tok {
    fn is_word(&self, kw: &str) -> bool {
        matches!(self, Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }
}

/// Split a query on top-level `|` (quotes respected).
pub fn split_pipes(spl: &str) -> Vec<String> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut esc = false;
    for c in spl.chars() {
        if esc {
            cur.push(c);
            esc = false;
            continue;
        }
        match c {
            '\\' if in_q => {
                cur.push(c);
                esc = true;
            }
            '"' => {
                cur.push(c);
                in_q = !in_q;
            }
            '|' if !in_q => {
                segs.push(std::mem::take(&mut cur));
            }
            c => cur.push(c),
        }
    }
    segs.push(cur);
    segs
}

fn read_quoted(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<String, String> {
    let mut s = String::new();
    loop {
        match chars.next() {
            None => return Err("unterminated string".into()),
            Some('"') => return Ok(s),
            Some('\\') => match chars.next() {
                Some('n') => s.push('\n'),
                Some('t') => s.push('\t'),
                Some('r') => s.push('\r'),
                Some(c) => s.push(c),
                None => return Err("unterminated string".into()),
            },
            Some(c) => s.push(c),
        }
    }
}

/// Quoted string for command arguments (regexes, values): backslashes pass
/// through untouched except `\"`, so `"user=(?<u>\w+)"` keeps its `\w`.
fn read_quoted_raw(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<String, String> {
    let mut s = String::new();
    loop {
        match chars.next() {
            None => return Err("unterminated string".into()),
            Some('"') => return Ok(s),
            Some('\\') if chars.peek() == Some(&'"') => {
                chars.next();
                s.push('"');
            }
            Some(c) => s.push(c),
        }
    }
}

/// Search-context lexer: quoted phrases, the comparison symbols, parens, and
/// barewords (which may contain `*`, `.`, `:`, `/`, `-`, …).
fn lex_search(s: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '"' => {
                chars.next();
                toks.push(Tok::Str(read_quoted(&mut chars)?));
            }
            '(' | ')' | ',' => {
                chars.next();
                toks.push(Tok::Sym(match c {
                    '(' => "(",
                    ')' => ")",
                    _ => ",",
                }));
            }
            '=' => {
                chars.next();
                toks.push(Tok::Sym("="));
            }
            '<' | '>' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    toks.push(Tok::Sym(if c == '<' { "<=" } else { ">=" }));
                } else {
                    toks.push(Tok::Sym(if c == '<' { "<" } else { ">" }));
                }
            }
            '!' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    toks.push(Tok::Sym("!="));
                } else {
                    return Err("stray '!' (use NOT or !=)".into());
                }
            }
            _ => {
                let mut w = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || "()=<>,\"".contains(c) || c == '!' {
                        break;
                    }
                    w.push(c);
                    chars.next();
                }
                toks.push(Tok::Word(w));
            }
        }
    }
    Ok(toks)
}

/// Eval-context lexer: numbers, identifiers, "strings", 'field names', operators.
fn lex_eval(s: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '"' => {
                chars.next();
                toks.push(Tok::Str(read_quoted(&mut chars)?));
            }
            '\'' => {
                chars.next();
                let mut f = String::new();
                loop {
                    match chars.next() {
                        None => return Err("unterminated 'field name'".into()),
                        Some('\'') => break,
                        Some(c) => f.push(c),
                    }
                }
                toks.push(Tok::FieldQ(f));
            }
            '0'..='9' => {
                let mut n = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_digit() {
                        n.push(c);
                        chars.next();
                    } else if c == '.' {
                        // Only consume the dot for a decimal, not concat: 1.5 vs 1 . x
                        let mut ahead = chars.clone();
                        ahead.next();
                        if ahead.peek().is_some_and(char::is_ascii_digit) {
                            n.push('.');
                            chars.next();
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                toks.push(Tok::Num(n.parse().map_err(|_| format!("bad number: {n}"))?));
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut w = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_alphanumeric() || c == '_' {
                        w.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                toks.push(Tok::Word(w));
            }
            _ => {
                chars.next();
                let two = |chars: &mut std::iter::Peekable<std::str::Chars>, tok| {
                    chars.next();
                    tok
                };
                let sym = match c {
                    '(' => "(",
                    ')' => ")",
                    ',' => ",",
                    '+' => "+",
                    '-' => "-",
                    '*' => "*",
                    '/' => "/",
                    '%' => "%",
                    '.' => ".",
                    '=' if chars.peek() == Some(&'=') => two(&mut chars, "=="),
                    '=' => "=",
                    '!' if chars.peek() == Some(&'=') => two(&mut chars, "!="),
                    '<' if chars.peek() == Some(&'=') => two(&mut chars, "<="),
                    '<' => "<",
                    '>' if chars.peek() == Some(&'=') => two(&mut chars, ">="),
                    '>' => ">",
                    c => return Err(format!("unexpected character: {c}")),
                };
                toks.push(Tok::Sym(sym));
            }
        }
    }
    Ok(toks)
}

struct P {
    toks: Vec<Tok>,
    pos: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn eat_sym(&mut self, s: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Sym(x)) if *x == s) {
            self.pos += 1;
            return true;
        }
        false
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.peek().is_some_and(|t| t.is_word(kw)) {
            self.pos += 1;
            return true;
        }
        false
    }
    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }
}

/// Time bounds pulled out of a base search (`earliest=`/`latest=`); values are
/// raw timespec strings resolved by the caller (this crate has no clock).
#[derive(Debug, Default, PartialEq)]
pub struct TimeBounds {
    pub earliest: Option<String>,
    pub latest: Option<String>,
}

// ---------- search grammar ----------

pub fn parse_search(seg: &str, allow_time: bool) -> Result<(Expr, TimeBounds), String> {
    let mut p = P { toks: lex_search(seg)?, pos: 0 };
    let mut tb = TimeBounds::default();
    if p.at_end() {
        return Ok((Expr::Lit(Value::Bool(true)), tb));
    }
    let e = search_or(&mut p, &mut tb, allow_time)?;
    if !p.at_end() {
        return Err(format!("unexpected token after search expression: {:?}", p.peek().unwrap()));
    }
    Ok((e, tb))
}

fn search_or(p: &mut P, tb: &mut TimeBounds, at: bool) -> Result<Expr, String> {
    let mut e = search_and(p, tb, at)?;
    while p.eat_kw("OR") {
        let r = search_and(p, tb, at)?;
        e = Expr::Or(Box::new(e), Box::new(r));
    }
    Ok(e)
}

fn search_and(p: &mut P, tb: &mut TimeBounds, at: bool) -> Result<Expr, String> {
    let mut e = search_unary(p, tb, at)?;
    loop {
        // Implicit AND: keep going until OR, ')' or end.
        match p.peek() {
            None | Some(Tok::Sym(")")) => break,
            Some(t) if t.is_word("OR") => break,
            _ => {}
        }
        p.eat_kw("AND");
        if p.at_end() || p.peek() == Some(&Tok::Sym(")")) {
            return Err("dangling AND".into());
        }
        let r = search_unary(p, tb, at)?;
        e = Expr::And(Box::new(e), Box::new(r));
    }
    Ok(e)
}

fn search_unary(p: &mut P, tb: &mut TimeBounds, at: bool) -> Result<Expr, String> {
    if p.eat_kw("NOT") {
        return Ok(Expr::Not(Box::new(search_unary(p, tb, at)?)));
    }
    if p.eat_sym("(") {
        let e = search_or(p, tb, at)?;
        if !p.eat_sym(")") {
            return Err("missing )".into());
        }
        return Ok(e);
    }
    search_atom(p, tb, at)
}

fn search_value(p: &mut P) -> Result<String, String> {
    match p.next() {
        Some(Tok::Word(w)) => Ok(w),
        Some(Tok::Str(s)) => Ok(s),
        t => Err(format!("expected a value, got {t:?}")),
    }
}

fn search_atom(p: &mut P, tb: &mut TimeBounds, allow_time: bool) -> Result<Expr, String> {
    let tok = p.next().ok_or("unexpected end of search expression")?;
    let word = match tok {
        Tok::Str(s) => return Ok(Expr::Term(s)), // quoted phrase
        Tok::Word(w) => w,
        t => return Err(format!("unexpected {t:?} in search expression")),
    };
    match p.peek() {
        Some(Tok::Sym(op @ ("=" | "!=" | "<" | "<=" | ">" | ">="))) => {
            let op = *op;
            p.pos += 1;
            let val = search_value(p)?;
            // Special fields handled by the engine rather than the event data.
            match word.to_ascii_lowercase().as_str() {
                "earliest" | "latest" if op == "=" => {
                    if !allow_time {
                        return Err("earliest/latest are only allowed in the base search".into());
                    }
                    if word.eq_ignore_ascii_case("earliest") {
                        tb.earliest = Some(val);
                    } else {
                        tb.latest = Some(val);
                    }
                    return Ok(Expr::Lit(Value::Bool(true)));
                }
                "index" if op == "=" => return Ok(Expr::Lit(Value::Bool(true))), // single index
                _ => {}
            }
            Ok(match op {
                "=" => Expr::FieldGlob(word, val),
                "!=" => Expr::Not(Box::new(Expr::FieldGlob(word, val))),
                _ => {
                    let cmp = match op {
                        "<" => CmpOp::Lt,
                        "<=" => CmpOp::Le,
                        ">" => CmpOp::Gt,
                        _ => CmpOp::Ge,
                    };
                    Expr::Cmp(cmp, Box::new(Expr::Field(word)), Box::new(Expr::Lit(Value::Str(val))))
                }
            })
        }
        Some(t) if t.is_word("IN") => {
            p.pos += 1;
            if !p.eat_sym("(") {
                return Err("IN needs a parenthesized value list".into());
            }
            let mut e: Option<Expr> = None;
            loop {
                if p.eat_sym(")") {
                    break;
                }
                p.eat_sym(",");
                let v = search_value(p)?;
                let g = Expr::FieldGlob(word.clone(), v);
                e = Some(match e {
                    Some(prev) => Expr::Or(Box::new(prev), Box::new(g)),
                    None => g,
                });
            }
            e.ok_or_else(|| "empty IN list".into())
        }
        _ => Ok(Expr::Term(word)),
    }
}

// ---------- eval/where grammar ----------

pub fn parse_eval_expr(src: &str) -> Result<Expr, String> {
    let mut p = P { toks: lex_eval(src)?, pos: 0 };
    let e = eval_or(&mut p)?;
    if !p.at_end() {
        return Err(format!("unexpected token: {:?}", p.peek().unwrap()));
    }
    Ok(e)
}

fn eval_or(p: &mut P) -> Result<Expr, String> {
    let mut e = eval_and(p)?;
    while p.eat_kw("OR") {
        e = Expr::Or(Box::new(e), Box::new(eval_and(p)?));
    }
    Ok(e)
}

fn eval_and(p: &mut P) -> Result<Expr, String> {
    let mut e = eval_not(p)?;
    while p.eat_kw("AND") {
        e = Expr::And(Box::new(e), Box::new(eval_not(p)?));
    }
    Ok(e)
}

fn eval_not(p: &mut P) -> Result<Expr, String> {
    if p.eat_kw("NOT") {
        return Ok(Expr::Not(Box::new(eval_not(p)?)));
    }
    eval_cmp(p)
}

fn eval_cmp(p: &mut P) -> Result<Expr, String> {
    let l = eval_concat(p)?;
    let op = match p.peek() {
        Some(Tok::Sym(s @ ("=" | "==" | "!=" | "<" | "<=" | ">" | ">="))) => {
            let op = match *s {
                "=" | "==" => CmpOp::Eq,
                "!=" => CmpOp::Ne,
                "<" => CmpOp::Lt,
                "<=" => CmpOp::Le,
                ">" => CmpOp::Gt,
                _ => CmpOp::Ge,
            };
            p.pos += 1;
            op
        }
        Some(t) if t.is_word("LIKE") => {
            p.pos += 1;
            let r = eval_concat(p)?;
            return Ok(Expr::Call("like".into(), vec![l, r]));
        }
        Some(t) if t.is_word("IN") => {
            p.pos += 1;
            if !p.eat_sym("(") {
                return Err("IN needs a parenthesized value list".into());
            }
            let mut args = vec![l];
            loop {
                if p.eat_sym(")") {
                    break;
                }
                p.eat_sym(",");
                args.push(eval_or(p)?);
            }
            if args.len() < 2 {
                return Err("empty IN list".into());
            }
            return Ok(Expr::Call("in".into(), args));
        }
        _ => return Ok(l),
    };
    let r = eval_concat(p)?;
    Ok(Expr::Cmp(op, Box::new(l), Box::new(r)))
}

fn eval_concat(p: &mut P) -> Result<Expr, String> {
    let mut e = eval_add(p)?;
    while p.eat_sym(".") {
        e = Expr::Arith(ArithOp::Concat, Box::new(e), Box::new(eval_add(p)?));
    }
    Ok(e)
}

fn eval_add(p: &mut P) -> Result<Expr, String> {
    let mut e = eval_mul(p)?;
    loop {
        let op = if p.eat_sym("+") {
            ArithOp::Add
        } else if p.eat_sym("-") {
            ArithOp::Sub
        } else {
            return Ok(e);
        };
        e = Expr::Arith(op, Box::new(e), Box::new(eval_mul(p)?));
    }
}

fn eval_mul(p: &mut P) -> Result<Expr, String> {
    let mut e = eval_unary(p)?;
    loop {
        let op = if p.eat_sym("*") {
            ArithOp::Mul
        } else if p.eat_sym("/") {
            ArithOp::Div
        } else if p.eat_sym("%") {
            ArithOp::Mod
        } else {
            return Ok(e);
        };
        e = Expr::Arith(op, Box::new(e), Box::new(eval_unary(p)?));
    }
}

fn eval_unary(p: &mut P) -> Result<Expr, String> {
    if p.eat_sym("-") {
        return Ok(Expr::Neg(Box::new(eval_unary(p)?)));
    }
    eval_primary(p)
}

fn eval_primary(p: &mut P) -> Result<Expr, String> {
    match p.next().ok_or("unexpected end of expression")? {
        Tok::Num(n) => Ok(Expr::Lit(Value::Num(n))),
        Tok::Str(s) => Ok(Expr::Lit(Value::Str(s))),
        Tok::FieldQ(f) => Ok(Expr::Field(f)),
        Tok::Sym("(") => {
            let e = eval_or(p)?;
            if !p.eat_sym(")") {
                return Err("missing )".into());
            }
            Ok(e)
        }
        Tok::Word(w) => {
            if w.eq_ignore_ascii_case("true") {
                return Ok(Expr::Lit(Value::Bool(true)));
            }
            if w.eq_ignore_ascii_case("false") {
                return Ok(Expr::Lit(Value::Bool(false)));
            }
            if p.eat_sym("(") {
                let name = w.to_ascii_lowercase();
                let (min, max) =
                    fn_arity(&name).ok_or_else(|| format!("unknown function: {name}"))?;
                let mut args = Vec::new();
                if !p.eat_sym(")") {
                    loop {
                        args.push(eval_or(p)?);
                        if p.eat_sym(")") {
                            break;
                        }
                        if !p.eat_sym(",") {
                            return Err(format!("expected , or ) in {name}()"));
                        }
                    }
                }
                if args.len() < min || args.len() > max {
                    let want = if max == usize::MAX {
                        format!("{min}+")
                    } else if max > min {
                        format!("{min}..{max}")
                    } else {
                        format!("{min}")
                    };
                    return Err(format!("{name}() takes {want} arguments, got {}", args.len()));
                }
                return Ok(Expr::Call(name, args));
            }
            Ok(Expr::Field(w))
        }
        t => Err(format!("unexpected {t:?}")),
    }
}

// ---------- command parsing ----------

/// Shell-ish word splitter: whitespace/comma separated, double quotes group
/// (also mid-token, so `field="a b"` is one word).
fn words(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() || c == ',' {
            chars.next();
            continue;
        }
        let mut w = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() || c == ',' {
                break;
            }
            if c == '"' {
                chars.next();
                w.push_str(&read_quoted_raw(&mut chars)?);
            } else {
                w.push(c);
                chars.next();
            }
        }
        out.push(w);
    }
    Ok(out)
}

pub fn parse_command(seg: &str) -> Result<Cmd, String> {
    let seg = seg.trim();
    let (name, rest) = seg.split_once(char::is_whitespace).unwrap_or((seg, ""));
    let rest = rest.trim();
    let err_args = |c: &str| format!("{c}: bad arguments: {rest}");
    match name.to_ascii_lowercase().as_str() {
        "" => Err("empty command".into()),
        "search" => Ok(Cmd::Where(parse_search(rest, false)?.0)),
        "where" => Ok(Cmd::Where(parse_eval_expr(rest)?)),
        "eval" => parse_eval_cmd(rest),
        "fields" => {
            let mut w = words(rest)?;
            if w.is_empty() {
                return Err("fields: needs a field list".into());
            }
            let mut remove = false;
            match w[0].as_str() {
                "-" => {
                    remove = true;
                    w.remove(0);
                }
                "+" => {
                    w.remove(0);
                }
                _ => {}
            }
            if w.is_empty() {
                return Err("fields: needs a field list".into());
            }
            Ok(if remove { Cmd::FieldsRemove(w) } else { Cmd::FieldsKeep(w) })
        }
        "table" => {
            let w = words(rest)?;
            if w.is_empty() {
                return Err("table: needs a field list".into());
            }
            Ok(Cmd::Table(w))
        }
        "rename" => {
            let w = words(rest)?;
            if w.is_empty() || w.len() % 3 != 0 {
                return Err("rename: expected <field> AS <field> [, ...]".into());
            }
            let mut pairs = Vec::new();
            for c in w.chunks(3) {
                if !c[1].eq_ignore_ascii_case("as") {
                    return Err("rename: expected <field> AS <field>".into());
                }
                pairs.push((c[0].clone(), c[2].clone()));
            }
            Ok(Cmd::Rename(pairs))
        }
        "sort" => {
            let w = words(rest)?;
            let mut limit = 0usize;
            let mut keys: Vec<(String, bool)> = Vec::new();
            let mut i = 0;
            if let Some(n) = w.first().and_then(|t| t.parse::<usize>().ok()) {
                limit = n;
                i = 1;
            }
            let mut pending_desc: Option<bool> = None;
            while i < w.len() {
                let t = &w[i];
                i += 1;
                if t == "-" || t == "+" {
                    pending_desc = Some(t == "-");
                    continue;
                }
                let (field, desc) = match t.strip_prefix('-') {
                    Some(f) => (f, true),
                    None => (t.strip_prefix('+').unwrap_or(t), t.starts_with('-')),
                };
                keys.push((field.to_string(), pending_desc.take().unwrap_or(desc)));
            }
            if keys.is_empty() {
                return Err("sort: needs at least one field".into());
            }
            Ok(Cmd::Sort { keys, limit })
        }
        "head" | "tail" => {
            let n = parse_count(rest, 10)?;
            Ok(if name.eq_ignore_ascii_case("head") { Cmd::Head(n) } else { Cmd::Tail(n) })
        }
        "reverse" => {
            if !rest.is_empty() {
                return Err("reverse takes no arguments".into());
            }
            Ok(Cmd::Reverse)
        }
        "dedup" => {
            let mut w = words(rest)?;
            let mut keep = 1usize;
            if let Some(n) = w.first().and_then(|t| t.parse::<usize>().ok()) {
                keep = n.max(1);
                w.remove(0);
            }
            if w.is_empty() {
                return Err("dedup: needs a field list".into());
            }
            Ok(Cmd::Dedup { keep, keys: w })
        }
        "stats" => parse_stats(rest),
        "top" | "rare" => parse_top(rest, name.eq_ignore_ascii_case("rare")).map_err(|e| format!("{name}: {e}")),
        "rex" => {
            let w = words(rest)?;
            let mut field = "_raw".to_string();
            let mut pattern = None;
            for t in w {
                if let Some(f) = t.strip_prefix("field=") {
                    field = f.to_string();
                } else if pattern.is_none() {
                    pattern = Some(t);
                } else {
                    return Err(err_args("rex"));
                }
            }
            let pat = pattern.ok_or("rex: needs a regex")?;
            let re = Regex::new(&pat).map_err(|e| format!("rex: bad regex: {e}"))?;
            if re.capture_names().flatten().next().is_none() {
                return Err("rex: regex has no named capture groups (?<name>...)".into());
            }
            Ok(Cmd::Rex { field, re })
        }
        "regex" => {
            // regex <field>=<pat> | <field>!=<pat> | <pat>   (pattern may be quoted)
            let (field, negate, pat) = match rest.split_once("!=") {
                Some((f, p)) if is_field_name(f.trim()) => (f.trim().to_string(), true, p.trim()),
                _ => match rest.split_once('=') {
                    Some((f, p)) if is_field_name(f.trim()) => (f.trim().to_string(), false, p.trim()),
                    _ => ("_raw".to_string(), false, rest),
                },
            };
            let pat = unquote(pat)?;
            if pat.is_empty() {
                return Err("regex: needs a pattern".into());
            }
            let re = Regex::new(&pat).map_err(|e| format!("regex: bad regex: {e}"))?;
            Ok(Cmd::RegexFilter { field, re, negate })
        }
        "fillnull" => {
            let w = words(rest)?;
            let mut value = Value::str("0");
            let mut fields = Vec::new();
            for t in w {
                if let Some(v) = t.strip_prefix("value=") {
                    value = Value::str(v);
                } else {
                    fields.push(t);
                }
            }
            Ok(Cmd::Fillnull { value, fields })
        }
        other => Err(format!("unsupported command: {other}")),
    }
}

fn is_field_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

fn unquote(s: &str) -> Result<String, String> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('"') {
        let mut chars = inner.chars().peekable();
        let out = read_quoted_raw(&mut chars)?;
        if chars.next().is_some() {
            return Err(format!("trailing characters after quoted string: {s}"));
        }
        return Ok(out);
    }
    Ok(s.to_string())
}

fn parse_count(rest: &str, default: usize) -> Result<usize, String> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(default);
    }
    let v = rest.strip_prefix("limit=").unwrap_or(rest);
    v.parse::<usize>().map_err(|_| format!("expected a count, got: {rest}"))
}

fn parse_eval_cmd(rest: &str) -> Result<Cmd, String> {
    let mut p = P { toks: lex_eval(rest)?, pos: 0 };
    let mut assigns = Vec::new();
    loop {
        let field = match p.next() {
            Some(Tok::Word(w)) => w,
            Some(Tok::FieldQ(f)) | Some(Tok::Str(f)) => f,
            t => return Err(format!("eval: expected a field name, got {t:?}")),
        };
        if !p.eat_sym("=") {
            return Err(format!("eval: expected = after {field}"));
        }
        assigns.push((field, eval_or(&mut p)?));
        if p.at_end() {
            break;
        }
        if !p.eat_sym(",") {
            return Err(format!("eval: expected , got {:?}", p.peek().unwrap()));
        }
    }
    Ok(Cmd::Eval(assigns))
}

/// Tokenize a stats spec: words, quoted words, and `(`, `)`, `,` as symbols.
fn stats_toks(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '(' | ')' | ',' => {
                chars.next();
                out.push(c.to_string());
            }
            '"' => {
                chars.next();
                out.push(read_quoted(&mut chars)?);
            }
            _ => {
                let mut w = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || "(),\"".contains(c) {
                        break;
                    }
                    w.push(c);
                    chars.next();
                }
                out.push(w);
            }
        }
    }
    Ok(out)
}

fn agg_func(name: &str) -> Option<AggFunc> {
    Some(match name {
        "count" | "c" => AggFunc::Count,
        "dc" | "distinct_count" => AggFunc::Dc,
        "sum" => AggFunc::Sum,
        "avg" | "mean" => AggFunc::Avg,
        "min" => AggFunc::Min,
        "max" => AggFunc::Max,
        "first" => AggFunc::First,
        "last" => AggFunc::Last,
        "values" => AggFunc::Values,
        "list" => AggFunc::List,
        "stdev" => AggFunc::Stdev,
        "median" => AggFunc::Median,
        "range" => AggFunc::Range,
        _ => {
            let digits = name.strip_prefix("perc").or_else(|| name.strip_prefix('p'))?;
            let p: f64 = digits.parse().ok()?;
            (0.0..=100.0).contains(&p).then_some(AggFunc::Perc(p))?
        }
    })
}

fn parse_stats(rest: &str) -> Result<Cmd, String> {
    let toks = stats_toks(rest)?;
    let mut aggs = Vec::new();
    let mut by = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == "," {
            i += 1;
            continue;
        }
        if toks[i].eq_ignore_ascii_case("by") {
            by = toks[i + 1..].iter().filter(|t| *t != ",").cloned().collect();
            if by.is_empty() {
                return Err("stats: empty by clause".into());
            }
            break;
        }
        let fname = toks[i].to_ascii_lowercase();
        let func = agg_func(&fname).ok_or_else(|| format!("stats: unknown function: {}", toks[i]))?;
        i += 1;
        let mut field = None;
        if toks.get(i).map(String::as_str) == Some("(") {
            let arg = toks.get(i + 1).ok_or("stats: unterminated (")?;
            if arg == ")" {
                return Err(format!("stats: {fname}() needs a field"));
            }
            if toks.get(i + 2).map(String::as_str) != Some(")") {
                return Err(format!("stats: {fname}() takes one field"));
            }
            if arg != "*" {
                field = Some(arg.clone());
            }
            i += 3;
        }
        if field.is_none() && func != AggFunc::Count {
            return Err(format!("stats: {fname} needs a field, e.g. {fname}(x)"));
        }
        let mut name = match &field {
            Some(f) => format!("{fname}({f})"),
            None => "count".to_string(),
        };
        if toks.get(i).is_some_and(|t| t.eq_ignore_ascii_case("as")) {
            name = toks.get(i + 1).cloned().ok_or("stats: AS needs a name")?;
            i += 2;
        }
        aggs.push(Agg { func, field, name });
    }
    if aggs.is_empty() {
        return Err("stats: needs at least one aggregation".into());
    }
    Ok(Cmd::Stats { aggs, by })
}

fn parse_top(rest: &str, rare: bool) -> Result<Cmd, String> {
    let mut w = words(rest)?;
    let mut limit = 10usize;
    if let Some(n) = w.first().and_then(|t| t.parse::<usize>().ok()) {
        limit = n;
        w.remove(0);
    }
    let mut fields = Vec::new();
    let mut by = Vec::new();
    let mut in_by = false;
    for t in w {
        if let Some(n) = t.strip_prefix("limit=") {
            limit = n.parse().map_err(|_| format!("bad limit: {n}"))?;
        } else if t.eq_ignore_ascii_case("by") {
            in_by = true;
        } else if in_by {
            by.push(t);
        } else {
            fields.push(t);
        }
    }
    if fields.is_empty() {
        return Err("needs at least one field".into());
    }
    if in_by && by.is_empty() {
        return Err("empty by clause".into());
    }
    Ok(Cmd::Top { limit, fields, by, rare })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_splitting_respects_quotes() {
        assert_eq!(split_pipes(r#"error "a|b" | head 5"#), vec![r#"error "a|b" "#, " head 5"]);
        assert_eq!(split_pipes("a").len(), 1);
    }

    #[test]
    fn search_grammar() {
        let (e, tb) = parse_search("error host=web* earliest=-15m NOT (a OR b)", true).unwrap();
        assert_eq!(tb.earliest.as_deref(), Some("-15m"));
        // Shape: ((((error AND host=web*) AND true) AND NOT (a OR b)))
        let s = format!("{e:?}");
        assert!(s.contains("Term(\"error\")") && s.contains("FieldGlob(\"host\", \"web*\")"), "{s}");
        assert!(s.contains("Or(Term(\"a\")"), "{s}");
    }

    #[test]
    fn search_in_and_comparisons() {
        let (e, _) = parse_search(r#"status IN (500, 503) bytes>1024 msg="a b""#, true).unwrap();
        let s = format!("{e:?}");
        assert!(s.contains("FieldGlob(\"status\", \"500\")") && s.contains("FieldGlob(\"status\", \"503\")"), "{s}");
        assert!(s.contains("Cmp(Gt, Field(\"bytes\")"), "{s}");
        assert!(s.contains("FieldGlob(\"msg\", \"a b\")"), "{s}");
    }

    #[test]
    fn search_time_only_in_base() {
        assert!(parse_search("earliest=-1h", false).is_err());
        assert!(parse_search("x latest=now", true).is_ok());
    }

    #[test]
    fn search_errors() {
        assert!(parse_search("a OR", true).is_err());
        assert!(parse_search("(a", true).is_err());
        assert!(parse_search("a ! b", true).is_err());
    }

    #[test]
    fn eval_grammar_precedence() {
        // 1+2*3 = 7, not 9
        let e = parse_eval_expr("1+2*3").unwrap();
        let s = format!("{e:?}");
        assert!(s.starts_with("Arith(Add"), "{s}");
        // concat binds looser than +
        let e = parse_eval_expr(r#""a" . 1+2"#).unwrap();
        assert!(format!("{e:?}").starts_with("Arith(Concat"), "{e:?}");
        // comparison over arithmetic
        let e = parse_eval_expr("x+1 > 2 AND y=3").unwrap();
        assert!(format!("{e:?}").starts_with("And("), "{e:?}");
    }

    #[test]
    fn eval_functions_validated_at_parse() {
        assert!(parse_eval_expr("nosuchfn(1)").is_err());
        assert!(parse_eval_expr("if(1,2)").is_err()); // arity
        assert!(parse_eval_expr("if(x>1, \"a\", \"b\")").is_ok());
        assert!(parse_eval_expr("'weird field' . lower(host)").is_ok());
    }

    #[test]
    fn eval_number_vs_concat_dot() {
        let e = parse_eval_expr("1.5 + 2").unwrap();
        assert!(format!("{e:?}").contains("Num(1.5)"));
        let e = parse_eval_expr("a.b").unwrap();
        assert!(format!("{e:?}").starts_with("Arith(Concat"), "{e:?}");
    }

    #[test]
    fn command_eval_multi_assign() {
        let Cmd::Eval(assigns) = parse_command("eval kb=bytes/1024, tag=\"x\"").unwrap() else { panic!() };
        assert_eq!(assigns.len(), 2);
        assert_eq!(assigns[0].0, "kb");
        assert_eq!(assigns[1].0, "tag");
    }

    #[test]
    fn command_sort() {
        let Cmd::Sort { keys, limit } = parse_command("sort 100 -count, +host").unwrap() else { panic!() };
        assert_eq!(limit, 100);
        assert_eq!(keys, vec![("count".to_string(), true), ("host".to_string(), false)]);
        // separate sign token
        let Cmd::Sort { keys, .. } = parse_command("sort - count").unwrap() else { panic!() };
        assert_eq!(keys, vec![("count".to_string(), true)]);
    }

    #[test]
    fn command_stats() {
        let Cmd::Stats { aggs, by } = parse_command("stats count, avg(bytes) as ab, p95(t) by host, status").unwrap()
        else {
            panic!()
        };
        assert_eq!(by, vec!["host", "status"]);
        assert_eq!(aggs[0].name, "count");
        assert_eq!(aggs[1].name, "ab");
        assert_eq!(aggs[1].func, AggFunc::Avg);
        assert_eq!(aggs[2].func, AggFunc::Perc(95.0));
        assert!(parse_command("stats").is_err());
        assert!(parse_command("stats avg").is_err()); // needs a field
        assert!(parse_command("stats nope(x)").is_err());
    }

    #[test]
    fn command_top_rare() {
        let Cmd::Top { limit, fields, by, rare } = parse_command("top 5 status by host").unwrap() else { panic!() };
        assert_eq!((limit, rare), (5, false));
        assert_eq!(fields, vec!["status"]);
        assert_eq!(by, vec!["host"]);
        assert!(matches!(parse_command("rare limit=3 x").unwrap(), Cmd::Top { rare: true, limit: 3, .. }));
    }

    #[test]
    fn command_rex_and_regex() {
        let Cmd::Rex { field, re } = parse_command(r#"rex field=msg "user=(?<user>\w+)""#).unwrap() else { panic!() };
        assert_eq!(field, "msg");
        assert!(re.is_match("user=bob"));
        assert!(parse_command(r#"rex "no groups""#).is_err());
        let Cmd::RegexFilter { field, negate, .. } = parse_command(r#"regex _raw!="^DEBUG""#).unwrap() else { panic!() };
        assert_eq!(field, "_raw");
        assert!(negate);
        assert!(matches!(parse_command(r#"regex "^a""#).unwrap(), Cmd::RegexFilter { negate: false, .. }));
    }

    #[test]
    fn command_misc() {
        assert!(matches!(parse_command("head").unwrap(), Cmd::Head(10)));
        assert!(matches!(parse_command("head limit=3").unwrap(), Cmd::Head(3)));
        assert!(matches!(parse_command("dedup 2 host").unwrap(), Cmd::Dedup { keep: 2, .. }));
        assert!(matches!(parse_command("fields - _raw junk*").unwrap(), Cmd::FieldsRemove(_)));
        let Cmd::Rename(p) = parse_command("rename a AS b, c as d").unwrap() else { panic!() };
        assert_eq!(p, vec![("a".to_string(), "b".to_string()), ("c".to_string(), "d".to_string())]);
        let Cmd::Fillnull { value, fields } = parse_command("fillnull value=N/A x y").unwrap() else { panic!() };
        assert_eq!(value, Value::str("N/A"));
        assert_eq!(fields, vec!["x", "y"]);
        assert!(parse_command("makeresults").is_err()); // unsupported
    }
}
