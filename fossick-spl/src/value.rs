//! Dynamically typed SPL values and the per-event record they live in.
//! Splunk semantics: everything is stringly typed at the edges; numbers
//! compare numerically whenever both sides coerce, strings otherwise.

use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }

    /// Numeric coercion: numbers, numeric strings, bools (1/0). Null → None.
    pub fn as_num(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            Value::Str(s) => s.trim().parse().ok(),
            Value::Bool(b) => Some(*b as u8 as f64),
            Value::Null => None,
        }
    }

    /// String rendering; None means null (absent field).
    pub fn render(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(b.to_string()),
            Value::Num(n) => Some(fmt_num(*n)),
            Value::Str(s) => Some(s.clone()),
        }
    }

    /// Truthiness for `where`/`if`: false, 0, "", and null are false.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0,
            Value::Str(s) => !s.is_empty() && s != "false" && s != "0",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// Integers print without a trailing `.0`; everything else is shortest-roundtrip.
pub fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Total order used by `sort`, `min`/`max`, and group keys: numeric when both
/// sides coerce, else lexicographic. Nulls sort first.
pub fn cmp_values(a: &Value, b: &Value) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    match (a.as_num(), b.as_num()) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => a.render().unwrap_or_default().cmp(&b.render().unwrap_or_default()),
    }
}

/// One event/result row: ordered field → value pairs. Insertion order is
/// preserved because it drives output column order. Linear lookup is fine at
/// the field counts logs have.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Record(pub Vec<(String, Value)>);

impl Record {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// Missing and explicit-null both read as Null.
    pub fn get_or_null(&self, name: &str) -> Value {
        self.get(name).cloned().unwrap_or(Value::Null)
    }

    pub fn set(&mut self, name: &str, v: Value) {
        match self.0.iter_mut().find(|(k, _)| k == name) {
            Some((_, slot)) => *slot = v,
            None => self.0.push((name.to_string(), v)),
        }
    }

    pub fn remove(&mut self, name: &str) {
        self.0.retain(|(k, _)| k != name);
    }
}

/// Case-insensitive glob match; `*` matches any run of characters.
/// Classic two-pointer backtracking — no allocation beyond lowercasing bytes.
pub fn glob_match(pat: &str, text: &str) -> bool {
    let (p, t) = (pat.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (usize::MAX, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'*' || p[pi].eq_ignore_ascii_case(&t[ti])) {
            if p[pi] == b'*' {
                star = pi;
                mark = ti;
                pi += 1;
            } else {
                pi += 1;
                ti += 1;
            }
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_coercion() {
        assert_eq!(Value::str("42").as_num(), Some(42.0));
        assert_eq!(Value::str(" 4.5 ").as_num(), Some(4.5));
        assert_eq!(Value::str("abc").as_num(), None);
        assert_eq!(Value::Null.as_num(), None);
        assert_eq!(Value::Bool(true).as_num(), Some(1.0));
    }

    #[test]
    fn number_formatting() {
        assert_eq!(fmt_num(3.0), "3");
        assert_eq!(fmt_num(-7.0), "-7");
        assert_eq!(fmt_num(2.5), "2.5");
        assert_eq!(fmt_num(0.1 + 0.2), "0.30000000000000004"); // honest roundtrip
    }

    #[test]
    fn value_ordering() {
        // Numeric when both coerce: "9" < "10".
        assert_eq!(cmp_values(&Value::str("9"), &Value::str("10")), Ordering::Less);
        // Lexicographic otherwise.
        assert_eq!(cmp_values(&Value::str("apple"), &Value::str("banana")), Ordering::Less);
        // Nulls first.
        assert_eq!(cmp_values(&Value::Null, &Value::Num(0.0)), Ordering::Less);
    }

    #[test]
    fn record_ops() {
        let mut r = Record::default();
        r.set("a", Value::Num(1.0));
        r.set("b", Value::str("x"));
        r.set("a", Value::Num(2.0)); // overwrite keeps position
        assert_eq!(r.get("a"), Some(&Value::Num(2.0)));
        assert_eq!(r.0[0].0, "a");
        r.remove("a");
        assert_eq!(r.get("a"), None);
        assert_eq!(r.get_or_null("a"), Value::Null);
    }

    #[test]
    fn globbing() {
        assert!(glob_match("web*", "WEB01"));
        assert!(glob_match("*error*", "an ERROR here"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b", "ac"));
        assert!(glob_match("*", ""));
        assert!(glob_match("exact", "EXACT"));
        assert!(!glob_match("exact", "exactly"));
    }
}
