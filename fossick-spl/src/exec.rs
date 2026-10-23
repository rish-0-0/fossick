//! Push-based pipeline executor. Records are fed one at a time; streaming
//! commands transform in place, blocking commands (stats, sort, tail, …)
//! buffer until `finish`. `feed` returns false once no further input can
//! change the output (head satisfied, sort buffer full, …) so the caller can
//! stop scanning the index early.

use crate::expr::{eval, Ctx, Expr};
use crate::value::{cmp_values, fmt_num, glob_match, Record, Value};
use regex::Regex;
use std::collections::{BTreeSet, HashMap, VecDeque};

/// Values kept by `values(x)` / `list(x)` per group (Splunk caps list at 100).
const VALUES_MAX: usize = 1000;
const LIST_MAX: usize = 100;

/// Result-size caps, normally taken from fossick.conf `[limits]`.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_results: usize,
    pub sort_max: usize,
    pub stats_max_groups: usize,
    pub dedup_max: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_results: 10_000, sort_max: 10_000, stats_max_groups: 100_000, dedup_max: 100_000 }
    }
}

/// Final result: column names plus rows of values aligned to them.
#[derive(Debug, PartialEq)]
pub struct Table {
    pub fields: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub is_aggregated: bool,
    pub truncated: bool,
}

/// A parsed pipeline command (state-free; the executor builds stages from these).
#[derive(Clone, Debug)]
pub enum Cmd {
    Where(Expr),
    Eval(Vec<(String, Expr)>),
    FieldsKeep(Vec<String>),
    FieldsRemove(Vec<String>),
    Table(Vec<String>),
    Rename(Vec<(String, String)>),
    Rex { field: String, re: Regex },
    RegexFilter { field: String, re: Regex, negate: bool },
    Fillnull { value: Value, fields: Vec<String> },
    Head(usize),
    Tail(usize),
    Reverse,
    Dedup { keep: usize, keys: Vec<String> },
    Sort { keys: Vec<(String, bool)>, limit: usize },
    Stats { aggs: Vec<Agg>, by: Vec<String> },
    Top { limit: usize, fields: Vec<String>, by: Vec<String>, rare: bool },
}

#[derive(Clone, Debug)]
pub struct Agg {
    pub func: AggFunc,
    pub field: Option<String>,
    /// Output column name (the spec as typed, or the `as` alias).
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AggFunc {
    Count,
    Dc,
    Sum,
    Avg,
    Min,
    Max,
    First,
    Last,
    Values,
    List,
    Stdev,
    Median,
    Range,
    Perc(f64),
}

enum AggState {
    Count(u64),
    Stat { sum: f64, sumsq: f64, n: u64 },
    Best(Option<Value>),
    Edge(Option<Value>),
    Set(BTreeSet<String>),
    List(Vec<String>),
    Nums(Vec<f64>),
    Span { min: f64, max: f64, n: u64 },
}

impl AggFunc {
    fn new_state(&self) -> AggState {
        match self {
            AggFunc::Count => AggState::Count(0),
            AggFunc::Sum | AggFunc::Avg | AggFunc::Stdev => AggState::Stat { sum: 0.0, sumsq: 0.0, n: 0 },
            AggFunc::Min | AggFunc::Max => AggState::Best(None),
            AggFunc::First | AggFunc::Last => AggState::Edge(None),
            AggFunc::Dc | AggFunc::Values => AggState::Set(BTreeSet::new()),
            AggFunc::List => AggState::List(Vec::new()),
            AggFunc::Median | AggFunc::Perc(_) => AggState::Nums(Vec::new()),
            AggFunc::Range => AggState::Span { min: f64::MAX, max: f64::MIN, n: 0 },
        }
    }
}

impl Agg {
    fn update(&self, state: &mut AggState, rec: &Record) {
        let v = match &self.field {
            Some(f) => rec.get_or_null(f),
            None => Value::Bool(true), // bare count: every record counts
        };
        if v.is_null() {
            return;
        }
        match state {
            AggState::Count(n) => *n += 1,
            AggState::Stat { sum, sumsq, n } => {
                if let Some(x) = v.as_num() {
                    *sum += x;
                    *sumsq += x * x;
                    *n += 1;
                }
            }
            AggState::Best(best) => {
                let keep = match (&self.func, best.as_ref()) {
                    (_, None) => true,
                    (AggFunc::Min, Some(b)) => cmp_values(&v, b).is_lt(),
                    (_, Some(b)) => cmp_values(&v, b).is_gt(),
                };
                if keep {
                    *best = Some(v);
                }
            }
            AggState::Edge(slot) => {
                if self.func == AggFunc::Last || slot.is_none() {
                    *slot = Some(v);
                }
            }
            AggState::Set(set) => {
                if set.len() < VALUES_MAX {
                    set.insert(v.render().unwrap_or_default());
                }
            }
            AggState::List(list) => {
                if list.len() < LIST_MAX {
                    list.push(v.render().unwrap_or_default());
                }
            }
            AggState::Nums(xs) => {
                if let Some(x) = v.as_num() {
                    xs.push(x);
                }
            }
            AggState::Span { min, max, n } => {
                if let Some(x) = v.as_num() {
                    *min = min.min(x);
                    *max = max.max(x);
                    *n += 1;
                }
            }
        }
    }

