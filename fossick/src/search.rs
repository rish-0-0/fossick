//! M1 search: brute-force scan of all buckets, newest first.
//! Query = whitespace-separated bare terms (case-insensitive substrings of
//! _raw), `field=value` filters on index-time fields, `earliest=`/`latest=`.
//! Kept simple on purpose — it becomes the differential-test oracle once the
//! tsidx path lands.

use crate::tstamp;
use fossick_index::bucket::{BucketScan, OwnedEvent};
use fossick_index::list_buckets;
use fossick_index::manifest::Manifest;
use std::io;
use std::path::Path;

#[derive(Debug, Default, PartialEq)]
pub struct Query {
    /// Lowercased substring terms.
    pub terms: Vec<String>,
    /// (field name, expected value) on index-time fields.
    pub fields: Vec<(String, String)>,
    pub earliest: u64,
    pub latest: u64,
}

pub fn parse_query(q: &str, now_ms: u64) -> io::Result<Query> {
    let mut out = Query { latest: u64::MAX, ..Query::default() };
    for tok in q.split_whitespace() {
        match tok.split_once('=') {
            Some(("earliest", v)) => {
                out.earliest = tstamp::parse_timespec(v, now_ms)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("bad earliest: {v}")))?
            }
            Some(("latest", v)) => {
                out.latest = tstamp::parse_timespec(v, now_ms)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("bad latest: {v}")))?
            }
            Some((f, v)) => out.fields.push((f.to_string(), v.to_string())),
            None => out.terms.push(tok.to_ascii_lowercase()),
        }
    }
    Ok(out)
}

fn matches(q: &Query, ev: &OwnedEvent, field_ids: &[Option<u8>]) -> bool {
    for (i, (_, want)) in q.fields.iter().enumerate() {
        // Case-insensitive value match, consistent with the lowercased
        // `name::value` terms the tsidx/bloom pushdown uses.
        let hit = match field_ids[i] {
            Some(id) => ev.metas.iter().any(|(k, v)| *k == id && v.eq_ignore_ascii_case(want.as_bytes())),
            None => false, // field not in this bucket's table
        };
        if !hit {
            return false;
        }
    }
    q.terms.iter().all(|t| contains_ci(&ev.raw, t.as_bytes()))
}

/// Case-insensitive (ASCII) substring test without allocating.
fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

