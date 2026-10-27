//! TCP ingest: per-connection reader threads feed a single indexer thread
//! that owns the hot bucket (single writer, no locks). Lines longer than
//! `[limits] max_line_bytes` (default 1 MiB) are truncated before indexing.
//!
//! **Graceful shutdown:** `fossick serve` installs a SIGINT/SIGTERM handler that
//! stops accepting new TCP connections (the accept backlog is drained once),
//! closes idle long-lived ingest reads, drains the ingest queue, flushes the hot
//! bucket, then exits. The HTTP API thread is not joined (loopback-only today).
//! Abrupt kill (`kill -9`) may lose at most the current unflushed journal block.
//!
//! **UTF-8 policy:** ingest treats each line as opaque bytes. Invalid UTF-8 is
//! stored verbatim in the journal; search/API display uses lossy UTF-8 conversion
//! (`from_utf8_lossy`) and does not reject or rewrite bytes at ingest time.

use crate::conf::{Conf, TcpInput};
use crate::tstamp;
use fossick_index::bucket::{HotBucket, RollLimits};
use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

const FLUSH_EVERY: Duration = Duration::from_secs(1);
const ACCEPT_POLL: Duration = Duration::from_millis(50);
const READ_POLL: Duration = Duration::from_millis(100);

/// Cooperative shutdown flag shared by listeners and `fossick serve`.
#[derive(Clone)]
pub struct Shutdown {
    flag: Arc<AtomicBool>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    pub fn new() -> Self {
        Self { flag: Arc::new(AtomicBool::new(false)) }
    }

    pub fn requested(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    pub fn request(&self) {
        self.flag.store(true, Ordering::Release);
    }
}

/// SIGINT/SIGTERM request graceful ingest drain (Unix only; no-op elsewhere).
pub fn install_shutdown_handler(shutdown: Shutdown) {
    shutdown_signals::install(shutdown);
}

#[cfg(unix)]
mod shutdown_signals {
    use super::Shutdown;
    use libc::{c_int, sigaction, sigemptyset, sigset_t, SA_RESTART, SIGINT, SIGTERM};
    use std::mem::MaybeUninit;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, OnceLock};

    static FLAG: OnceLock<Arc<std::sync::atomic::AtomicBool>> = OnceLock::new();

    // SAFETY: async-signal-safe; only sets an atomic flag installed before handlers run.
    extern "C" fn on_signal(_: c_int) {
        if let Some(flag) = FLAG.get() {
            flag.store(true, Ordering::Release);
        }
    }

    pub fn install(shutdown: Shutdown) {
        let _ = FLAG.set(shutdown.flag);
        let sa = new_sigaction();
        for sig in [SIGINT, SIGTERM] {
            // SAFETY: `sa` points at a valid `sigaction`; prior handlers are replaced.
            if unsafe { sigaction(sig, &sa, std::ptr::null_mut()) } != 0 {
                eprintln!("fossick: warning: could not install handler for signal {sig}");
            }
        }
        eprintln!("fossick: press Ctrl+C or send SIGTERM for graceful shutdown");
    }

    fn new_sigaction() -> libc::sigaction {
        let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
        sa.sa_sigaction = on_signal as *const () as usize;
        sa.sa_mask = empty_sigset();
        sa.sa_flags = SA_RESTART;
        #[cfg(target_os = "linux")]
        {
            sa.sa_restorer = None;
        }
        sa
    }

    fn empty_sigset() -> sigset_t {
        let mut set = MaybeUninit::<sigset_t>::uninit();
        // SAFETY: `sigemptyset` initializes `set` on success (always for a fresh set).
        unsafe {
            sigemptyset(set.as_mut_ptr());
            set.assume_init()
        }
    }
}

#[cfg(not(unix))]
mod shutdown_signals {
    use super::Shutdown;

