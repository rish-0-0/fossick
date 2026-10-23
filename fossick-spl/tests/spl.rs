//! End-to-end SPL tests: parse a query string, feed synthetic events through
//! the executor, assert on the resulting table.

use fossick_spl::{parse, Limits, Record, Table, Value};

const NOW: u64 = 1_789_473_600_000;

/// Web-log-ish fixture: 10 events with status/bytes/host/level fields.
fn events() -> Vec<Record> {
    let rows = [
        ("web01", "200", "512", "info", "GET /index ok"),
        ("web01", "200", "1024", "info", "GET /about ok"),
        ("web02", "404", "128", "warn", "GET /missing not found"),
        ("web01", "500", "0", "error", "POST /pay failed: timeout"),
        ("web02", "200", "2048", "info", "GET /index ok"),
        ("web02", "500", "0", "error", "POST /pay failed: db down"),
        ("web03", "200", "256", "info", "GET /health ok"),
        ("web01", "404", "64", "warn", "GET /old not found"),
        ("web02", "200", "4096", "info", "GET /big ok"),
        ("web01", "301", "0", "info", "GET /moved redirect"),
    ];
    rows.iter()
        .enumerate()
        .map(|(i, (host, status, bytes, level, msg))| {
            Record(vec![
                ("_time".into(), Value::str(format!("2026-09-15T10:00:{i:02}.000Z"))),
                ("host".into(), Value::str(*host)),
                ("status".into(), Value::str(*status)),
                ("bytes".into(), Value::str(*bytes)),
                ("level".into(), Value::str(*level)),
                ("_raw".into(), Value::str(format!("{msg} status={status} bytes={bytes}"))),
            ])
        })
        .collect()
}

fn run(spl: &str) -> Table {
    run_limited(spl, Limits::default())
}

fn run_limited(spl: &str, limits: Limits) -> Table {
    let p = parse(spl).unwrap_or_else(|e| panic!("{spl}: {e}"));
    let mut ex = p.executor(limits, NOW);
    for r in events() {
        if !ex.feed(r).unwrap() {
            break;
        }
    }
    ex.finish().unwrap()
}

fn col<'a>(t: &'a Table, name: &str) -> Vec<&'a Value> {
    let i = t.fields.iter().position(|f| f == name).unwrap_or_else(|| panic!("no column {name} in {:?}", t.fields));
    t.rows.iter().map(|r| &r[i]).collect()
}

#[test]
fn plain_search_filters_and_orders_columns() {
    let t = run("failed");
    assert_eq!(t.rows.len(), 2);
    assert_eq!(t.fields.first().map(String::as_str), Some("_time"));
    assert_eq!(t.fields.last().map(String::as_str), Some("_raw"));
    assert!(!t.is_aggregated);
}

#[test]
fn search_boolean_logic() {
    assert_eq!(run("status=500 OR status=404").rows.len(), 4);
    assert_eq!(run("host=web01 NOT level=error").rows.len(), 4);
    assert_eq!(run("host=web0* level=info status!=301").rows.len(), 5);
    assert_eq!(run("status IN (301, 500)").rows.len(), 3);
    assert_eq!(run("bytes>=1024").rows.len(), 3); // numeric comparison
    assert_eq!(run("\"not found\"").rows.len(), 2); // quoted phrase
    assert_eq!(run("GET NOT (ok OR redirect)").rows.len(), 2);
}

#[test]
fn stats_by_host() {
    let t = run("| stats count, sum(bytes) as total by host");
    assert_eq!(t.fields, vec!["host", "count", "total"]);
    assert!(t.is_aggregated);
    assert_eq!(t.rows.len(), 3);
    // web01: 5 events, 512+1024+0+64+0
    assert_eq!(t.rows[0][0], Value::str("web01"));
    assert_eq!(t.rows[0][1], Value::Num(5.0));
    assert_eq!(t.rows[0][2], Value::Num(1600.0));
}

#[test]
fn stats_global_and_dc() {
    let t = run("| stats count, dc(host), avg(bytes), max(bytes)");
    assert_eq!(t.rows.len(), 1);
    assert_eq!(t.rows[0][0], Value::Num(10.0));
    assert_eq!(t.rows[0][1], Value::Num(3.0));
    assert_eq!(t.rows[0][2], Value::Num(812.8));
    assert_eq!(t.rows[0][3], Value::str("4096"));
}

#[test]
fn eval_where_sort_head_chain() {
    let t = run("| eval kb=round(bytes/1024, 2) | where kb>0 | sort -kb | head 2 | table host kb");
    assert_eq!(t.fields, vec!["host", "kb"]);
    assert_eq!(t.rows, vec![vec![Value::str("web02"), Value::Num(4.0)], vec![Value::str("web02"), Value::Num(2.0)],]);
}

