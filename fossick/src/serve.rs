//! TCP ingest: per-connection reader threads feed a single indexer thread
//! that owns the hot bucket (single writer, no locks).

use crate::conf::{Conf, TcpInput};
use crate::tstamp;
use fossick_index::bucket::HotBucket;
use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

const MAX_LINE: usize = 1 << 20;
const FLUSH_EVERY: Duration = Duration::from_secs(1);

pub struct IngestEvent {
    pub time_ms: u64,
    pub from_receipt: bool,
    pub source: Arc<str>,
    pub sourcetype: Arc<str>,
    pub host: Arc<str>,
    pub raw: Vec<u8>,
}

/// Index-time field table: source/sourcetype/host, configured kv fields,
/// then _time_source (set to "receipt" when no timestamp was parsed).
pub fn base_fields(conf: &Conf) -> Vec<String> {
    let mut f = vec!["source".to_string(), "sourcetype".to_string(), "host".to_string()];
    f.extend(conf.indexed_fields.iter().cloned());
    f.push("_time_source".to_string());
    f
}

pub fn run(conf: Conf) -> io::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    for input in conf.inputs.clone() {
        let listener = TcpListener::bind(("127.0.0.1", input.port))?;
        eprintln!("fossick: listening on 127.0.0.1:{} (sourcetype={})", input.port, input.sourcetype);
        spawn_listener(listener, input, tx.clone());
    }
    drop(tx);
    run_indexer(&conf, rx)
}

pub fn spawn_listener(listener: TcpListener, input: TcpInput, tx: Sender<IngestEvent>) {
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let input = input.clone();
            let tx = tx.clone();
            std::thread::spawn(move || handle_conn(stream, &input, tx));
        }
    });
}

/// Read newline-delimited events from one connection until EOF.
pub fn handle_conn(mut stream: TcpStream, input: &TcpInput, tx: Sender<IngestEvent>) {
    let peer = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let mut source: Arc<str> = input.source.as_str().into();
    let mut sourcetype: Arc<str> = input.sourcetype.as_str().into();
    let mut host: Arc<str> = if input.host == "%REMOTE_IP%" { peer.into() } else { input.host.as_str().into() };

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut first_line = true;
    let mut skipping_oversize = false;
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        let mut start = 0;
        while let Some(pos) = buf[start..].iter().position(|&b| b == b'\n') {
            let mut line = &buf[start..start + pos];
            start += pos + 1;
            if skipping_oversize {
                skipping_oversize = false; // tail of an already-truncated line
                continue;
            }
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            if line.is_empty() {
                continue;
            }
            if first_line {
                first_line = false;
                if let Some(hdr) = line.strip_prefix(b"##fossick ".as_slice()) {
                    for kv in String::from_utf8_lossy(hdr).split_whitespace() {
                        if let Some((k, v)) = kv.split_once('=') {
                            match k {
                                "source" => source = v.into(),
                                "sourcetype" => sourcetype = v.into(),
                                "host" => host = v.into(),
                                _ => {}
                            }
                        }
                    }
                    continue;
                }
            }
            if send_line(line, &source, &sourcetype, &host, &tx).is_err() {
                return; // indexer gone
            }
        }
        buf.drain(..start);
        if buf.len() > MAX_LINE {
            // Oversized line: index the first MAX_LINE bytes, drop the rest.
            let _ = send_line(&buf[..MAX_LINE], &source, &sourcetype, &host, &tx);
            buf.clear();
            skipping_oversize = true;
        }
    }
    // Trailing data without a final newline is still an event.
    if !buf.is_empty() && !skipping_oversize {
        let _ = send_line(&buf, &source, &sourcetype, &host, &tx);
    }
}

fn send_line(
    line: &[u8],
    source: &Arc<str>,
    sourcetype: &Arc<str>,
    host: &Arc<str>,
    tx: &Sender<IngestEvent>,
) -> Result<(), ()> {
    let now = tstamp::now_ms();
    let (time_ms, from_receipt) = match tstamp::extract(line, now) {
        Some(t) => (t, false),
        None => (now, true),
    };
    tx.send(IngestEvent {
        time_ms,
        from_receipt,
        source: source.clone(),
        sourcetype: sourcetype.clone(),
        host: host.clone(),
        raw: line.to_vec(),
    })
    .map_err(|_| ())
}

/// Single-writer indexer loop; flushes the hot bucket at least every second.
pub fn run_indexer(conf: &Conf, rx: Receiver<IngestEvent>) -> io::Result<()> {
    let fields = base_fields(conf);
    let id = fossick_index::list_buckets(&conf.data_dir)?.iter().map(|b| b.id + 1).max().unwrap_or(0);
    let mut hot = HotBucket::create(&conf.data_dir, id, fields)?;
    let time_source_id = (3 + conf.indexed_fields.len()) as u8;
    loop {
        match rx.recv_timeout(FLUSH_EVERY) {
            Ok(ev) => {
                let mut metas: Vec<(u8, &[u8])> = vec![
                    (0, ev.source.as_bytes()),
                    (1, ev.sourcetype.as_bytes()),
                    (2, ev.host.as_bytes()),
                ];
                for (i, name) in conf.indexed_fields.iter().enumerate() {
                    if let Some(val) = kv_extract(&ev.raw, name.as_bytes()) {
                        metas.push((3 + i as u8, val));
                    }
                }
                if ev.from_receipt {
                    metas.push((time_source_id, b"receipt"));
                }
                hot.add_event(ev.time_ms, &metas, &ev.raw)?;
            }
            Err(RecvTimeoutError::Timeout) => hot.flush()?,
            Err(RecvTimeoutError::Disconnected) => return hot.flush(),
        }
    }
}

/// Find `name=value` or `name="value"` in a raw line (index-time kv fields).
pub fn kv_extract<'a>(raw: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut from = 0;
    while from + name.len() + 1 <= raw.len() {
        let i = from + raw[from..].windows(name.len() + 1).position(|w| &w[..name.len()] == name && w[name.len()] == b'=')?;
        let boundary_ok = i == 0 || !(raw[i - 1].is_ascii_alphanumeric() || raw[i - 1] == b'_');
        if !boundary_ok {
            from = i + 1;
            continue;
        }
        let v = &raw[i + name.len() + 1..];
        return Some(if v.first() == Some(&b'"') {
            let end = v[1..].iter().position(|&b| b == b'"').map(|p| p + 1).unwrap_or(v.len());
            &v[1..end]
        } else {
            let end = v.iter().position(|b| b.is_ascii_whitespace()).unwrap_or(v.len());
            &v[..end]
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_extraction() {
        let raw = b"ts=1 level=info msg=\"hello world\" xlevel=bad n=5";
        assert_eq!(kv_extract(raw, b"level"), Some(&b"info"[..]));
        assert_eq!(kv_extract(raw, b"msg"), Some(&b"hello world"[..]));
        assert_eq!(kv_extract(raw, b"n"), Some(&b"5"[..]));
        assert_eq!(kv_extract(raw, b"missing"), None);
        // xlevel=bad must not match `level` (word boundary).
        assert_eq!(kv_extract(b"xlevel=bad", b"level"), None);
        assert_eq!(kv_extract(b"level=", b"level"), Some(&b""[..]));
    }
}