    pub fn install(_shutdown: Shutdown) {}
}

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
    let shutdown = Shutdown::new();
    install_shutdown_handler(shutdown.clone());
    if conf.api_port != 0 {
        let api = TcpListener::bind(("127.0.0.1", conf.api_port))?;
        eprintln!("fossick: web UI + search API on http://127.0.0.1:{}/", conf.api_port);
        crate::http::spawn(api, conf.clone());
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let mut listeners = Vec::new();
    for input in conf.inputs.clone() {
        let listener = TcpListener::bind(("127.0.0.1", input.port))?;
        eprintln!("fossick: listening on 127.0.0.1:{} (sourcetype={})", input.port, input.sourcetype);
        listeners.push(spawn_listener(listener, input, conf.max_line_bytes, tx.clone(), shutdown.clone()));
    }
    drop(tx);
    run_indexer(&conf, rx)?;
    for h in listeners {
        let _ = h.join();
    }
    Ok(())
}

pub fn spawn_listener(
    listener: TcpListener,
    input: TcpInput,
    max_line_bytes: usize,
    tx: Sender<IngestEvent>,
    shutdown: Shutdown,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let _ = listener.set_nonblocking(true);
        let accept_one = |stream: TcpStream| {
            let _ = stream.set_nonblocking(false);
            let input = input.clone();
            let tx = tx.clone();
            let conn_shutdown = shutdown.clone();
            std::thread::spawn(move || handle_conn(stream, &input, max_line_bytes, tx, conn_shutdown));
        };
        while !shutdown.requested() {
            match listener.accept() {
                Ok((stream, _)) => accept_one(stream),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(_) => break,
            }
        }
        // Accept connections already in the backlog so their readers keep a sender clone.
        while let Ok((stream, _)) = listener.accept() {
            accept_one(stream);
        }
    })
}

