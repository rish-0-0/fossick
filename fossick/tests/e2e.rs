//! End-to-end: TCP ingest → hot bucket on disk → scan search. Runs the real
//! connection handler and indexer loop in-process on an ephemeral port.

use fossick::conf::{Conf, TcpInput};
use fossick::{search, serve, tstamp};
use std::io::Write;
use std::net::{TcpListener, TcpStream};

fn test_conf(name: &str) -> Conf {
    let dir = std::env::temp_dir().join(format!("fossick-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Conf { data_dir: dir, indexed_fields: vec!["level".into()], ..Conf::default() }
}

/// Push `lines` through a real TCP connection into a real indexer; returns
/// once everything is flushed to disk.
fn ingest(conf: &Conf, input: TcpInput, header: Option<&str>, lines: &[String]) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();

    let conn_input = input.clone();
    let max_line_bytes = conf.max_line_bytes;
    let acceptor = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        serve::handle_conn(stream, &conn_input, max_line_bytes, tx, serve::Shutdown::new());
    });
    let indexer = {
        let conf = conf.clone();
        std::thread::spawn(move || serve::run_indexer(&conf, rx))
    };

    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    if let Some(h) = header {
        writeln!(client, "{h}").unwrap();
    }
    for l in lines {
        writeln!(client, "{l}").unwrap();
    }
    drop(client);

    acceptor.join().unwrap();
    indexer.join().unwrap().unwrap(); // Disconnected → final flush → Ok
}

#[test]
fn ingest_and_search_roundtrip() {
    let conf = test_conf("roundtrip");
    let input = TcpInput { port: 0, source: "test".into(), sourcetype: "app".into(), host: "unit".into() };
    let lines: Vec<String> = (0..500)
        .map(|i| format!("2026-09-15T10:{:02}:{:02}Z level=info job step {i} needle_{}", i / 60, i % 60, i % 7))
        .collect();
    ingest(&conf, input, None, &lines);

    // Bare term, case-insensitive.
    let hits = search::collect(&conf.data_dir, "NEEDLE_3", tstamp::now_ms()).unwrap();
    assert_eq!(hits.len(), 71); // i % 7 == 3, i in 0..500
    assert!(hits.iter().all(|(_, raw)| raw.contains("needle_3")));

    // Newest first: parsed timestamps must be descending.
    assert!(hits.windows(2).all(|w| w[0].0 >= w[1].0));

    // AND of terms + index-time field filter (sourcetype from input defaults).
    let hits = search::collect(&conf.data_dir, "needle_3 step sourcetype=app", tstamp::now_ms()).unwrap();
    assert_eq!(hits.len(), 71);
    assert!(search::collect(&conf.data_dir, "needle_3 sourcetype=other", tstamp::now_ms()).unwrap().is_empty());

    // Indexed kv field (level=info extracted at index time).
    let hits = search::collect(&conf.data_dir, "level=info needle_0", tstamp::now_ms()).unwrap();
    assert_eq!(hits.len(), 500 / 7 + 1);

    // Time range clamps: events span 10:00:00–10:08:19 on 2026-09-15.
    let hits = search::collect(
        &conf.data_dir,
        "step earliest=2026-09-15T10:01:00Z latest=2026-09-15T10:01:59Z",
        tstamp::now_ms(),
    )
    .unwrap();
    assert_eq!(hits.len(), 60);
}

