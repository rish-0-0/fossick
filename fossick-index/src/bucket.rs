//! Hot bucket writer and bucket scan reader.
//!
//! Bucket dir contents: journal.zj (+ .bidx while hot), events.tbl, manifest.
//! events.tbl: "FET1" + u32 count, then count × { u64 time_ms, u64 journal_addr }.
//! Records are appended per block flush so on-disk state always points at
//! readable journal blocks; count in the header is fixed up at finalize.

use crate::journal::{self, decode_event, JournalReader, JournalWriter};
use crate::manifest::{Manifest, State};
use crate::{bloom, tokenize, tsidx};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

type TermMap = HashMap<Box<[u8]>, Vec<u32>>;

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
    /// term → arrival-order ordinals; becomes the tsidx at finalize.
    terms: TermMap,
}

fn push_ord(map: &mut TermMap, term: &[u8], ord: u32) {
    match map.get_mut(term) {
        Some(v) => {
            if v.last() != Some(&ord) {
                v.push(ord);
            }
        }
        None => {
            map.insert(term.to_vec().into_boxed_slice(), vec![ord]);
        }
    }
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
        Ok(HotBucket {
            dir,
            man,
            journal,
            tbl,
            pending: Vec::new(),
            blocks_since_sync: 0,
            uncomp_bytes: 0,
            terms: TermMap::new(),
        })
    }

    /// Append one event. `metas` are (key_id, value) pairs; key_ids come from
    /// `self.man.field_id()`. Flushes the journal block when full.
    pub fn add_event(&mut self, time_ms: u64, metas: &[(u8, &[u8])], raw: &[u8]) -> io::Result<()> {
        let before = self.journal.block_len();
        let addr = self.journal.append(time_ms, metas, raw);
        self.uncomp_bytes += (self.journal.block_len() - before) as u64;
        self.pending.push((time_ms, addr));
        let ord = self.man.event_count as u32;
        tokenize::tokenize(raw, |tok| push_ord(&mut self.terms, tok, ord));
        for (id, val) in metas {
            let name = &self.man.fields[*id as usize];
            push_ord(&mut self.terms, &tokenize::field_term(name, val), ord);
        }
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

/// Hot→warm rollover thresholds (from fossick.conf [bucket]).
#[derive(Debug, Clone, Copy)]
pub struct RollLimits {
    pub max_bytes: u64,
    pub max_events: u64,
    pub max_span_ms: u64,
}

impl HotBucket {
    pub fn should_roll(&self, l: &RollLimits) -> bool {
        self.man.event_count > 0
            && (self.uncomp_bytes >= l.max_bytes
                || self.man.event_count >= l.max_events
                || self.man.max_time_ms - self.man.min_time_ms >= l.max_span_ms)
    }

    /// Hot→warm: flush, write the journal trailer, then seal (time-sort
    /// events.tbl, remap+write tsidx and bloom, warm manifest, db_* rename).
    pub fn finalize(mut self) -> io::Result<PathBuf> {
        self.flush()?;
        self.journal.finalize()?;
        drop(self.tbl);
        let recs = read_events_tbl(&self.dir)?; // arrival order
        seal(&self.dir, &mut self.man, recs, std::mem::take(&mut self.terms))
    }
}

/// Common hot→warm sealing for finalize and crash recovery. `recs` is the
/// event table in arrival order (index = journal ordinal); `terms` maps
/// term → arrival ordinals. Sorts by time, remaps ordinals to time order,
/// writes events.tbl + index.tsidx + bloom.bf, renames to db_*.
fn seal(dir: &Path, man: &mut Manifest, recs: Vec<(u64, u64)>, terms: TermMap) -> io::Result<PathBuf> {
    let n = recs.len();
    let mut idx: Vec<u32> = (0..n as u32).collect();
    idx.sort_by_key(|&i| recs[i as usize].0);
    let sorted: Vec<(u64, u64)> = idx.iter().map(|&i| recs[i as usize]).collect();
    write_events_tbl(dir, &sorted)?;
    let mut new_of_old = vec![0u32; n];
    for (new, &old) in idx.iter().enumerate() {
        new_of_old[old as usize] = new as u32;
    }
    let mut term_vec: Vec<(Box<[u8]>, Vec<u32>)> = terms
        .into_iter()
        .map(|(t, ords)| {
            let mut m: Vec<u32> = ords.into_iter().map(|o| new_of_old[o as usize]).collect();
            m.sort_unstable();
            m.dedup();
            (t, m)
        })
        .collect();
    tsidx::write(dir, &mut term_vec)?;
    bloom::write(dir, term_vec.iter().map(|(t, _)| t.as_ref()), term_vec.len())?;
    man.event_count = n as u64;
    if let (Some(first), Some(last)) = (sorted.first(), sorted.last()) {
        man.min_time_ms = first.0;
        man.max_time_ms = last.0;
    }
    man.state = State::Warm;
    man.write(dir)?;
    rename_to_db(dir, man)
}

/// Read events.tbl records; when `count` in the header is 0 the bucket is
/// hot and the record count is derived from the file length.
pub fn read_events_tbl(dir: &Path) -> io::Result<Vec<(u64, u64)>> {
    let bytes = std::fs::read(dir.join("events.tbl"))?;
    if bytes.len() < 8 || &bytes[0..4] != TBL_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad events.tbl header"));
    }
    let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let n = if count == 0 { (bytes.len() - 8) / 16 } else { count };
    let mut out = Vec::with_capacity(n);
    for c in bytes[8..8 + n * 16].chunks_exact(16) {
        out.push((
            u64::from_le_bytes(c[0..8].try_into().unwrap()),
            u64::from_le_bytes(c[8..16].try_into().unwrap()),
        ));
    }
    Ok(out)
}

