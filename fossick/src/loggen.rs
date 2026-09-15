//! Test-harness load generator: pushes synthetic events over TCP.
//! Timestamps step one second per event, ending at "now"; every event
//! carries a `needle_<i%13>` term so searches can assert exact hit counts.

use crate::tstamp;
use std::io::{self, BufWriter, Write};
use std::net::TcpStream;

pub fn run(port: u16, count: u64) -> io::Result<()> {
    let stream = TcpStream::connect(("127.0.0.1", port))?;
    let mut w = BufWriter::with_capacity(64 * 1024, stream);
    let base = tstamp::now_ms();
    let start = std::time::Instant::now();
    for i in 0..count {
        let t = base.saturating_sub((count - i) * 1000);
        writeln!(
            w,
            "{} level=info svc=checkout amount={} needle_{} request completed",
            tstamp::to_iso(t),
            i % 997,
            i % 13
        )?;
    }
    w.flush()?;
    let secs = start.elapsed().as_secs_f64();
    eprintln!("loggen: sent {count} events in {secs:.2}s ({:.0} ev/s)", count as f64 / secs);
    Ok(())
}
