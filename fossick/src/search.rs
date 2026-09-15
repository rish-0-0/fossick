//! M1 search: brute-force scan of all buckets, newest first.
//! Query = whitespace-separated bare terms (case-insensitive substrings of
//! _raw), `field=value` filters on index-time fields, `earliest=`/`latest=`.
//! Kept simple on purpose — it becomes the differential-test oracle once the
//! tsidx path lands.

use crate::tstamp;
use fossick_index::bucket::{BucketScan, OwnedEvent};
use fossick_index::list_buckets;
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
        let hit = match field_ids[i] {
            Some(id) => ev.metas.iter().any(|(k, v)| *k == id && v == want.as_bytes()),
            None => false, // field not in this bucket's table
        };
        if !hit {
            return false;
        }
    }
    if !q.terms.is_empty() {
        let raw = ev.raw.to_ascii_lowercase();
        return q.terms.iter().all(|t| raw.windows(t.len().max(1)).any(|w| w == t.as_bytes()));
    }
    true
}

/// Stream matching events, newest bucket first, to `emit(event, field_names)`.
pub fn run<F>(data_dir: &Path, q: &Query, mut emit: F) -> io::Result<u64>
where
    F: FnMut(&OwnedEvent, &[String]),
{
    let mut hits = 0;
    for bucket in list_buckets(data_dir)? {
        if bucket.max_time_ms < q.earliest || bucket.min_time_ms > q.latest {
            continue;
        }
        let scan = BucketScan::open(&bucket.dir, q.earliest, q.latest, true)?;
        let field_ids: Vec<Option<u8>> = q.fields.iter().map(|(f, _)| scan.man.field_id(f)).collect();
        let fields = scan.man.fields.clone();
        for ev in scan {
            let ev = ev?;
            if matches(q, &ev, &field_ids) {
                hits += 1;
                emit(&ev, &fields);
            }
        }
    }
    Ok(hits)
}

/// Convenience for tests: collect matching (time_ms, raw-as-string).
pub fn collect(data_dir: &Path, query: &str, now_ms: u64) -> io::Result<Vec<(u64, String)>> {
    let q = parse_query(query, now_ms)?;
    let mut out = Vec::new();
    run(data_dir, &q, |ev, _| out.push((ev.time_ms, String::from_utf8_lossy(&ev.raw).into_owned())))?;
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