#[test]
fn rollover_produces_warm_buckets_and_search_spans_them() {
    let mut conf = test_conf("rollover");
    conf.hot_max_events = 100;
    let input = TcpInput { port: 0, source: "s".into(), sourcetype: "app".into(), host: "h".into() };
    let lines: Vec<String> = (0..550)
        .map(|i| format!("2026-09-15T{:02}:{:02}:{:02}Z roll_needle event {i}", 8 + i / 3600, i / 60 % 60, i % 60))
        .collect();
    ingest(&conf, input.clone(), None, &lines);

    let buckets = fossick_index::list_buckets(&conf.data_dir).unwrap();
    let warm: Vec<_> = buckets.iter().filter(|b| !b.hot).collect();
    assert_eq!(warm.len(), 5, "550 events at 100/bucket → 5 warm rolls");
    assert!(buckets.iter().any(|b| b.hot), "remainder stays hot");
    for w in &warm {
        assert!(w.min_time_ms <= w.max_time_ms && w.min_time_ms > 0, "db_ name carries time span");
    }
    let now = tstamp::now_ms();
    assert_eq!(search::collect(&conf.data_dir, "roll_needle", now).unwrap().len(), 550);

    // Restart: a second indexer run must recover the leftover hot bucket to warm.
    let (tx, rx) = std::sync::mpsc::channel::<serve::IngestEvent>();
    drop(tx);
    serve::run_indexer(&conf, rx).unwrap();
    let after = fossick_index::list_buckets(&conf.data_dir).unwrap();
    assert_eq!(after.iter().filter(|b| !b.hot).count(), 6);
    assert_eq!(search::collect(&conf.data_dir, "roll_needle", now).unwrap().len(), 550);
}

/// Differential test: the indexed (tsidx+bloom) path must return exactly what
/// the brute-force scan oracle returns, across warm and hot buckets.
#[test]
fn indexed_search_matches_scan_oracle() {
    let mut conf = test_conf("differential");
    conf.hot_max_events = 150; // force several warm buckets
    let input = TcpInput { port: 0, source: "s".into(), sourcetype: "app".into(), host: "h".into() };
    let lines: Vec<String> = (0..700)
        .map(|i| {
            format!(
                "2026-09-15T{:02}:{:02}:{:02}Z level={} svc=api-{} diff_needle_{} some words",
                9 + i / 3600,
                i / 60 % 60,
                i % 60,
                if i % 3 == 0 { "error" } else { "info" },
                i % 4,
                i % 11
            )
        })
        .collect();
    ingest(&conf, input, None, &lines);
    assert!(fossick_index::list_buckets(&conf.data_dir).unwrap().iter().any(|b| !b.hot));

    let now = tstamp::now_ms();
    let queries = [
        "diff_needle_7",
        "diff_needle_7 level=error",
        "error words",
        "sourcetype=app diff_needle_0",
        "diff_needle_3 earliest=2026-09-15T09:02:00Z latest=2026-09-15T09:05:00Z",
        "level=info api",
        "absent_term_xyz",
        "sourcetype=nope",
    ];
    for qs in queries {
        let q = search::parse_query(qs, now).unwrap();
        let mut indexed = Vec::new();
        search::run(&conf.data_dir, &q, |ev, _| {
            indexed.push((ev.time_ms, ev.raw.clone()));
            true
        })
        .unwrap();
        let mut oracle = Vec::new();
        search::run_scan_only(&conf.data_dir, &q, |ev, _| {
            oracle.push((ev.time_ms, ev.raw.clone()));
            true
        })
        .unwrap();
        indexed.sort();
        oracle.sort();
        assert_eq!(indexed, oracle, "query: {qs}");
        // Sanity: the interesting queries must actually match something.
        if !qs.contains("absent") && !qs.contains("nope") {
            assert!(!oracle.is_empty(), "oracle empty for: {qs}");
        }
    }
}

#[test]
fn fossick_header_overrides_metadata() {
    let conf = test_conf("header");
    let input = TcpInput { port: 0, source: "tcp:0".into(), sourcetype: "default".into(), host: "x".into() };
    ingest(
        &conf,
        input,
        Some("##fossick host=web01 sourcetype=nginx source=/var/log/nginx.log"),
        &["2026-09-15T10:00:00Z GET /".to_string()],
    );
    let now = tstamp::now_ms();
    assert_eq!(
        search::collect(&conf.data_dir, "sourcetype=nginx host=web01 source=/var/log/nginx.log", now).unwrap().len(),
        1
    );
    assert!(search::collect(&conf.data_dir, "sourcetype=default", now).unwrap().is_empty());
}

