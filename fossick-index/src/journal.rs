//! journal.zj: raw events in independently-compressed zstd blocks.
//!
//! Layout: 16B header | blocks (one zstd frame each) | block index table | 16B trailer.
//! While hot, the table lives in the `journal.zj.bidx` sidecar (appended per block flush)
//! so readers can address blocks before the trailer exists. An event address packs
//! `(block_no << 32) | offset_in_block`.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAGIC_HDR: &[u8; 4] = b"FZJ1";
const MAGIC_END: &[u8; 4] = b"FZJE";
/// A block closes at 256 KiB uncompressed or 2048 events — this is the hard
/// per-block decode-memory bound for every reader.
pub const BLOCK_BYTES: usize = 256 * 1024;
pub const BLOCK_EVENTS: u32 = 2048;
const ENTRY_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockEntry {
    pub file_offset: u64,
    pub comp_len: u32,
    pub uncomp_len: u32,
    pub min_time_ms: u64,
    pub first_event_ord: u32,
}

impl BlockEntry {
    fn encode(&self) -> [u8; ENTRY_LEN] {
        let mut b = [0u8; ENTRY_LEN];
        b[0..8].copy_from_slice(&self.file_offset.to_le_bytes());
        b[8..12].copy_from_slice(&self.comp_len.to_le_bytes());
        b[12..16].copy_from_slice(&self.uncomp_len.to_le_bytes());
        b[16..24].copy_from_slice(&self.min_time_ms.to_le_bytes());
        b[24..28].copy_from_slice(&self.first_event_ord.to_le_bytes());
        b
    }

    fn decode(b: &[u8]) -> BlockEntry {
        BlockEntry {
            file_offset: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            comp_len: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            uncomp_len: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            min_time_ms: u64::from_le_bytes(b[16..24].try_into().unwrap()),
            first_event_ord: u32::from_le_bytes(b[24..28].try_into().unwrap()),
        }
    }
}

/// Encode one event record into `buf`. Meta values are capped at u16::MAX bytes.
pub fn encode_event(buf: &mut Vec<u8>, time_ms: u64, metas: &[(u8, &[u8])], raw: &[u8]) {
    buf.extend_from_slice(&time_ms.to_le_bytes());
    buf.push(metas.len() as u8);
    for (id, val) in metas {
        let val = &val[..val.len().min(u16::MAX as usize)];
        buf.push(*id);
        buf.extend_from_slice(&(val.len() as u16).to_le_bytes());
        buf.extend_from_slice(val);
    }
    buf.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    buf.extend_from_slice(raw);
}

