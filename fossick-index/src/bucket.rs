//! Hot bucket writer and bucket scan reader.
//!
//! Bucket dir contents: journal.zj (+ .bidx while hot), events.tbl, manifest.
//! events.tbl: "FET1" + u32 count, then count × { u64 time_ms, u64 journal_addr }.
//! Records are appended per block flush so on-disk state always points at
//! readable journal blocks; count in the header is fixed up at finalize.

use crate::journal::{decode_event, JournalReader, JournalWriter};
use crate::manifest::Manifest;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const TBL_MAGIC: &[u8; 4] = b"FET1";
const SYNC_EVERY_BLOCKS: u32 = 4;

pub struct HotBucket {
    pub dir: PathBuf,
    pub man: Manifest,
    journal: JournalWriter,
    tbl: File,
    /// (time_ms, addr) for events whose block is not yet flushed.
    pending: Vec<(u64, u64)>,
    blocks_since_sync: u32,
    /// Uncompressed journal bytes appended, for rollover checks.
    pub uncomp_bytes: u64,
}

impl HotBucket {
    pub fn create(data_dir: &Path, id: u64, fields: Vec<String>) -> io::Result<HotBucket> {
        let dir = data_dir.join("main").join(format!("hot_{id}"));
        std::fs::create_dir_all(&dir)?;
        let journal = JournalWriter::create(&dir)?;
        let mut tbl = File::create(dir.join("events.tbl"))?;
        tbl.write_all(TBL_MAGIC)?;
        tbl.write_all(&0u32.to_le_bytes())?;
        let man = Manifest::new(id, fields);
        man.write(&dir)?;
        Ok(HotBucket { dir, man, journal, tbl, pending: Vec::new(), blocks_since_sync: 0, uncomp_bytes: 0 })
    }

    /// Append one event. `metas` are (key_id, value) pairs; key_ids come from
    /// `self.man.field_id()`. Flushes the journal block when full.
    pub fn add_event(&mut self, time_ms: u64, metas: &[(u8, &[u8])], raw: &[u8]) -> io::Result<()> {
        let before = self.journal.block_len();
        let addr = self.journal.append(time_ms, metas, raw);
        self.uncomp_bytes += (self.journal.block_len() - before) as u64;
        self.pending.push((time_ms, addr));
        self.man.event_count += 1;
        self.man.min_time_ms = self.man.min_time_ms.min(time_ms);
        self.man.max_time_ms = self.man.max_time_ms.max(time_ms);
        if self.journal.block_full() {
            self.flush()?;
        }
        Ok(())
    }

    /// Flush the open journal block and the matching events.tbl records; sync
    /// every few blocks. Called on block-full and by the serve timed flush.
    pub fn flush(&mut self) -> io::Result<()> {
        if !self.journal.block_dirty() {
            return Ok(());
        }
        self.journal.flush_block()?;
        let mut rec = Vec::with_capacity(self.pending.len() * 16);
        for (t, a) in &self.pending {
            rec.extend_from_slice(&t.to_le_bytes());
            rec.extend_from_slice(&a.to_le_bytes());
        }
        self.tbl.write_all(&rec)?;
        self.pending.clear();
        self.blocks_since_sync += 1;
        if self.blocks_since_sync >= SYNC_EVERY_BLOCKS {
            self.journal.sync()?;
            self.tbl.sync_data()?;
            self.blocks_since_sync = 0;
        }
        self.man.write(&self.dir)
    }
}

/// An event copied out of a block buffer (owned, block buffer can be reused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedEvent {
    pub time_ms: u64,
    pub metas: Vec<(u8, Vec<u8>)>,
    pub raw: Vec<u8>,
}

/// Brute-force scan of one bucket, one 256 KiB block in memory at a time.
/// `newest_first` iterates blocks in reverse and events within a block in
/// reverse (approximately reverse-time for in-order arrival).
pub struct BucketScan {
    pub man: Manifest,
    reader: JournalReader,
    newest_first: bool,
    min_ms: u64,
    max_ms: u64,
    /// Remaining block indices, in emit order (popped from the back).
    block_order: Vec<usize>,
    block: Vec<u8>,
    /// Events decoded from the current block, in emit order (popped from the back).
    current: Vec<OwnedEvent>,
}

