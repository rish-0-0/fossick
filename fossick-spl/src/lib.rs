//! fossick-spl: SPL lexer, parser, and push-based executor.
//!
//! Supported surface:
//! - Base search: bare terms (case-insensitive substrings of `_raw`, `*`
//!   globs), quoted phrases, `field=value` globs (case-insensitive),
//!   `field!=v`, `< <= > >=`, `field IN (a, b)`, `AND`/`OR`/`NOT` with
//!   implicit AND, parentheses, `earliest=`/`latest=` (values resolved by the
//!   caller), `index=` (accepted, ignored — fossick has one index).
//! - Commands: `search`, `where`, `eval`, `fields`, `table`, `rename`,
//!   `sort`, `head`, `tail`, `reverse`, `dedup`, `stats`, `top`, `rare`,
//!   `rex`, `regex`, `fillnull`.
//! - Eval functions: see `expr::fn_arity`.
//!
//! The crate is engine-agnostic: the caller feeds `Record`s into an
//! `Executor` and gets a `Table` back. `Pipeline::pushdown` exposes the
//! conjunctive terms/field-equalities an index may safely use to prune input
//! (the executor re-verifies everything, so over-delivery is fine but
//! under-delivery is not).

mod exec;
mod expr;
mod parse;
mod value;

pub use exec::{render_cell, Agg, AggFunc, Cmd, Executor, Limits, Table};
pub use expr::{Ctx, Expr};
pub use value::{fmt_num, Record, Value};

/// Index-safe filters extracted from the base search: only positive,
/// glob-free conjuncts qualify.
#[derive(Debug, Default, PartialEq)]
pub struct Pushdown {
    /// Lowercased bare terms that MUST appear in `_raw`.
    pub terms: Vec<String>,
    /// (field, value) equalities that MUST hold (match case-insensitively).
    pub fields: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct Pipeline {
    pub base: Expr,
    /// Raw `earliest=` / `latest=` timespec strings, if given in the search.
    pub earliest: Option<String>,
    pub latest: Option<String>,
    pub cmds: Vec<Cmd>,
    pub pushdown: Pushdown,
}

impl Pipeline {
    pub fn executor(&self, limits: Limits, now_ms: u64) -> Executor {
        Executor::new(self.base.clone(), &self.cmds, limits, now_ms)
    }
}

pub fn parse(spl: &str) -> Result<Pipeline, String> {
    let segs = parse::split_pipes(spl);
    let base_seg = segs[0].trim();
    // A leading `search` keyword is the command name, not a term.
    let base_seg = base_seg
        .split_once(char::is_whitespace)
        .filter(|(w, _)| w.eq_ignore_ascii_case("search"))
        .map(|(_, rest)| rest)
        .unwrap_or(if base_seg.eq_ignore_ascii_case("search") { "" } else { base_seg });
    let (base, tb) = parse::parse_search(base_seg, true)?;
    let cmds = segs[1..].iter().map(|s| parse::parse_command(s)).collect::<Result<Vec<_>, _>>()?;
    let mut pushdown = Pushdown::default();
    collect_pushdown(&base, &mut pushdown);
    Ok(Pipeline { base, earliest: tb.earliest, latest: tb.latest, cmds, pushdown })
}

/// Walk the top-level AND spine; anything under OR/NOT stays out.
fn collect_pushdown(e: &Expr, out: &mut Pushdown) {
    match e {
        Expr::And(a, b) => {
            collect_pushdown(a, out);
            collect_pushdown(b, out);
        }
        Expr::Term(t) if !t.is_empty() && !t.contains('*') => out.terms.push(t.to_lowercase()),
        Expr::FieldGlob(f, v) if !v.contains('*') => out.fields.push((f.clone(), v.clone())),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_pipeline() {
        let p = parse("search error host=web01 earliest=-1h | stats count by host | sort -count | head 5").unwrap();
        assert_eq!(p.earliest.as_deref(), Some("-1h"));
        assert_eq!(p.latest, None);
        assert_eq!(p.cmds.len(), 3);
        assert_eq!(p.pushdown.terms, vec!["error"]);
        assert_eq!(p.pushdown.fields, vec![("host".to_string(), "web01".to_string())]);
    }

    #[test]
    fn pushdown_skips_or_not_and_globs() {
        let p = parse("a (b OR c) NOT d host=web* sourcetype=app").unwrap();
        assert_eq!(p.pushdown.terms, vec!["a"]);
        assert_eq!(p.pushdown.fields, vec![("sourcetype".to_string(), "app".to_string())]);
    }

    #[test]
    fn empty_and_star_searches_match_all() {
        for q in ["", "*", "search", "  | head 5"] {
            let p = parse(q).unwrap_or_else(|e| panic!("{q}: {e}"));
            assert!(p.pushdown.terms.is_empty(), "{q}");
        }
    }

    #[test]
    fn parse_errors_bubble_up() {
        assert!(parse("a | nosuchcmd x").is_err());
        assert!(parse("a | where lower(").is_err());
        assert!(parse("a | stats count | search earliest=-1h").is_err()); // time only in base
    }
}