fn write_events_tbl(dir: &Path, recs: &[(u64, u64)]) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(8 + recs.len() * 16);
    bytes.extend_from_slice(TBL_MAGIC);
    bytes.extend_from_slice(&(recs.len() as u32).to_le_bytes());
    for (t, a) in recs {
        bytes.extend_from_slice(&t.to_le_bytes());
        bytes.extend_from_slice(&a.to_le_bytes());
    }
    let mut f = File::create(dir.join("events.tbl"))?;
    f.write_all(&bytes)?;
    f.sync_data()
}

/// Rename hot_<id> → db_<max>_<min>_<id>, retrying briefly for Windows
/// sharing violations (a searcher may hold the files open).
fn rename_to_db(dir: &Path, man: &Manifest) -> io::Result<PathBuf> {
    let dst = dir
        .parent()
        .unwrap()
        .join(format!("db_{}_{}_{}", man.max_time_ms, man.min_time_ms, man.id));
    let mut last = None;
    for _ in 0..20 {
        match std::fs::rename(dir, &dst) {
            Ok(()) => return Ok(dst),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err(last.unwrap())
}

/// Crash recovery for an orphaned hot dir: rebuild the journal trailer from
/// the bidx sidecar, rebuild events.tbl and manifest counts by scanning the
/// journal, then finalize to warm. Empty buckets are deleted. Returns the
/// warm dir, or None if the bucket was empty.
pub fn recover_hot_dir(dir: &Path) -> io::Result<Option<PathBuf>> {
    let mut man = Manifest::read(dir)?;
    let blocks = journal::recover(dir)?;
    if blocks.is_empty() {
        std::fs::remove_dir_all(dir)?;
        return Ok(None);
    }
    let mut reader = JournalReader::open(dir)?;
    let mut recs: Vec<(u64, u64)> = Vec::new();
    let mut terms = TermMap::new();
    let mut block = Vec::new();
    for i in 0..reader.blocks.len() {
        reader.read_block(i, &mut block)?;
        let mut off = 0;
        while off < block.len() {
            let (v, next) = decode_event(&block, off)?;
            let ord = recs.len() as u32;
            tokenize::tokenize(v.raw, |tok| push_ord(&mut terms, tok, ord));
            for (id, val) in &v.metas {
                if let Some(name) = man.fields.get(*id as usize) {
                    push_ord(&mut terms, &tokenize::field_term(name, val), ord);
                }
            }
            recs.push((v.time_ms, ((i as u64) << 32) | off as u64));
            off = next;
        }
    }
    drop(reader);
    seal(dir, &mut man, recs, terms).map(Some)
}

/// mmap view of events.tbl: ordinal → (time_ms, journal addr), plus the
/// binary-searched ordinal range for a time clamp (valid once time-sorted).
pub struct EventsTbl {
    map: memmap2::Mmap,
    count: usize,
}

impl EventsTbl {
    pub fn open(dir: &Path) -> io::Result<EventsTbl> {
        let file = File::open(dir.join("events.tbl"))?;
        let map = unsafe { memmap2::Mmap::map(&file)? };
        if map.len() < 8 || &map[0..4] != TBL_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad events.tbl header"));
        }
        let count = u32::from_le_bytes(map[4..8].try_into().unwrap()) as usize;
        let count = if count == 0 { (map.len() - 8) / 16 } else { count };
        Ok(EventsTbl { map, count })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// (time_ms, journal_addr) at ordinal `i`.
    pub fn get(&self, i: usize) -> (u64, u64) {
        let o = 8 + i * 16;
        (
            u64::from_le_bytes(self.map[o..o + 8].try_into().unwrap()),
            u64::from_le_bytes(self.map[o + 8..o + 16].try_into().unwrap()),
        )
    }

    /// Half-open ordinal range [lo, hi) whose times fall in [min_ms, max_ms].
    pub fn ord_range(&self, min_ms: u64, max_ms: u64) -> (usize, usize) {
        let lower = |target: u64| {
            let (mut lo, mut hi) = (0, self.count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if self.get(mid).0 < target {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };
        (lower(min_ms), lower(max_ms.saturating_add(1)))
    }
}

/// Small LRU of decompressed journal blocks for postings-driven reads.
/// Bounds read memory to MAX_SLOTS × 256 KiB per open bucket.
pub struct BlockCache {
    reader: JournalReader,
    slots: Vec<(u32, Vec<u8>)>,
}

const MAX_SLOTS: usize = 4;

impl BlockCache {
    pub fn open(dir: &Path) -> io::Result<BlockCache> {
        Ok(BlockCache { reader: JournalReader::open(dir)?, slots: Vec::new() })
    }

    /// Decode the event at a journal address.
    pub fn event(&mut self, addr: u64) -> io::Result<OwnedEvent> {
        let blk = (addr >> 32) as u32;
        let off = (addr & 0xffff_ffff) as usize;
        let idx = match self.slots.iter().position(|(b, _)| *b == blk) {
            Some(i) => i,
            None => {
                let mut buf = if self.slots.len() >= MAX_SLOTS { self.slots.pop().unwrap().1 } else { Vec::new() };
                self.reader.read_block(blk as usize, &mut buf)?;
                self.slots.insert(0, (blk, buf));
                0
            }
        };
        if idx != 0 {
            let s = self.slots.remove(idx);
            self.slots.insert(0, s);
        }
        let (v, _) = decode_event(&self.slots[0].1, off)?;
        Ok(OwnedEvent {
            time_ms: v.time_ms,
            metas: v.metas.iter().map(|(id, val)| (*id, val.to_vec())).collect(),
            raw: v.raw.to_vec(),
        })
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
    fn finalize_sorts_seals_and_renames() {
        let data = tmp();
        let mut b = HotBucket::create(&data, 5, fields()).unwrap();
        for i in (0u64..1000).rev() {
            b.add_event(1000 + i, &[], format!("e{i}").as_bytes()).unwrap(); // reverse arrival
        }
        let dir = b.finalize().unwrap();
        assert_eq!(dir.file_name().unwrap().to_string_lossy(), "db_1999_1000_5");
        assert!(!dir.join("journal.zj.bidx").exists());
        let recs = read_events_tbl(&dir).unwrap();
        assert_eq!(recs.len(), 1000);
        assert!(recs.windows(2).all(|w| w[0].0 <= w[1].0), "events.tbl must be time-sorted");
        assert_eq!(Manifest::read(&dir).unwrap().state, State::Warm);
        assert_eq!(BucketScan::open(&dir, 0, u64::MAX, false).unwrap().count(), 1000);
    }

    #[test]
    fn crash_recovery_rebuilds_bucket() {
        let data = tmp();
        let mut b = HotBucket::create(&data, 9, fields()).unwrap();
        for i in 0u64..2500 {
            b.add_event(1000 + i, &[], format!("e{i}").as_bytes()).unwrap();
        }
        b.flush().unwrap();
        let dir = b.dir.clone();
        drop(b); // "crash": no finalize
        // Torn final block and a corrupt events.tbl must both be repaired.
        use std::io::Write as _;
        std::fs::OpenOptions::new().append(true).open(dir.join("journal.zj")).unwrap()
            .write_all(b"torn-partial-block-garbage").unwrap();
        std::fs::write(dir.join("events.tbl"), b"FET1\0\0\0\0junk").unwrap();

        let warm = recover_hot_dir(&dir).unwrap().unwrap();
        let man = Manifest::read(&warm).unwrap();
        assert_eq!(man.state, State::Warm);
        assert_eq!((man.event_count, man.min_time_ms, man.max_time_ms), (2500, 1000, 3499));
        assert_eq!(read_events_tbl(&warm).unwrap().len(), 2500);
        let n = BucketScan::open(&warm, 0, u64::MAX, false).unwrap().map(|r| r.unwrap()).count();
        assert_eq!(n, 2500);
    }

    #[test]
    fn finalize_builds_tsidx_and_bloom() {
        let data = tmp();
        let mut b = HotBucket::create(&data, 7, fields()).unwrap();
        let st = b.man.field_id("sourcetype").unwrap();
        // Reverse arrival order so ordinal remapping is actually exercised.
        for i in (0u64..500).rev() {
            b.add_event(1000 + i, &[(st, b"App")], format!("msg Alpha_{i} done").as_bytes()).unwrap();
        }
        let dir = b.finalize().unwrap();

        let idx = crate::tsidx::Tsidx::open(&dir).unwrap();
        // Token lookup: postings must be time-ordered ordinals.
        let ords: Vec<u32> = idx.lookup(b"alpha_42").unwrap().collect();
        assert_eq!(ords, vec![42]); // time 1042 → 43rd-smallest time → ordinal 42
        assert_eq!(idx.lookup(b"done").unwrap().count(), 500);
        assert!(idx.lookup(b"Alpha_42").is_none()); // terms are lowercased
        // Composite field term (value lowercased).
        assert_eq!(idx.lookup(b"sourcetype::app").unwrap().count(), 500);

        let bloom = crate::bloom::Bloom::open(&dir).unwrap().unwrap();
        assert!(bloom.may_contain(b"alpha_42"));
        assert!(bloom.may_contain(b"sourcetype::app"));

        // events.tbl ordinal → journal addr agrees with tsidx ordinals.
        let tbl = EventsTbl::open(&dir).unwrap();
        assert_eq!(tbl.len(), 500);
        let (t, addr) = tbl.get(42);
        assert_eq!(t, 1042);
        let mut cache = BlockCache::open(&dir).unwrap();
        let ev = cache.event(addr).unwrap();
        assert_eq!(String::from_utf8_lossy(&ev.raw), "msg Alpha_42 done");
        // Time clamp range.
        assert_eq!(tbl.ord_range(1100, 1199), (100, 200));
        assert_eq!(tbl.ord_range(0, u64::MAX), (0, 500));
    }

    #[test]
    fn recovery_deletes_empty_hot_dir() {
        let data = tmp();
        let b = HotBucket::create(&data, 3, fields()).unwrap();
        let dir = b.dir.clone();
        drop(b);
        assert!(recover_hot_dir(&dir).unwrap().is_none());
        assert!(!dir.exists());
    }

    #[test]
    fn should_roll_thresholds() {
        let data = tmp();
        let mut b = HotBucket::create(&data, 4, fields()).unwrap();
        let l = RollLimits { max_bytes: u64::MAX, max_events: 10, max_span_ms: u64::MAX };
        assert!(!b.should_roll(&l));
        for i in 0..10 {
            b.add_event(i, &[], b"x").unwrap();
        }
        assert!(b.should_roll(&l));
        let span = RollLimits { max_bytes: u64::MAX, max_events: u64::MAX, max_span_ms: 5 };
        assert!(b.should_roll(&span)); // span 0..9 ms ≥ 5
    }

    #[test]
    fn empty_bucket_scans_empty() {
        let data = tmp();
        let b = HotBucket::create(&data, 3, fields()).unwrap();
        let n = BucketScan::open(&b.dir, 0, u64::MAX, true).unwrap().count();
        assert_eq!(n, 0);
    }
}