/// Read newline-delimited events from one connection until EOF or graceful shutdown.
pub fn handle_conn(
    mut stream: TcpStream,
    input: &TcpInput,
    max_line_bytes: usize,
    tx: Sender<IngestEvent>,
    shutdown: Shutdown,
) {
    let _ = stream.set_read_timeout(Some(READ_POLL));
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
            Ok(0) => break,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                if shutdown.requested() {
                    break;
                }
                continue;
            }
            Err(_) => break,
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
            if line.len() > max_line_bytes {
                line = &line[..max_line_bytes];
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
        if buf.len() > max_line_bytes {
            // Oversized line: index the first max_line_bytes, drop the rest.
            let _ = send_line(&buf[..max_line_bytes], &source, &sourcetype, &host, &tx);
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

/// Single-writer indexer loop; flushes the hot bucket at least every second,
/// recovers orphaned hot buckets at startup, rolls hot→warm at the limits.
pub fn run_indexer(conf: &Conf, rx: Receiver<IngestEvent>) -> io::Result<()> {
    for b in fossick_index::list_buckets(&conf.data_dir)? {
        if b.hot {
            fossick_index::bucket::recover_hot_dir(&b.dir)?;
        }
    }
    let limits = RollLimits {
        max_bytes: conf.hot_max_mb * 1024 * 1024,
        max_events: conf.hot_max_events,
        max_span_ms: conf.hot_max_hours * 3_600_000,
    };
    let fields = base_fields(conf);
    let id = fossick_index::list_buckets(&conf.data_dir)?.iter().map(|b| b.id + 1).max().unwrap_or(0);
    let mut hot = HotBucket::create(&conf.data_dir, id, fields.clone())?;
    loop {
        match rx.recv_timeout(FLUSH_EVERY) {
            Ok(ev) => {
                let metas = event_metas(conf, &ev);
                hot.add_event(ev.time_ms, &metas, &ev.raw)?;
                if hot.should_roll(&limits) {
                    let next_id = hot.man.id + 1;
                    let full = std::mem::replace(&mut hot, HotBucket::create(&conf.data_dir, next_id, fields.clone())?);
                    full.finalize()?;
                }
            }
            Err(RecvTimeoutError::Timeout) => hot.flush()?,
            Err(RecvTimeoutError::Disconnected) => return hot.flush(),
        }
    }
}

fn event_metas<'a>(conf: &'a Conf, ev: &'a IngestEvent) -> Vec<(u8, &'a [u8])> {
    let time_source_id = (3 + conf.indexed_fields.len()) as u8;
    let mut metas: Vec<(u8, &[u8])> =
        vec![(0, ev.source.as_bytes()), (1, ev.sourcetype.as_bytes()), (2, ev.host.as_bytes())];
    for (i, name) in conf.indexed_fields.iter().enumerate() {
        if let Some(val) = kv_extract(&ev.raw, name.as_bytes()) {
            metas.push((3 + i as u8, val));
        }
    }
    if ev.from_receipt {
        metas.push((time_source_id, b"receipt"));
    }
    metas
}

/// Find `name=value` or `name="value"` in a raw line (index-time kv fields).
pub fn kv_extract<'a>(raw: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut from = 0;
    while from + name.len() < raw.len() {
        let i = from
            + raw[from..].windows(name.len() + 1).position(|w| &w[..name.len()] == name && w[name.len()] == b'=')?;
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
    fn invalid_utf8_line_bytes_preserved() {
        let (tx, rx) = std::sync::mpsc::channel();
        let source: Arc<str> = "s".into();
        let st: Arc<str> = "st".into();
        let host: Arc<str> = "h".into();
        let line = b"2026-09-15T10:00:00Z needle\xff\xfe";
        send_line(line, &source, &st, &host, &tx).unwrap();
        let ev = rx.recv().unwrap();
        assert_eq!(ev.raw, line);
    }

    #[test]
    fn handle_conn_exits_when_shutdown_requested_without_eof() {
        let shutdown = Shutdown::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        let input = TcpInput { port: 0, source: "s".into(), sourcetype: "st".into(), host: "h".into() };
        let conn_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = stream.set_nonblocking(false);
            handle_conn(stream, &input, 4096, tx, conn_shutdown);
        });
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        use std::io::Write;
        writeln!(client, "2026-09-15T10:00:00Z keepalive_line").unwrap();
        client.flush().unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(ev.raw.windows(b"keepalive_line".len()).any(|w| w == b"keepalive_line"));
        shutdown.request();
        server.join().unwrap();
        drop(rx);
    }

    #[cfg(unix)]
    #[test]
    fn sigterm_requests_shutdown() {
        let shutdown = Shutdown::new();
        install_shutdown_handler(shutdown.clone());
        // SAFETY: raises SIGTERM in-process; handler only sets an atomic bool.
        unsafe {
            libc::raise(libc::SIGTERM);
        }
        for _ in 0..100 {
            if shutdown.requested() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("SIGTERM did not set shutdown flag");
    }

    #[test]
    fn shutdown_stops_listener_accept_loop() {
        let shutdown = Shutdown::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let input = TcpInput { port: 0, source: "s".into(), sourcetype: "st".into(), host: "h".into() };
        let handle = spawn_listener(listener, input, 4096, tx, shutdown.clone());
        shutdown.request();
        handle.join().unwrap();
    }

    #[test]
    fn indexer_final_flush_persists_without_periodic_timer() {
        let dir = std::env::temp_dir().join(format!("fossick-shutdown-flush-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let conf = Conf { data_dir: dir, ..Conf::default() };
        let (tx, rx) = std::sync::mpsc::channel();
        let source: Arc<str> = "s".into();
        let st: Arc<str> = "st".into();
        let host: Arc<str> = "h".into();
        send_line(b"2026-09-15T10:00:00Z shutdown_flush_marker", &source, &st, &host, &tx).unwrap();
        drop(tx);
        run_indexer(&conf, rx).unwrap();
        let hits = crate::search::collect(&conf.data_dir, "shutdown_flush_marker", tstamp::now_ms()).unwrap();
        assert_eq!(hits.len(), 1);
    }

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