#[test]
fn eval_string_functions() {
    let t = run("host=web03 | eval tag=upper(host) . \":\" . level | table tag");
    assert_eq!(t.rows, vec![vec![Value::str("WEB03:info")]]);
}

#[test]
fn where_is_case_sensitive_but_search_is_not() {
    assert_eq!(run("host=WEB01").rows.len(), 5); // search: case-insensitive
    assert_eq!(run("| where host=\"WEB01\"").rows.len(), 0); // where: exact
    assert_eq!(run("| where like(host, \"web0%\")").rows.len(), 10);
}

#[test]
fn top_and_rare() {
    let t = run("| top 1 host");
    assert_eq!(t.fields, vec!["host", "count", "percent"]);
    assert_eq!(t.rows, vec![vec![Value::str("web01"), Value::Num(5.0), Value::Num(50.0)]]);

    let t = run("| rare 1 host");
    assert_eq!(t.rows[0][0], Value::str("web03"));

    let t = run("| top 1 status by host");
    assert_eq!(t.fields, vec!["host", "status", "count", "percent"]);
    assert_eq!(t.rows.len(), 3); // one per host
}

#[test]
fn dedup_and_fields() {
    let t = run("| dedup host | fields host, level | table host level");
    assert_eq!(t.rows.len(), 3);

    let t = run("| dedup 2 host");
    assert_eq!(t.rows.len(), 5); // 2 + 2 kept for web01/web02, web03 has 1
}

#[test]
fn rex_extracts_then_aggregates() {
    let t = run(r#"failed | rex "failed: (?<reason>.+) status" | stats values(reason) as reasons"#);
    assert_eq!(t.rows[0][0], Value::str("db down\ntimeout"));
}

#[test]
fn regex_filters_raw() {
    assert_eq!(run(r#"| regex "^POST""#).rows.len(), 2);
    assert_eq!(run(r#"| regex _raw!="^GET""#).rows.len(), 2);
    assert_eq!(run(r#"| regex level="^(warn|error)$""#).rows.len(), 4);
}

#[test]
fn rename_fillnull_reverse_tail() {
    let t = run("| rename host AS server | table server");
    assert_eq!(t.fields, vec!["server"]);
    assert_eq!(t.rows[0][0], Value::str("web01"));

    let t = run(r#"| rex "failed: (?<reason>.+) status" | fillnull value=- reason | dedup reason | table reason"#);
    assert_eq!(col(&t, "reason").len(), 3); // "-", "timeout", "db down"

    let t = run("| reverse | head 1 | table status");
    assert_eq!(t.rows[0][0], Value::str("301")); // last event first

    let t = run("| tail 1 | table status");
    assert_eq!(t.rows[0][0], Value::str("301"));
}

#[test]
fn mid_pipeline_search() {
    let t = run("| stats count by host | search count>4");
    assert_eq!(t.rows.len(), 1);
    assert_eq!(t.rows[0][0], Value::str("web01"));
}

#[test]
fn sort_is_numeric_aware() {
    let t = run("| stats count by status | sort -count | head 1");
    assert_eq!(t.rows[0][0], Value::str("200")); // 5 events
    let t = run("| dedup bytes | sort bytes | table bytes | head 2");
    assert_eq!(t.rows[0][0], Value::str("0"));
    assert_eq!(t.rows[1][0], Value::str("64")); // numeric, not lexicographic
}

#[test]
fn head_terminates_input_early() {
    let p = parse("| head 3").unwrap();
    let mut ex = p.executor(Limits::default(), NOW);
    let mut fed = 0;
    for r in events() {
        fed += 1;
        if !ex.feed(r).unwrap() {
            break;
        }
    }
    assert_eq!(fed, 3);
    assert_eq!(ex.finish().unwrap().rows.len(), 3);
}

#[test]
fn limits_truncate_and_flag() {
    let t = run_limited("*", Limits { max_results: 4, ..Limits::default() });
    assert_eq!(t.rows.len(), 4);
    assert!(t.truncated);

    let t = run_limited("| stats count by status", Limits { stats_max_groups: 2, ..Limits::default() });
    assert_eq!(t.rows.len(), 2);
    assert!(t.truncated);

    let t = run_limited("| sort bytes", Limits { sort_max: 5, ..Limits::default() });
    assert_eq!(t.rows.len(), 5);
    assert!(t.truncated);
}

#[test]
fn aggregated_flag_reflects_pipeline() {
    assert!(!run("| head 3").is_aggregated);
    assert!(run("| stats count").is_aggregated);
    assert!(run("| top host").is_aggregated);
}

#[test]
fn runtime_regex_error_surfaces() {
    let p = parse("| eval x=match(_raw, level)").unwrap(); // dynamic pattern
    let mut ex = p.executor(Limits::default(), NOW);
    let bad = Record(vec![("level".into(), Value::str("[")), ("_raw".into(), Value::str("x"))]);
    assert!(ex.feed(bad).is_err());
}