impl BucketScan {
    pub fn open(dir: &Path, min_ms: u64, max_ms: u64, newest_first: bool) -> io::Result<BucketScan> {
        let man = Manifest::read(dir)?;
        let reader = JournalReader::open(dir)?;
        let mut block_order: Vec<usize> = (0..reader.blocks.len()).collect();
        if !newest_first {
            block_order.reverse(); // popped from back → ascending
        }
        Ok(BucketScan { man, reader, newest_first, min_ms, max_ms, block_order, block: Vec::new(), current: Vec::new() })
    }

    fn load_next_block(&mut self) -> io::Result<bool> {
        while let Some(i) = self.block_order.pop() {
            self.reader.read_block(i, &mut self.block)?;
            let mut off = 0;
            while off < self.block.len() {
                let (v, next) = decode_event(&self.block, off)?;
                if v.time_ms >= self.min_ms && v.time_ms <= self.max_ms {
                    self.current.push(OwnedEvent {
                        time_ms: v.time_ms,
                        metas: v.metas.iter().map(|(id, val)| (*id, val.to_vec())).collect(),
                        raw: v.raw.to_vec(),
                    });
                }
                off = next;
            }
            if !self.newest_first {
                self.current.reverse(); // popped from back → forward order
            }
            if !self.current.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Iterator for BucketScan {
    type Item = io::Result<OwnedEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(ev) = self.current.pop() {
                return Some(Ok(ev));
            }
            match self.load_next_block() {
                Ok(true) => {}
                Ok(false) => return None,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("fossick-bkt-{}", std::process::id()))
            .join(format!("{:?}", std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fields() -> Vec<String> {
        vec!["source".into(), "sourcetype".into(), "host".into()]
    }

    #[test]
    fn write_flush_scan_roundtrip() {
        let data = tmp();
        let mut b = HotBucket::create(&data, 0, fields()).unwrap();
        let st = b.man.field_id("sourcetype").unwrap();
        for i in 0u64..3000 {
            let raw = format!("line {i} needle_{}", i % 10);
            b.add_event(1000 + i, &[(st, b"app")], raw.as_bytes()).unwrap();
        }
        b.flush().unwrap();
        let dir = b.dir.clone();

        // Scan while still hot (reader uses the bidx sidecar).
        let evs: Vec<OwnedEvent> = BucketScan::open(&dir, 0, u64::MAX, false)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(evs.len(), 3000);
        assert_eq!(evs[0].time_ms, 1000);
        assert_eq!(evs[0].metas, vec![(st, b"app".to_vec())]);
        assert_eq!(String::from_utf8_lossy(&evs[2999].raw), "line 2999 needle_9");
    }

    #[test]
    fn scan_newest_first_and_time_clamp() {
        let data = tmp();
        let mut b = HotBucket::create(&data, 1, fields()).unwrap();
        for i in 0u64..3000 {
            b.add_event(1000 + i, &[], format!("e{i}").as_bytes()).unwrap();
        }
        b.flush().unwrap();
        let times: Vec<u64> = BucketScan::open(&b.dir, 1500, 1599, true)
            .unwrap()
            .map(|r| r.unwrap().time_ms)
            .collect();
        assert_eq!(times.len(), 100);
        assert_eq!(times[0], 1599);
        assert_eq!(times[99], 1500);
    }

    #[test]
    fn manifest_tracks_counts_and_span() {
        let data = tmp();
        let mut b = HotBucket::create(&data, 2, fields()).unwrap();
        b.add_event(500, &[], b"a").unwrap();
        b.add_event(100, &[], b"b").unwrap();
        b.flush().unwrap();
        let m = Manifest::read(&b.dir).unwrap();
        assert_eq!((m.event_count, m.min_time_ms, m.max_time_ms), (2, 100, 500));
    }

    #[test]
    fn empty_bucket_scans_empty() {
        let data = tmp();
        let b = HotBucket::create(&data, 3, fields()).unwrap();
        let n = BucketScan::open(&b.dir, 0, u64::MAX, true).unwrap().count();
        assert_eq!(n, 0);
    }
}
