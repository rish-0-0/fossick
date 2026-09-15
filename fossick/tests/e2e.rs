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
    let acceptor = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        serve::handle_conn(stream, &conn_input, tx); // tx dropped when conn closes
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
    let hits = search::collect(&conf.data_dir, "step earliest=2026-09-15T10:01:00Z latest=2026-09-15T10:01:59Z", tstamp::now_ms()).unwrap();
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
    assert_eq!(search::collect(&conf.data_dir, "sourcetype=nginx host=web01 source=/var/log/nginx.log", now).unwrap().len(), 1);
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
fn crlf_blank_lines_and_no_trailing_newline() {
    let conf = test_conf("framing");
    let input = TcpInput { port: 0, source: "s".into(), sourcetype: "st".into(), host: "h".into() };

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let conn_input = input.clone();
    let acceptor = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        serve::handle_conn(stream, &conn_input, tx);
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