/// Stream matching events, newest bucket first, to `emit(event, field_names)`.
/// Hot buckets are scanned; warm buckets go through bloom → tsidx postings.
/// `emit` returning false stops the search early (e.g. `| head N` satisfied).
pub fn run<F>(data_dir: &Path, q: &Query, mut emit: F) -> io::Result<u64>
where
    F: FnMut(&OwnedEvent, &[String]) -> bool,
{
    let mut hits = 0;
    for bucket in list_buckets(data_dir)? {
        if bucket.max_time_ms < q.earliest || bucket.min_time_ms > q.latest {
            continue; // time-prune via db_* name
        }
        let r = if bucket.hot { scan_bucket(&bucket.dir, q, &mut emit) } else { indexed_bucket(&bucket.dir, q, &mut emit) };
        match r {
            Ok((n, more)) => {
                hits += n;
                if !more {
                    break;
                }
            }
            // Bucket was renamed/removed mid-search (hot→warm roll): skip it.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(hits)
}

/// Brute-force block scan — the hot-bucket path and the differential-test oracle.
fn scan_bucket<F>(dir: &Path, q: &Query, emit: &mut F) -> io::Result<(u64, bool)>
where
    F: FnMut(&OwnedEvent, &[String]) -> bool,
{
    let scan = BucketScan::open(dir, q.earliest, q.latest, true)?;
    let field_ids: Vec<Option<u8>> = q.fields.iter().map(|(f, _)| scan.man.field_id(f)).collect();
    let fields = scan.man.fields.clone();
    let mut hits = 0;
    for ev in scan {
        let ev = ev?;
        if matches(q, &ev, &field_ids) {
            hits += 1;
            if !emit(&ev, &fields) {
                return Ok((hits, false));
            }
        }
    }
    Ok((hits, true))
}

/// Warm-bucket path: bloom prune → postings intersection → time clamp via
/// events.tbl → block-cache decode (newest first) → verify → emit.
fn indexed_bucket<F>(dir: &Path, q: &Query, emit: &mut F) -> io::Result<(u64, bool)>
where
    F: FnMut(&OwnedEvent, &[String]) -> bool,
{
    use fossick_index::{bloom::Bloom, bucket::{BlockCache, EventsTbl}, tokenize, tsidx::{self, Tsidx}};

    let idx = match Tsidx::open(dir) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return scan_bucket(dir, q, emit), // pre-tsidx bucket
        Err(e) => return Err(e),
    };
    // Required index terms: every token of every AND term + field composites.
    let mut req: Vec<Vec<u8>> = Vec::new();
    for t in &q.terms {
        tokenize::tokenize(t.as_bytes(), |tok| req.push(tok.to_vec()));
    }
    for (f, v) in &q.fields {
        req.push(tokenize::field_term(f, v.as_bytes()));
    }
    req.sort();
    req.dedup();
    if req.is_empty() {
        return scan_bucket(dir, q, emit); // pure time-range search
    }
    if let Some(bloom) = Bloom::open(dir)? {
        if req.iter().any(|t| !bloom.may_contain(t)) {
            return Ok((0, true)); // bucket cannot contain all required terms
        }
    }
    let mut posts = Vec::with_capacity(req.len());
    for t in &req {
        match idx.lookup(t) {
            Some(p) => posts.push(p),
            None => return Ok((0, true)),
        }
    }
    let ords = tsidx::intersect(posts);
    let man = Manifest::read(dir)?;
    let field_ids: Vec<Option<u8>> = q.fields.iter().map(|(f, _)| man.field_id(f)).collect();
    let tbl = EventsTbl::open(dir)?;
    let (lo, hi) = tbl.ord_range(q.earliest, q.latest);
    let mut cache = BlockCache::open(dir)?;
    let mut hits = 0;
    for &ord in ords.iter().rev() {
        // postings are time-ordered → reverse iteration = newest first
        let ord = ord as usize;
        if ord < lo || ord >= hi {
            continue;
        }
        let (_, addr) = tbl.get(ord);
        let ev = cache.event(addr)?;
        if matches(q, &ev, &field_ids) {
            hits += 1;
            if !emit(&ev, &man.fields) {
                return Ok((hits, false));
            }
        }
    }
    Ok((hits, true))
}

/// Differential-test oracle: brute-force scan of every bucket, ignoring
/// tsidx/bloom entirely.
pub fn run_scan_only<F>(data_dir: &Path, q: &Query, mut emit: F) -> io::Result<u64>
where
    F: FnMut(&OwnedEvent, &[String]) -> bool,
{
    let mut hits = 0;
    for bucket in list_buckets(data_dir)? {
        if bucket.max_time_ms < q.earliest || bucket.min_time_ms > q.latest {
            continue;
        }
        let (n, more) = scan_bucket(&bucket.dir, q, &mut emit)?;
        hits += n;
        if !more {
            break;
        }
    }
    Ok(hits)
}

/// Convenience for tests: collect matching (time_ms, raw-as-string).
pub fn collect(data_dir: &Path, query: &str, now_ms: u64) -> io::Result<Vec<(u64, String)>> {
    let q = parse_query(query, now_ms)?;
    let mut out = Vec::new();
    run(data_dir, &q, |ev, _| {
        out.push((ev.time_ms, String::from_utf8_lossy(&ev.raw).into_owned()));
        true
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing() {
        let q = parse_query("ERROR sourcetype=app earliest=-15m latest=now needle", 1_000_000_000).unwrap();
        assert_eq!(q.terms, vec!["error", "needle"]);
        assert_eq!(q.fields, vec![("sourcetype".to_string(), "app".to_string())]);
        assert_eq!(q.earliest, 1_000_000_000 - 900_000);
        assert_eq!(q.latest, 1_000_000_000);
        assert!(parse_query("earliest=junk", 0).is_err());
    }
}
