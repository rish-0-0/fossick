//! Search-head bridge between fossick-spl and the index.
//!
//! Every search request lands here first (never on the index directly): the
//! search head resolves time bounds, may rewrite the request (default time
//! range, result caps), derives the index pushdown, then feeds decoded events
//! through the SPL executor.

use crate::conf::Conf;
use crate::{search, serve, tstamp};
use fossick_index::bucket::OwnedEvent;
use fossick_spl::{Limits, Pipeline, Record, Table, Value};
use std::io;
use std::path::Path;

/// Fields auto-extracted from `_raw` as `key=value` at search time, per event.
const KV_MAX: usize = 50;

pub struct SplRequest<'a> {
    pub spl: &'a str,
    /// Fallback time bounds (e.g. from the UI time picker); the SPL's own
    /// earliest=/latest= win.
    pub earliest: Option<&'a str>,
    pub latest: Option<&'a str>,
}

pub struct SplResult {
    pub table: Table,
    /// Events that matched the base search (before pipeline commands).
    pub matched: u64,
}

pub fn limits_from_conf(conf: &Conf) -> Limits {
    Limits {
        max_results: conf.max_results,
        sort_max: conf.sort_max,
        stats_max_groups: conf.stats_max_groups,
        dedup_max: conf.dedup_max,
    }
}