    fn finish(&self, state: AggState) -> Value {
        match state {
            AggState::Count(n) => Value::Num(n as f64),
            AggState::Stat { n: 0, .. } => Value::Null,
            AggState::Stat { sum, sumsq, n } => {
                let nf = n as f64;
                match self.func {
                    AggFunc::Sum => Value::Num(sum),
                    AggFunc::Avg => Value::Num(sum / nf),
                    _ => {
                        // sample stdev; a single value has spread 0
                        if n == 1 {
                            Value::Num(0.0)
                        } else {
                            Value::Num(((sumsq - sum * sum / nf) / (nf - 1.0)).max(0.0).sqrt())
                        }
                    }
                }
            }
            AggState::Best(v) | AggState::Edge(v) => v.unwrap_or(Value::Null),
            AggState::Set(set) => match self.func {
                AggFunc::Dc => Value::Num(set.len() as f64),
                _ if set.is_empty() => Value::Null,
                _ => Value::Str(set.into_iter().collect::<Vec<_>>().join("\n")),
            },
            AggState::List(list) if list.is_empty() => Value::Null,
            AggState::List(list) => Value::Str(list.join("\n")),
            AggState::Nums(mut xs) => {
                if xs.is_empty() {
                    return Value::Null;
                }
                xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let p = match self.func {
                    AggFunc::Perc(p) => p,
                    _ => 50.0,
                };
                // nearest-rank percentile
                let rank = ((p / 100.0 * xs.len() as f64).ceil() as usize).clamp(1, xs.len());
                Value::Num(xs[rank - 1])
            }
            AggState::Span { n: 0, .. } => Value::Null,
            AggState::Span { min, max, .. } => Value::Num(max - min),
        }
    }
}

type GroupKey = Vec<String>;

enum Stage {
    Where(Expr),
    Eval(Vec<(String, Expr)>),
    FieldsKeep(Vec<String>),
    FieldsRemove(Vec<String>),
    Table {
        spec: Vec<String>,
        seen: Vec<String>,
    },
    Rename(Vec<(String, String)>),
    Rex {
        field: String,
        re: Regex,
    },
    RegexFilter {
        field: String,
        re: Regex,
        negate: bool,
    },
    Fillnull {
        value: Value,
        fields: Vec<String>,
    },
    FillnullAll {
        value: Value,
        buf: Vec<Record>,
        truncated: bool,
    },
    Head {
        n: usize,
        seen: usize,
    },
    Tail {
        n: usize,
        buf: VecDeque<Record>,
    },
    Reverse {
        buf: Vec<Record>,
        truncated: bool,
    },
    Dedup {
        keep: usize,
        keys: Vec<String>,
        seen: HashMap<GroupKey, usize>,
        truncated: bool,
    },
    Sort {
        keys: Vec<(String, bool)>,
        limit: usize,
        buf: Vec<Record>,
        truncated: bool,
    },
    Stats {
        aggs: Vec<Agg>,
        by: Vec<String>,
        groups: HashMap<GroupKey, Vec<AggState>>,
        truncated: bool,
    },
    Top {
        limit: usize,
        fields: Vec<String>,
        by: Vec<String>,
        rare: bool,
        counts: HashMap<GroupKey, HashMap<GroupKey, u64>>,
        truncated: bool,
    },
}

fn build_stage(cmd: &Cmd) -> Stage {
    match cmd.clone() {
        Cmd::Where(e) => Stage::Where(e),
        Cmd::Eval(v) => Stage::Eval(v),
        Cmd::FieldsKeep(v) => Stage::FieldsKeep(v),
        Cmd::FieldsRemove(v) => Stage::FieldsRemove(v),
        Cmd::Table(spec) => Stage::Table { spec, seen: Vec::new() },
        Cmd::Rename(v) => Stage::Rename(v),
        Cmd::Rex { field, re } => Stage::Rex { field, re },
        Cmd::RegexFilter { field, re, negate } => Stage::RegexFilter { field, re, negate },
        Cmd::Fillnull { value, fields } if fields.is_empty() => {
            Stage::FillnullAll { value, buf: Vec::new(), truncated: false }
        }
        Cmd::Fillnull { value, fields } => Stage::Fillnull { value, fields },
        Cmd::Head(n) => Stage::Head { n, seen: 0 },
        Cmd::Tail(n) => Stage::Tail { n, buf: VecDeque::new() },
        Cmd::Reverse => Stage::Reverse { buf: Vec::new(), truncated: false },
        Cmd::Dedup { keep, keys } => Stage::Dedup { keep, keys, seen: HashMap::new(), truncated: false },
        Cmd::Sort { keys, limit } => Stage::Sort { keys, limit, buf: Vec::new(), truncated: false },
        Cmd::Stats { aggs, by } => Stage::Stats { aggs, by, groups: HashMap::new(), truncated: false },
        Cmd::Top { limit, fields, by, rare } => {
            Stage::Top { limit, fields, by, rare, counts: HashMap::new(), truncated: false }
        }
    }
}

struct Output {
    recs: Vec<Record>,
    cap: usize,
    truncated: bool,
}

impl Output {
    fn push(&mut self, rec: Record) {
        if self.recs.len() < self.cap {
            self.recs.push(rec);
        } else {
            self.truncated = true;
        }
    }
    fn full(&self) -> bool {
        self.recs.len() >= self.cap
    }
}

pub struct Executor {
    base: Expr,
    stages: Vec<Stage>,
    out: Output,
    ctx: Ctx,
    limits: Limits,
    is_aggregated: bool,
}

impl Executor {
    pub fn new(base: Expr, cmds: &[Cmd], limits: Limits, now_ms: u64) -> Executor {
        Executor {
            base,
            stages: cmds.iter().map(build_stage).collect(),
            out: Output { recs: Vec::new(), cap: limits.max_results, truncated: false },
            ctx: Ctx { now_ms },
            limits,
            is_aggregated: cmds.iter().any(|c| matches!(c, Cmd::Stats { .. } | Cmd::Top { .. })),
        }
    }

