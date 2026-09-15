//! Hand-rolled HTTP/1.1 search API (no framework, std::net only).
//! Binds 127.0.0.1 only. Responses are close-delimited so results stream
//! event-at-a-time with bounded memory.
//!
//! GET /health            → 200 "ok"
//! GET /search?q=<query>  → 200 application/x-ndjson, one JSON event per line

use crate::{search, tstamp};
use fossick_index::bucket::OwnedEvent;
use std::io::{self, BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

const MAX_REQUEST: usize = 16 * 1024;

/// Accept-loop in a background thread; one thread per request (laptop scale).
pub fn spawn(listener: TcpListener, data_dir: PathBuf) {
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let d = data_dir.clone();
            std::thread::spawn(move || {
                let _ = handle(stream, &d);
            });
        }
    });
}

fn handle(mut stream: TcpStream, data_dir: &Path) -> io::Result<()> {
    // Read request head (we ignore any body — GET only).
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() > MAX_REQUEST {
            return respond(&mut stream, 431, "request too large\n");
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&chunk[..n]);
    }
    let first = head.split(|&b| b == b'\r').next().unwrap_or_default();
    let line = String::from_utf8_lossy(first);
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method != "GET" {
        return respond(&mut stream, 405, "only GET is supported\n");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/health" => respond(&mut stream, 200, "ok\n"),
        "/search" => {
            let q = query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == "q")
                .map(|(_, v)| pct_decode(v))
                .unwrap_or_default();
            let parsed = match search::parse_query(&q, tstamp::now_ms()) {
                Ok(p) => p,
                Err(e) => return respond(&mut stream, 400, &format!("bad query: {e}\n")),
            };
            let mut w = BufWriter::new(stream);
            w.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n",
            )?;
            let mut line = String::new();
            search::run(data_dir, &parsed, |ev, fields| {
                line.clear();
                event_json(&mut line, ev, fields);
                line.push('\n');
                let _ = w.write_all(line.as_bytes());
            })?;
            w.flush()
        }
        _ => respond(&mut stream, 404, "not found\n"),
    }
}

fn respond(stream: &mut TcpStream, code: u16, body: &str) -> io::Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Percent-decode a URL query value (`+` → space, `%XX` → byte).
pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                };
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h * 16 + l);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn json_escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

/// One event as a JSON object: _time, index-time fields, _raw.
pub fn event_json(out: &mut String, ev: &OwnedEvent, fields: &[String]) {
    out.push_str("{\"_time\":\"");
    out.push_str(&tstamp::to_iso(ev.time_ms));
    out.push('"');
    for (id, val) in &ev.metas {
        if let Some(name) = fields.get(*id as usize) {
            out.push_str(",\"");
            json_escape(out, name);
            out.push_str("\":\"");
            json_escape(out, &String::from_utf8_lossy(val));
            out.push('"');
        }
    }
    out.push_str(",\"_raw\":\"");
    json_escape(out, &String::from_utf8_lossy(&ev.raw));
    out.push_str("\"}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_decoding() {
        assert_eq!(pct_decode("a+b%20c%3Dd"), "a b c=d");
        assert_eq!(pct_decode("plain"), "plain");
        assert_eq!(pct_decode("bad%2"), "bad%2");
        assert_eq!(pct_decode("%zz"), "%zz");
    }

    #[test]
    fn json_escaping() {
        let mut s = String::new();
        json_escape(&mut s, "a\"b\\c\nd\te\u{1}");
        assert_eq!(s, "a\\\"b\\\\c\\nd\\te\\u0001");
    }

    #[test]
    fn event_json_shape() {
        let ev = OwnedEvent {
            time_ms: 1_789_467_072_345,
            metas: vec![(1, b"app".to_vec()), (2, b"web01".to_vec())],
            raw: b"hello \"world\"".to_vec(),
        };
        let fields = vec!["source".to_string(), "sourcetype".to_string(), "host".to_string()];
        let mut s = String::new();
        event_json(&mut s, &ev, &fields);
        assert_eq!(
            s,
            "{\"_time\":\"2026-09-15T10:11:12.345Z\",\"sourcetype\":\"app\",\"host\":\"web01\",\"_raw\":\"hello \\\"world\\\"\"}"
        );
    }
}