fn bad_input(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Search-head request rewrite: SPL time bounds beat request parameters, and
/// `latest` is clamped to now. This is the hook where per-deployment policy
/// (default windows, region tweaks) belongs.
fn resolve_time(pipeline: &Pipeline, req: &SplRequest, now_ms: u64) -> io::Result<(u64, u64)> {
    let spec = |s: &str| {
        tstamp::parse_timespec(s, now_ms).ok_or_else(|| bad_input(format!("bad timespec: {s}")))
    };
    let earliest = match pipeline.earliest.as_deref().or(req.earliest) {
        Some(s) => spec(s)?,
        None => 0,
    };
    let latest = match pipeline.latest.as_deref().or(req.latest) {
        Some(s) => spec(s)?.min(now_ms),
        None => now_ms,
    };
    if earliest > latest {
        return Err(bad_input("earliest is after latest".into()));
    }
    Ok((earliest, latest))
}

/// Pushdown query for the index: time range, conjunctive terms, and only the
/// field equalities that are index-time fields (anything else would wrongly
/// exclude events — the executor re-verifies the full expression anyway).
fn pushdown_query(pipeline: &Pipeline, conf: &Conf, earliest: u64, latest: u64) -> search::Query {
    let indexable = serve::base_fields(conf);
    search::Query {
        terms: pipeline.pushdown.terms.clone(),
        fields: pipeline
            .pushdown
            .fields
            .iter()
            .filter(|(f, _)| indexable.iter().any(|i| i == f))
            .cloned()
            .collect(),
        earliest,
        latest,
    }
}

/// Decode an indexed event into an SPL record: search-time `key=value`
/// extraction from `_raw` first, then index-time fields (authoritative),
/// then `_time` (ISO-8601, so lexicographic order is chronological) and `_raw`.
pub fn to_record(ev: &OwnedEvent, fields: &[String]) -> Record {
    let mut rec = Record::default();
    rec.set("_time", Value::str(tstamp::to_iso(ev.time_ms)));
    extract_kv(&ev.raw, &mut rec);
    for (id, val) in &ev.metas {
        if let Some(name) = fields.get(*id as usize) {
            rec.set(name, Value::str(String::from_utf8_lossy(val).into_owned()));
        }
    }
    rec.set("_raw", Value::str(String::from_utf8_lossy(&ev.raw).into_owned()));
    rec
}

/// Auto-extract `key=value` / `key="quoted value"` pairs from a raw line.
/// First occurrence of a key wins.
fn extract_kv(raw: &[u8], rec: &mut Record) {
    let mut i = 0;
    let mut found = 0;
    while i < raw.len() && found < KV_MAX {
        // Key: identifier immediately followed by '=' on a word boundary.
        if !(raw[i].is_ascii_alphabetic() || raw[i] == b'_')
            || (i > 0 && (raw[i - 1].is_ascii_alphanumeric() || raw[i - 1] == b'_'))
        {
            i += 1;
            continue;
        }
        let start = i;
        while i < raw.len() && (raw[i].is_ascii_alphanumeric() || raw[i] == b'_') {
            i += 1;
        }
        if raw.get(i) != Some(&b'=') {
            continue;
        }
        let key = &raw[start..i];
        i += 1;
        let val = if raw.get(i) == Some(&b'"') {
            i += 1;
            let vs = i;
            while i < raw.len() && raw[i] != b'"' {
                i += 1;
            }
            let v = &raw[vs..i];
            i = (i + 1).min(raw.len());
            v
        } else {
            let vs = i;
            while i < raw.len() && !raw[i].is_ascii_whitespace() {
                i += 1;
            }
            &raw[vs..i]
        };
        let key = String::from_utf8_lossy(key).into_owned();
        if rec.get(&key).is_none() {
            rec.set(&key, Value::str(String::from_utf8_lossy(val).into_owned()));
            found += 1;
        }
    }
}

/// Parse and run an SPL request against the index. Errors of kind
/// InvalidInput are the caller's fault (HTTP 400); the rest are I/O.
pub fn run(data_dir: &Path, conf: &Conf, req: &SplRequest, now_ms: u64) -> io::Result<SplResult> {
    let pipeline = fossick_spl::parse(req.spl).map_err(bad_input)?;
    let (earliest, latest) = resolve_time(&pipeline, req, now_ms)?;
    let q = pushdown_query(&pipeline, conf, earliest, latest);
    let mut ex = pipeline.executor(limits_from_conf(conf), now_ms);
    let mut exec_err = None;
    let matched = search::run(data_dir, &q, |ev, fields| {
        match ex.feed(to_record(ev, fields)) {
            Ok(more) => more,
            Err(e) => {
                exec_err = Some(e);
                false
            }
        }
    })?;
    if let Some(e) = exec_err {
        return Err(bad_input(e));
    }
    let table = ex.finish().map_err(bad_input)?;
    Ok(SplResult { table, matched })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(raw: &[u8]) -> Record {
        let mut r = Record::default();
        extract_kv(raw, &mut r);
        r
    }

    #[test]
    fn kv_extraction_basics() {
        let r = kv(b"ts=1 level=info msg=\"hello world\" empty= n=5");
        assert_eq!(r.get("level"), Some(&Value::str("info")));
        assert_eq!(r.get("msg"), Some(&Value::str("hello world")));
        assert_eq!(r.get("n"), Some(&Value::str("5")));
        assert_eq!(r.get("empty"), Some(&Value::str("")));
        assert_eq!(r.get("hello"), None); // inside quotes, not a key
    }

    #[test]
    fn kv_respects_word_boundaries_and_first_wins() {
        let r = kv(b"xlevel=bad");
        assert_eq!(r.get("level"), None);
        assert_eq!(r.get("xlevel"), Some(&Value::str("bad")));
        let r = kv(b"a=1 a=2");
        assert_eq!(r.get("a"), Some(&Value::str("1")));
        // keys can't start with a digit
        assert_eq!(kv(b"9x=1").get("9x"), None);
    }

    #[test]
    fn record_shape() {
        let ev = OwnedEvent {
            time_ms: 1_789_467_072_345,
            metas: vec![(1, b"app".to_vec()), (2, b"web01".to_vec())],
            raw: b"level=warn boom".to_vec(),
        };
        let fields = vec!["source".into(), "sourcetype".into(), "host".into()];
        let r = to_record(&ev, &fields);
        assert_eq!(r.get("_time"), Some(&Value::str("2026-09-15T10:11:12.345Z")));
        assert_eq!(r.get("sourcetype"), Some(&Value::str("app")));
        assert_eq!(r.get("host"), Some(&Value::str("web01")));
        assert_eq!(r.get("level"), Some(&Value::str("warn")));
        assert_eq!(r.get("_raw"), Some(&Value::str("level=warn boom")));
        // _time first, _raw last
        assert_eq!(r.0.first().unwrap().0, "_time");
        assert_eq!(r.0.last().unwrap().0, "_raw");
    }

    #[test]
    fn index_time_fields_beat_kv() {
        let ev = OwnedEvent {
            time_ms: 0,
            metas: vec![(2, b"real-host".to_vec())],
            raw: b"host=spoofed msg=x".to_vec(),
        };
        let fields = vec!["source".into(), "sourcetype".into(), "host".into()];
        let r = to_record(&ev, &fields);
        assert_eq!(r.get("host"), Some(&Value::str("real-host")));
    }
}