/// A decoded event borrowing from a block buffer.
pub struct EventView<'a> {
    pub time_ms: u64,
    pub metas: Vec<(u8, &'a [u8])>,
    pub raw: &'a [u8],
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Decode the event at `off`; returns the view and the offset of the next record.
pub fn decode_event(buf: &[u8], off: usize) -> io::Result<(EventView<'_>, usize)> {
    let need = |end: usize| if end > buf.len() { Err(bad("journal record truncated")) } else { Ok(()) };
    need(off + 9)?;
    let time_ms = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
    let n_meta = buf[off + 8] as usize;
    let mut p = off + 9;
    let mut metas = Vec::with_capacity(n_meta);
    for _ in 0..n_meta {
        need(p + 3)?;
        let id = buf[p];
        let len = u16::from_le_bytes(buf[p + 1..p + 3].try_into().unwrap()) as usize;
        need(p + 3 + len)?;
        metas.push((id, &buf[p + 3..p + 3 + len]));
        p += 3 + len;
    }
    need(p + 4)?;
    let raw_len = u32::from_le_bytes(buf[p..p + 4].try_into().unwrap()) as usize;
    need(p + 4 + raw_len)?;
    let raw = &buf[p + 4..p + 4 + raw_len];
    Ok((EventView { time_ms, metas, raw }, p + 4 + raw_len))
}

pub struct JournalWriter {
    file: File,
    bidx: File,
    dir: std::path::PathBuf,
    block_buf: Vec<u8>,
    block_events: u32,
    block_min_time: u64,
    block_first_ord: u32,
    next_ord: u32,
    file_len: u64,
    pub blocks: Vec<BlockEntry>,
}

impl JournalWriter {
    pub fn create(dir: &Path) -> io::Result<JournalWriter> {
        let mut file = File::create(dir.join("journal.zj"))?;
        let mut hdr = [0u8; 16];
        hdr[0..4].copy_from_slice(MAGIC_HDR);
        hdr[4..6].copy_from_slice(&1u16.to_le_bytes());
        file.write_all(&hdr)?;
        let bidx = File::create(dir.join("journal.zj.bidx"))?;
        Ok(JournalWriter {
            file,
            bidx,
            dir: dir.to_path_buf(),
            block_buf: Vec::with_capacity(BLOCK_BYTES),
            block_events: 0,
            block_min_time: u64::MAX,
            block_first_ord: 0,
            next_ord: 0,
            file_len: 16,
            blocks: Vec::new(),
        })
    }

    /// Append an event to the open block; returns its address. Does NOT flush —
    /// the caller checks `block_full()` so it can persist its own per-event state
    /// (events.tbl) in lockstep with block flushes.
    pub fn append(&mut self, time_ms: u64, metas: &[(u8, &[u8])], raw: &[u8]) -> u64 {
        let addr = ((self.blocks.len() as u64) << 32) | self.block_buf.len() as u64;
        encode_event(&mut self.block_buf, time_ms, metas, raw);
        self.block_events += 1;
        self.next_ord += 1;
        self.block_min_time = self.block_min_time.min(time_ms);
        addr
    }

    /// Bytes currently in the open (unflushed) block buffer.
    pub fn block_len(&self) -> usize {
        self.block_buf.len()
    }

    pub fn block_full(&self) -> bool {
        self.block_buf.len() >= BLOCK_BYTES || self.block_events >= BLOCK_EVENTS
    }

    pub fn block_dirty(&self) -> bool {
        !self.block_buf.is_empty()
    }

    /// Compress and write the open block; append its entry to the bidx sidecar.
    pub fn flush_block(&mut self) -> io::Result<()> {
        if self.block_buf.is_empty() {
            return Ok(());
        }
        let comp = crate::codec::compress(&self.block_buf)?;
        self.file.write_all(&comp)?;
        let entry = BlockEntry {
            file_offset: self.file_len,
            comp_len: comp.len() as u32,
            uncomp_len: self.block_buf.len() as u32,
            min_time_ms: self.block_min_time,
            first_event_ord: self.block_first_ord,
        };
        self.file_len += comp.len() as u64;
        self.bidx.write_all(&entry.encode())?;
        self.blocks.push(entry);
        self.block_buf.clear();
        self.block_events = 0;
        self.block_min_time = u64::MAX;
        self.block_first_ord = self.next_ord;
        Ok(())
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()?;
        self.bidx.sync_data()
    }

    /// Flush, write the block index table + trailer, drop the sidecar.
    pub fn finalize(mut self) -> io::Result<()> {
        self.flush_block()?;
        write_trailer(&mut self.file, &self.blocks, self.file_len)?;
        drop(self.bidx);
        std::fs::remove_file(self.dir.join("journal.zj.bidx"))
    }
}

fn write_trailer(file: &mut File, blocks: &[BlockEntry], table_offset: u64) -> io::Result<()> {
    for e in blocks {
        file.write_all(&e.encode())?;
    }
    let mut tr = [0u8; 16];
    tr[0..4].copy_from_slice(&(blocks.len() as u32).to_le_bytes());
    tr[4..12].copy_from_slice(&table_offset.to_le_bytes());
    tr[12..16].copy_from_slice(MAGIC_END);
    file.write_all(&tr)?;
    file.sync_all()
}

/// Crash recovery for a hot journal: rebuild the trailer from the bidx
/// sidecar, discarding any partially-written final block. No-op if the
/// journal is already finalized. Returns the recovered block entries.
pub fn recover(dir: &Path) -> io::Result<Vec<BlockEntry>> {
    let side_path = dir.join("journal.zj.bidx");
    if !side_path.exists() {
        return Ok(JournalReader::open(dir)?.blocks); // already finalized
    }
    let mut side = Vec::new();
    File::open(&side_path)?.read_to_end(&mut side)?;
    let blocks: Vec<BlockEntry> = side.as_chunks::<ENTRY_LEN>().0.iter().map(|c| BlockEntry::decode(c)).collect();
    let end = blocks.last().map(|e| e.file_offset + e.comp_len as u64).unwrap_or(16);
    let mut file = OpenOptions::new().read(true).write(true).open(dir.join("journal.zj"))?;
    file.set_len(end)?; // drop a torn final block, if any
    file.seek(SeekFrom::End(0))?;
    write_trailer(&mut file, &blocks, end)?;
    std::fs::remove_file(&side_path)?;
    Ok(blocks)
}

pub struct JournalReader {
    file: File,
    scratch: Vec<u8>,
    pub blocks: Vec<BlockEntry>,
}

impl JournalReader {
    /// Open via trailer (finalized) or bidx sidecar (hot).
    pub fn open(dir: &Path) -> io::Result<JournalReader> {
        let mut file = File::open(dir.join("journal.zj"))?;
        let len = file.metadata()?.len();
        let mut hdr = [0u8; 16];
        file.read_exact(&mut hdr)?;
        if &hdr[0..4] != MAGIC_HDR {
            return Err(bad("bad journal header magic"));
        }
        let mut blocks = Vec::new();
        let mut tr = [0u8; 16];
        let finalized = len >= 32 && {
            file.seek(SeekFrom::End(-16))?;
            file.read_exact(&mut tr)?;
            &tr[12..16] == MAGIC_END
        };
        if finalized {
            let count = u32::from_le_bytes(tr[0..4].try_into().unwrap()) as usize;
            let table_offset = u64::from_le_bytes(tr[4..12].try_into().unwrap());
            let mut tbl = vec![0u8; count * ENTRY_LEN];
            file.seek(SeekFrom::Start(table_offset))?;
            file.read_exact(&mut tbl)?;
            for c in tbl.as_chunks::<ENTRY_LEN>().0 {
                blocks.push(BlockEntry::decode(c));
            }
        } else {
            let mut side = Vec::new();
            match File::open(dir.join("journal.zj.bidx")) {
                Ok(mut f) => {
                    f.read_to_end(&mut side)?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {} // empty hot journal
                Err(e) => return Err(e),
            }
            for c in side.as_chunks::<ENTRY_LEN>().0 {
                blocks.push(BlockEntry::decode(c));
            }
        }
        Ok(JournalReader { file, scratch: Vec::new(), blocks })
    }

    /// Decompress block `i` into `out` (reused by callers to bound memory).
    pub fn read_block(&mut self, i: usize, out: &mut Vec<u8>) -> io::Result<()> {
        let e = self.blocks[i];
        self.scratch.resize(e.comp_len as usize, 0);
        self.file.seek(SeekFrom::Start(e.file_offset))?;
        self.file.read_exact(&mut self.scratch)?;
        crate::codec::decompress_into(&self.scratch, out, e.uncomp_len as usize)?;
        if out.len() != e.uncomp_len as usize {
            return Err(bad("block uncompressed length mismatch"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join(format!("fossick-jnl-{}", std::process::id()))
            .join(format!("{:?}", std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn event_roundtrip() {
        let mut buf = Vec::new();
        encode_event(&mut buf, 42, &[(0, b"src"), (2, b"host1")], b"hello world");
        encode_event(&mut buf, 43, &[], b"");
        let (v, next) = decode_event(&buf, 0).unwrap();
        assert_eq!(v.time_ms, 42);
        assert_eq!(v.metas, vec![(0u8, &b"src"[..]), (2u8, &b"host1"[..])]);
        assert_eq!(v.raw, b"hello world");
        let (v2, end) = decode_event(&buf, next).unwrap();
        assert_eq!((v2.time_ms, v2.raw.len()), (43, 0));
        assert_eq!(end, buf.len());
    }

    #[test]
    fn decode_rejects_truncation() {
        let mut buf = Vec::new();
        encode_event(&mut buf, 1, &[(0, b"x")], b"abc");
        for cut in 0..buf.len() {
            assert!(decode_event(&buf[..cut], 0).is_err(), "cut={cut}");
        }
    }

    fn write_events(dir: &Path, n: u64, finalize: bool) {
        let mut w = JournalWriter::create(dir).unwrap();
        for i in 0..n {
            let raw = format!("event number {i} with some padding padding padding");
            w.append(1000 + i, &[(1, b"st")], raw.as_bytes());
            if w.block_full() {
                w.flush_block().unwrap();
            }
        }
        if finalize {
            w.finalize().unwrap();
        } else {
            w.flush_block().unwrap();
        }
    }

    fn read_all(dir: &Path) -> Vec<(u64, String)> {
        let mut r = JournalReader::open(dir).unwrap();
        let mut out = Vec::new();
        let mut block = Vec::new();
        for i in 0..r.blocks.len() {
            r.read_block(i, &mut block).unwrap();
            let mut off = 0;
            while off < block.len() {
                let (v, next) = decode_event(&block, off).unwrap();
                out.push((v.time_ms, String::from_utf8_lossy(v.raw).into_owned()));
                off = next;
            }
        }
        out
    }

    #[test]
    fn journal_roundtrip_finalized() {
        let d = tmp();
        write_events(&d, 5000, true); // > BLOCK_EVENTS → multiple blocks
        assert!(!d.join("journal.zj.bidx").exists());
        let all = read_all(&d);
        assert_eq!(all.len(), 5000);
        assert_eq!(all[0].0, 1000);
        assert_eq!(all[4999], (5999, "event number 4999 with some padding padding padding".into()));
    }

    #[test]
    fn journal_roundtrip_hot_via_sidecar() {
        let d = tmp();
        write_events(&d, 3000, false); // no trailer → reader must use sidecar
        assert!(d.join("journal.zj.bidx").exists());
        let all = read_all(&d);
        assert_eq!(all.len(), 3000);
    }

    #[test]
    fn block_entries_track_min_time_and_ord() {
        let d = tmp();
        write_events(&d, 5000, true);
        let r = JournalReader::open(&d).unwrap();
        assert!(r.blocks.len() >= 2);
        assert_eq!(r.blocks[0].first_event_ord, 0);
        assert_eq!(r.blocks[0].min_time_ms, 1000);
        assert_eq!(r.blocks[1].first_event_ord, BLOCK_EVENTS.min(5000));
    }
}