#[test]
fn unparseable_timestamp_gets_receipt_time_and_marker() {
    let conf = test_conf("receipt");
    let input = TcpInput { port: 0, source: "s".into(), sourcetype: "st".into(), host: "h".into() };
    let before = tstamp::now_ms();
    ingest(&conf, input, None, &["no timestamp in this line at all".to_string()]);
    let after = tstamp::now_ms();

    let hits = search::collect(&conf.data_dir, "_time_source=receipt", before).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].0 >= before && hits[0].0 <= after);
}

#[test]
fn rest_api_streams_ndjson_results() {
    use std::io::Read;
    let conf = test_conf("http");
    let input = TcpInput { port: 0, source: "s".into(), sourcetype: "app".into(), host: "web01".into() };
    let lines: Vec<String> =
        (0..50).map(|i| format!("2026-09-15T10:00:{:02}Z api_needle_{} msg=\"q{i}\"", i % 60, i % 5)).collect();
    ingest(&conf, input, None, &lines);

    let api = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = api.local_addr().unwrap().port();
    fossick::http::spawn(api, conf.clone());

    let get = |target: &str| -> (String, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let (head, body) = resp.split_once("\r\n\r\n").unwrap();
        (head.to_string(), body.to_string())
    };

    let (head, body) = get("/search?q=api_needle_2+sourcetype%3Dapp");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("application/x-ndjson"));
    let events: Vec<&str> = body.lines().collect();
    assert_eq!(events.len(), 10);
    for e in &events {
        assert!(e.starts_with("{\"_time\":\"2026-09-15T10:00:"), "{e}");
        assert!(e.contains("\"sourcetype\":\"app\"") && e.contains("\"host\":\"web01\""));
        assert!(e.contains("api_needle_2") && e.contains("msg=\\\"q"));
    }

    let (head, body) = get("/health");
    assert!(head.starts_with("HTTP/1.1 200"));
    assert_eq!(body, "ok\n");
    assert!(get("/nope").0.starts_with("HTTP/1.1 404"));
    assert!(get("/search?q=earliest%3Djunk").0.starts_with("HTTP/1.1 400"));
}

#[test]
fn invalid_utf8_preserves_bytes_through_index_and_search() {
    let conf = test_conf("utf8");
    let input = TcpInput { port: 0, source: "s".into(), sourcetype: "st".into(), host: "h".into() };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let conn_input = input.clone();
    let max_line_bytes = conf.max_line_bytes;
    let acceptor = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        serve::handle_conn(stream, &conn_input, max_line_bytes, tx, serve::Shutdown::new());
    });
    let indexer = {
        let conf = conf.clone();
        std::thread::spawn(move || serve::run_indexer(&conf, rx))
    };
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client.write_all(b"2026-09-15T10:00:00Z uniq_utf8\xff\xfe_marker\n").unwrap();
    drop(client);
    acceptor.join().unwrap();
    indexer.join().unwrap().unwrap();

    let hits = search::collect(&conf.data_dir, "uniq_utf8", tstamp::now_ms()).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].1.contains("uniq_utf8"));
    // Search path uses lossy UTF-8 for display; invalid bytes become U+FFFD.
    assert!(hits[0].1.contains('\u{FFFD}'));
}

#[test]
fn oversize_line_truncated_to_max_line_bytes() {
    let mut conf = test_conf("oversize");
    conf.max_line_bytes = 32;
    let input = TcpInput { port: 0, source: "s".into(), sourcetype: "st".into(), host: "h".into() };
    let long = format!("2026-09-15T10:00:00Z marker_{}", "x".repeat(80));
    ingest(&conf, input, None, &[long, "2026-09-15T10:00:01Z ok_line".to_string()]);

    let now = tstamp::now_ms();
    let hits = search::collect(&conf.data_dir, "marker_", now).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1.len(), 32);
    assert!(hits[0].1.starts_with("2026-09-15T10:00:00Z marker_"));
    assert_eq!(search::collect(&conf.data_dir, "ok_line", now).unwrap().len(), 1);
}