    /// Feed one record; Ok(false) means further input cannot change the output.
    pub fn feed(&mut self, rec: Record) -> Result<bool, String> {
        if eval(&self.base, &rec, &self.ctx)?.truthy() {
            push(&mut self.stages, &mut self.out, &self.ctx, &self.limits, rec)?;
        }
        Ok(self.wants_more())
    }

    /// Whether more input could still affect the result.
    pub fn wants_more(&self) -> bool {
        for s in &self.stages {
            match s {
                Stage::Head { n, seen } if seen >= n => return false,
                Stage::Sort { buf, .. } | Stage::Reverse { buf, .. } if buf.len() >= self.limits.sort_max => {
                    return false
                }
                Stage::Dedup { keep: 1, seen, .. } if seen.len() >= self.limits.dedup_max => return false,
                Stage::Sort { .. }
                | Stage::Reverse { .. }
                | Stage::Tail { .. }
                | Stage::Stats { .. }
                | Stage::Top { .. }
                | Stage::FillnullAll { .. } => return true,
                _ => {}
            }
        }
        !self.out.full()
    }

    /// Flush blocking stages and assemble the result table.
    pub fn finish(mut self) -> Result<Table, String> {
        // Hitting a cap exactly counts as truncated: the caller may have
        // stopped feeding on wants_more() == false, so more input could exist.
        let mut truncated = self.out.truncated || self.out.full();
        for i in 0..self.stages.len() {
            let (drained, trunc) = drain(&mut self.stages[i], &self.limits);
            truncated |= trunc;
            let rest = &mut self.stages[i + 1..];
            for rec in drained {
                push(rest, &mut self.out, &self.ctx, &self.limits, rec)?;
            }
        }
        truncated |= self.out.truncated;

        // Column order: an explicit `table` wins; otherwise first-seen order,
        // with raw-event searches hiding internal fields and pinning
        // _time first / _raw last.
        let table_cols = self.stages.iter().rev().find_map(|s| match s {
            Stage::Table { seen, .. } => Some(seen.clone()),
            _ => None,
        });
        let fields = match table_cols {
            Some(cols) => cols,
            None => {
                let mut cols: Vec<String> = Vec::new();
                for rec in &self.out.recs {
                    for (k, _) in &rec.0 {
                        if !cols.contains(k) {
                            cols.push(k.clone());
                        }
                    }
                }
                if !self.is_aggregated {
                    cols.retain(|c| !c.starts_with('_') || c == "_time" || c == "_raw");
                    if let Some(p) = cols.iter().position(|c| c == "_time") {
                        let c = cols.remove(p);
                        cols.insert(0, c);
                    }
                    if let Some(p) = cols.iter().position(|c| c == "_raw") {
                        let c = cols.remove(p);
                        cols.push(c);
                    }
                }
                cols
            }
        };
        let rows = self.out.recs.iter().map(|rec| fields.iter().map(|f| rec.get_or_null(f)).collect()).collect();
        Ok(Table { fields, rows, is_aggregated: self.is_aggregated, truncated })
    }
}

/// Route one record through `stages`, appending survivors to `out`.
fn push(stages: &mut [Stage], out: &mut Output, ctx: &Ctx, limits: &Limits, mut rec: Record) -> Result<(), String> {
    let Some((stage, rest)) = stages.split_first_mut() else {
        out.push(rec);
        return Ok(());
    };
    match stage {
        Stage::Where(e) => {
            if !eval(e, &rec, ctx)?.truthy() {
                return Ok(());
            }
        }
        Stage::Eval(assigns) => {
            for (name, e) in assigns.iter() {
                let v = eval(e, &rec, ctx)?;
                rec.set(name, v);
            }
        }
        Stage::FieldsKeep(names) => {
            // Internal fields survive keep-mode (remove them with `fields - _raw`).
            rec.0.retain(|(k, _)| k.starts_with('_') || names.iter().any(|p| glob_match(p, k)));
        }
        Stage::FieldsRemove(names) => {
            rec.0.retain(|(k, _)| !names.iter().any(|p| glob_match(p, k)));
        }
        Stage::Table { spec, seen } => {
            let mut projected = Record::default();
            for pat in spec.iter() {
                if pat.contains('*') {
                    for (k, v) in &rec.0 {
                        if glob_match(pat, k) {
                            if !seen.contains(k) {
                                seen.push(k.clone());
                            }
                            projected.set(k, v.clone());
                        }
                    }
                } else {
                    // Exact names always become columns, even if never present.
                    if !seen.contains(pat) {
                        seen.push(pat.clone());
                    }
                    if let Some(v) = rec.get(pat) {
                        let v = v.clone();
                        projected.set(pat, v);
                    }
                }
            }
            rec = projected;
        }
        Stage::Rename(pairs) => {
            for (src, dst) in pairs.iter() {
                if src != dst && rec.get(src).is_some() {
                    rec.remove(dst);
                    for (k, _) in rec.0.iter_mut() {
                        if k == src {
                            *k = dst.clone();
                        }
                    }
                }
            }
        }
        Stage::Rex { field, re } => {
            let text = rec.get(field).and_then(Value::render).unwrap_or_default();
            if let Some(caps) = re.captures(&text) {
                let names: Vec<String> = re.capture_names().flatten().map(String::from).collect();
                for name in names {
                    if let Some(m) = caps.name(&name) {
                        rec.set(&name, Value::str(m.as_str()));
                    }
                }
            }
        }
        Stage::RegexFilter { field, re, negate } => {
            let hit = match rec.get(field).and_then(Value::render) {
                Some(text) => re.is_match(&text),
                None => false,
            };
            if hit == *negate {
                return Ok(());
            }
        }
        Stage::Fillnull { value, fields } => {
            for f in fields.iter() {
                if rec.get_or_null(f).is_null() {
                    rec.set(f, value.clone());
                }
            }
        }
        Stage::FillnullAll { buf, truncated, .. } => {
            if buf.len() < limits.sort_max {
                buf.push(rec);
            } else {
                *truncated = true;
            }
            return Ok(());
        }
        Stage::Head { n, seen } => {
            if seen >= n {
                return Ok(());
            }
            *seen += 1;
        }
        Stage::Tail { n, buf } => {
            if buf.len() == *n {
                buf.pop_front();
            }
            buf.push_back(rec);
            return Ok(());
        }
        Stage::Reverse { buf, truncated } => {
            if buf.len() < limits.sort_max {
                buf.push(rec);
            } else {
                *truncated = true;
            }
            return Ok(());
        }
        Stage::Dedup { keep, keys, seen, truncated } => {
            // Streaming: the first `keep` records per key pass through.
            let Some(key) = group_key(&rec, keys) else { return Ok(()) };
            if !seen.contains_key(&key) && seen.len() >= limits.dedup_max {
                *truncated = true;
                return Ok(());
            }
            let n = seen.entry(key).or_insert(0);
            if *n >= *keep {
                return Ok(());
            }
            *n += 1;
        }
        Stage::Sort { buf, truncated, .. } => {
            if buf.len() < limits.sort_max {
                buf.push(rec);
            } else {
                *truncated = true;
            }
            return Ok(());
        }
        Stage::Stats { aggs, by, groups, truncated } => {
            let Some(key) = group_key(&rec, by) else { return Ok(()) };
            if !groups.contains_key(&key) && groups.len() >= limits.stats_max_groups {
                *truncated = true;
                return Ok(());
            }
            let states = groups.entry(key).or_insert_with(|| aggs.iter().map(|a| a.func.new_state()).collect());
            for (agg, state) in aggs.iter().zip(states.iter_mut()) {
                agg.update(state, &rec);
            }
            return Ok(());
        }
        Stage::Top { fields, by, counts, truncated, .. } => {
            let (Some(bkey), Some(fkey)) = (group_key(&rec, by), group_key(&rec, fields)) else {
                return Ok(());
            };
            let inner = counts.entry(bkey).or_default();
            if !inner.contains_key(&fkey) && inner.len() >= limits.stats_max_groups {
                *truncated = true;
            } else {
                *inner.entry(fkey).or_insert(0) += 1;
            }
            return Ok(());
        }
    }
    push(rest, out, ctx, limits, rec)
}

/// Rendered values of `fields`; None if any field is absent (record excluded,
/// matching Splunk's by-clause behaviour).
fn group_key(rec: &Record, fields: &[String]) -> Option<GroupKey> {
    fields.iter().map(|f| rec.get(f).and_then(Value::render)).collect()
}

fn sort_keys(keys: &mut [GroupKey]) {
    keys.sort_by(|a, b| {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| cmp_values(&Value::str(x.clone()), &Value::str(y.clone())))
            .find(|o| !o.is_eq())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Pull buffered records out of a blocking stage at finish time.
fn drain(stage: &mut Stage, limits: &Limits) -> (Vec<Record>, bool) {
    match stage {
        Stage::Sort { keys, limit, buf, truncated } => {
            let mut recs = std::mem::take(buf);
            recs.sort_by(|a, b| {
                keys.iter()
                    .map(|(f, desc)| {
                        let o = cmp_values(&a.get_or_null(f), &b.get_or_null(f));
                        if *desc {
                            o.reverse()
                        } else {
                            o
                        }
                    })
                    .find(|o| !o.is_eq())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let trunc = *truncated || recs.len() >= limits.sort_max;
            if *limit > 0 {
                recs.truncate(*limit);
            }
            (recs, trunc)
        }
        Stage::Tail { buf, .. } => {
            // Splunk tail returns the last N in reverse order.
            (buf.drain(..).rev().collect(), false)
        }
        Stage::Reverse { buf, truncated } => {
            let mut recs = std::mem::take(buf);
            let trunc = *truncated || recs.len() >= limits.sort_max;
            recs.reverse();
            (recs, trunc)
        }
        Stage::Dedup { keep: 1, seen, truncated, .. } => (Vec::new(), *truncated || seen.len() >= limits.dedup_max),
        Stage::FillnullAll { value, buf, truncated } => {
            let recs = std::mem::take(buf);
            let mut all: Vec<String> = Vec::new();
            for rec in &recs {
                for (k, _) in &rec.0 {
                    if !all.contains(k) {
                        all.push(k.clone());
                    }
                }
            }
            let filled = recs
                .into_iter()
                .map(|mut rec| {
                    for f in &all {
                        if rec.get_or_null(f).is_null() {
                            rec.set(f, value.clone());
                        }
                    }
                    rec
                })
                .collect();
            (filled, *truncated)
        }
        Stage::Stats { aggs, by, groups, truncated } => {
            let mut keys: Vec<GroupKey> = groups.keys().cloned().collect();
            sort_keys(&mut keys);
            let recs = keys
                .into_iter()
                .map(|key| {
                    let states = groups.remove(&key).unwrap();
                    let mut rec = Record::default();
                    for (f, v) in by.iter().zip(key) {
                        rec.set(f, Value::Str(v));
                    }
                    for (agg, state) in aggs.iter().zip(states) {
                        let v = agg.finish(state);
                        rec.set(&agg.name, v);
                    }
                    rec
                })
                .collect();
            (recs, *truncated)
        }
        Stage::Top { limit, fields, by, rare, counts, truncated } => {
            let mut bkeys: Vec<GroupKey> = counts.keys().cloned().collect();
            sort_keys(&mut bkeys);
            let mut recs = Vec::new();
            for bkey in bkeys {
                let inner = counts.remove(&bkey).unwrap();
                let total: u64 = inner.values().sum();
                let mut entries: Vec<(GroupKey, u64)> = inner.into_iter().collect();
                entries.sort_by(|(ka, na), (kb, nb)| {
                    let by_count = if *rare { na.cmp(nb) } else { nb.cmp(na) };
                    by_count.then_with(|| ka.cmp(kb))
                });
                for (fkey, n) in entries.into_iter().take((*limit).max(1)) {
                    let mut rec = Record::default();
                    for (f, v) in by.iter().zip(&bkey) {
                        rec.set(f, Value::str(v.clone()));
                    }
                    for (f, v) in fields.iter().zip(fkey) {
                        rec.set(f, Value::Str(v));
                    }
                    rec.set("count", Value::Num(n as f64));
                    let pct = (n as f64 / total as f64 * 100.0 * 1e6).round() / 1e6;
                    rec.set("percent", Value::Num(pct));
                    recs.push(rec);
                }
            }
            (recs, *truncated)
        }
        _ => (Vec::new(), false), // streaming stages hold nothing
    }
}

/// Render a value for plain-text output (CLI): null prints as empty.
pub fn render_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Num(n) => fmt_num(*n),
        v => v.render().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::Expr;

    fn rec(pairs: &[(&str, &str)]) -> Record {
        Record(pairs.iter().map(|(k, v)| (k.to_string(), Value::str(*v))).collect())
    }

    fn run(cmds: Vec<Cmd>, recs: Vec<Record>) -> Table {
        let mut ex = Executor::new(Expr::Lit(Value::Bool(true)), &cmds, Limits::default(), 0);
        for r in recs {
            ex.feed(r).unwrap();
        }
        ex.finish().unwrap()
    }

    #[test]
    fn stats_count_by() {
        let t = run(
            vec![Cmd::Stats {
                aggs: vec![Agg { func: AggFunc::Count, field: None, name: "count".into() }],
                by: vec!["host".into()],
            }],
            vec![rec(&[("host", "a")]), rec(&[("host", "b")]), rec(&[("host", "a")]), rec(&[("other", "x")])],
        );
        assert_eq!(t.fields, vec!["host", "count"]);
        assert!(t.is_aggregated);
        assert_eq!(t.rows, vec![vec![Value::str("a"), Value::Num(2.0)], vec![Value::str("b"), Value::Num(1.0)],]);
    }

    #[test]
    fn stats_numeric_aggs() {
        let aggs = vec![
            Agg { func: AggFunc::Avg, field: Some("x".into()), name: "avg(x)".into() },
            Agg { func: AggFunc::Sum, field: Some("x".into()), name: "sum(x)".into() },
            Agg { func: AggFunc::Min, field: Some("x".into()), name: "min(x)".into() },
            Agg { func: AggFunc::Max, field: Some("x".into()), name: "max(x)".into() },
            Agg { func: AggFunc::Median, field: Some("x".into()), name: "median(x)".into() },
            Agg { func: AggFunc::Perc(75.0), field: Some("x".into()), name: "perc75(x)".into() },
            Agg { func: AggFunc::Range, field: Some("x".into()), name: "range(x)".into() },
            Agg { func: AggFunc::Dc, field: Some("x".into()), name: "dc(x)".into() },
            Agg { func: AggFunc::Stdev, field: Some("x".into()), name: "stdev(x)".into() },
        ];
        let recs = ["1", "2", "3", "4"].iter().map(|v| rec(&[("x", v)])).collect();
        let t = run(vec![Cmd::Stats { aggs, by: vec![] }], recs);
        let row = &t.rows[0];
        assert_eq!(row[0], Value::Num(2.5)); // avg
        assert_eq!(row[1], Value::Num(10.0)); // sum
        assert_eq!(row[2], Value::str("1")); // min keeps original value
        assert_eq!(row[3], Value::str("4"));
        assert_eq!(row[4], Value::Num(2.0)); // median (nearest rank)
        assert_eq!(row[5], Value::Num(3.0)); // p75
        assert_eq!(row[6], Value::Num(3.0)); // range
        assert_eq!(row[7], Value::Num(4.0)); // dc
        let Value::Num(sd) = row[8] else { panic!() };
        assert!((sd - 1.2909944).abs() < 1e-6); // sample stdev
    }

    #[test]
    fn sort_head_tail_reverse() {
        let recs: Vec<Record> = ["3", "1", "2"].iter().map(|v| rec(&[("n", v)])).collect();
        let sort = Cmd::Sort { keys: vec![("n".into(), false)], limit: 0 };
        let t = run(vec![sort.clone()], recs.clone());
        assert_eq!(
            t.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
            vec![Value::str("1"), Value::str("2"), Value::str("3")]
        );

        let t = run(vec![sort.clone(), Cmd::Head(2)], recs.clone());
        assert_eq!(t.rows.len(), 2);

        let t = run(vec![Cmd::Tail(2)], recs.clone());
        // last two, reversed: 2 then 1
        assert_eq!(t.rows[0][0], Value::str("2"));
        assert_eq!(t.rows[1][0], Value::str("1"));

        let t = run(vec![Cmd::Reverse], recs);
        assert_eq!(t.rows[0][0], Value::str("2"));
    }

    #[test]
    fn head_stops_the_scan() {
        let mut ex = Executor::new(Expr::Lit(Value::Bool(true)), &[Cmd::Head(1)], Limits::default(), 0);
        assert!(!ex.feed(rec(&[("a", "1")])).unwrap());
    }

    #[test]
    fn dedup_streams_first_occurrence() {
        let recs: Vec<Record> = [("h", "a"), ("h", "b"), ("h", "a")].iter().map(|(k, v)| rec(&[(*k, *v)])).collect();
        let t = run(vec![Cmd::Dedup { keep: 1, keys: vec!["h".into()] }], recs);
        assert_eq!(t.rows.len(), 2);
    }

    #[test]
    fn top_counts_and_percent() {
        let recs: Vec<Record> = ["a", "a", "a", "b"].iter().map(|v| rec(&[("h", v)])).collect();
        let t = run(vec![Cmd::Top { limit: 10, fields: vec!["h".into()], by: vec![], rare: false }], recs.clone());
        assert_eq!(t.fields, vec!["h", "count", "percent"]);
        assert_eq!(t.rows[0], vec![Value::str("a"), Value::Num(3.0), Value::Num(75.0)]);
        assert_eq!(t.rows[1], vec![Value::str("b"), Value::Num(1.0), Value::Num(25.0)]);

        let t = run(vec![Cmd::Top { limit: 1, fields: vec!["h".into()], by: vec![], rare: true }], recs);
        assert_eq!(t.rows[0][0], Value::str("b")); // rare
    }

    #[test]
    fn table_orders_and_creates_columns() {
        let t =
            run(vec![Cmd::Table(vec!["b".into(), "missing".into(), "a".into()])], vec![rec(&[("a", "1"), ("b", "2")])]);
        assert_eq!(t.fields, vec!["b", "missing", "a"]);
        assert_eq!(t.rows[0], vec![Value::str("2"), Value::Null, Value::str("1")]);
    }

    #[test]
    fn fields_keep_hides_but_retains_internal() {
        let t = run(
            vec![Cmd::FieldsKeep(vec!["a".into()])],
            vec![rec(&[("a", "1"), ("b", "2"), ("_raw", "raw text"), ("_time", "t")])],
        );
        assert_eq!(t.fields, vec!["_time", "a", "_raw"]); // _time first, _raw last, b gone
    }

    #[test]
    fn fillnull_all_fields() {
        let t = run(
            vec![Cmd::Fillnull { value: Value::str("0"), fields: vec![] }],
            vec![rec(&[("a", "1")]), rec(&[("b", "2")])],
        );
        assert_eq!(t.rows[0], vec![Value::str("1"), Value::str("0")]);
        assert_eq!(t.rows[1], vec![Value::str("0"), Value::str("2")]);
    }

    #[test]
    fn rex_extracts_named_groups() {
        let re = Regex::new(r"user=(?<user>\w+)").unwrap();
        let t = run(
            vec![Cmd::Rex { field: "_raw".into(), re }, Cmd::Table(vec!["user".into()])],
            vec![Record(vec![("_raw".into(), Value::str("login user=alice ok"))])],
        );
        assert_eq!(t.rows[0], vec![Value::str("alice")]);
    }

    #[test]
    fn regex_filters() {
        let re = Regex::new("^a").unwrap();
        let recs: Vec<Record> = ["abc", "xyz"].iter().map(|v| rec(&[("f", v)])).collect();
        let t = run(vec![Cmd::RegexFilter { field: "f".into(), re: re.clone(), negate: false }], recs.clone());
        assert_eq!(t.rows.len(), 1);
        let t = run(vec![Cmd::RegexFilter { field: "f".into(), re, negate: true }], recs);
        assert_eq!(t.rows[0][0], Value::str("xyz"));
    }

    #[test]
    fn rename_moves_values() {
        let t = run(vec![Cmd::Rename(vec![("a".into(), "z".into())])], vec![rec(&[("a", "1"), ("b", "2")])]);
        assert_eq!(t.fields, vec!["z", "b"]);
    }

    #[test]
    fn limits_mark_truncation() {
        let limits = Limits { max_results: 2, ..Limits::default() };
        let mut ex = Executor::new(Expr::Lit(Value::Bool(true)), &[], limits, 0);
        for i in 0..5 {
            ex.feed(rec(&[("n", &i.to_string())])).unwrap();
        }
        let t = ex.finish().unwrap();
        assert_eq!(t.rows.len(), 2);
        assert!(t.truncated);
    }
}