#[test]
fn crlf_blank_lines_and_no_trailing_newline() {
    let conf = test_conf("framing");
    let input = TcpInput { port: 0, source: "s".into(), sourcetype: "st".into(), host: "h".into() };

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let conn_input = input.clone();
    let max_line_bytes = conf.max_line_bytes;
    let acceptor = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        serve::handle_conn(stream, &conn_input, max_line_bytes, tx, serve::Shutdown::new());
    });
    let indexer = {
        let conf = conf.clone();
        std::thread::spawn(move || serve::run_indexer(&conf, rx))
    };
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client.write_all(b"first line\r\n\r\n\nsecond line").unwrap(); // CRLF, blanks, no trailing \n
    drop(client);
    acceptor.join().unwrap();
    indexer.join().unwrap().unwrap();

    let hits = search::collect(&conf.data_dir, "line", tstamp::now_ms()).unwrap();
    let raws: Vec<&str> = hits.iter().map(|(_, r)| r.as_str()).collect();
    assert_eq!(hits.len(), 2);
    assert!(raws.contains(&"first line") && raws.contains(&"second line"));
}

#[test]
fn graceful_shutdown_drains_in_flight_events() {
    let conf = test_conf("graceful-shutdown");
    let shutdown = serve::Shutdown::new();
    let input = TcpInput { port: 0, source: "s".into(), sourcetype: "st".into(), host: "h".into() };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let listener_thread = serve::spawn_listener(listener, input, conf.max_line_bytes, tx, shutdown.clone());
    let indexer = std::thread::spawn({
        let conf = conf.clone();
        move || serve::run_indexer(&conf, rx)
    });

    const N: usize = 150;
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    for i in 0..N {
        writeln!(client, "2026-09-15T10:00:{:02}Z graceful_evt_{}", i % 60, i).unwrap();
    }
    client.flush().unwrap();
    shutdown.request();
    listener_thread.join().unwrap();
    // Client stays open (long-lived shipper); readers must still exit and drain.
    indexer.join().unwrap().unwrap();
    drop(client);

    let hits = search::collect(&conf.data_dir, "graceful_evt_", tstamp::now_ms()).unwrap();
    assert_eq!(hits.len(), N);
}

#[test]
fn spl_api_returns_json_tables() {
    use std::io::Read;
    let conf = test_conf("spl-api");
    let input = TcpInput { port: 0, source: "s".into(), sourcetype: "app".into(), host: "web01".into() };
    let lines: Vec<String> = (0..30)
        .map(|i| {
            format!("2026-09-15T10:00:{:02}Z level={} req dur={}", i % 60, if i % 5 == 0 { "error" } else { "info" }, i)
        })
        .collect();
    ingest(&conf, input, None, &lines);

    let api = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = api.local_addr().unwrap().port();
    fossick::http::spawn(api, conf.clone());
    let get = |target: &str| -> (String, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let (head, body) = resp.split_once("\r\n\r\n").unwrap();
        (head.to_string(), body.to_string())
    };

    // Aggregating pipeline: stats by the index-time `level` field.
    let (head, body) = get("/api/search?q=req%20%7C%20stats%20count%20by%20level");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("application/json"));
    assert!(body.contains("\"fields\":[\"level\",\"count\"]"), "{body}");
    assert!(body.contains("[\"error\",6]") && body.contains("[\"info\",24]"), "{body}");
    assert!(body.contains("\"isAggregated\":true") && body.contains("\"matched\":30"), "{body}");

    // Event search with search-time kv extraction + eval.
    let (_, body) = get("/api/search?q=level%3Derror%20%7C%20eval%20d2%3Ddur*2%20%7C%20sort%20-d2%20%7C%20head%201%20%7C%20table%20dur%20d2");
    assert!(body.contains("\"fields\":[\"dur\",\"d2\"]"), "{body}");
    assert!(body.contains("[\"25\",50]"), "{body}");

    // Bad SPL → 400 with a JSON error.
    let (head, body) = get("/api/search?q=%7C%20nosuchcmd");
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
    assert!(body.contains("\"error\""), "{body}");

    // UI shell is served at /.
    let (head, body) = get("/");
    assert!(head.starts_with("HTTP/1.1 200") && head.contains("text/html"), "{head}");
    assert!(body.contains("fossick"), "{body}");
}
