//! index.tsidx: term dictionary → postings of time-sorted event ordinals.
//!
//! Layout: 12B header ("FTX1", u16 ver, u16 pad, u32 term_count)
//!       | postings region (per-term delta-varint ascending ordinals)
//!       | lexicon (sorted terms: u16 len, term, u64 postings_off, u32 postings_len, u32 freq)
//!       | offsets array (u64 × term_count → lexicon entry file offsets)
//!       | 20B trailer (u64 lexicon_off, u64 offsets_off, "FTXE").
//! Read via mmap; lookups binary-search the offsets array, postings decode
//! lazily so intersection never materializes input lists.

use std::io::{self, BufWriter, Write};
use std::path::Path;

const MAGIC_HDR: &[u8; 4] = b"FTX1";
const MAGIC_END: &[u8; 4] = b"FTXE";

fn put_varint(buf: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(b);
            break;
        }
        buf.push(b | 0x80);
    }
}

fn get_varint(data: &[u8], pos: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    let mut shift = 0;
    loop {
        let b = *data.get(*pos)?;
        *pos += 1;
        v |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

/// Write index.tsidx. `terms` maps term → ascending, deduped event ordinals;
/// entries are sorted in place.
pub fn write(dir: &Path, terms: &mut [(Box<[u8]>, Vec<u32>)]) -> io::Result<()> {
    terms.sort_by(|a, b| a.0.cmp(&b.0));
    let mut f = BufWriter::new(std::fs::File::create(dir.join("index.tsidx"))?);
    let mut pos: u64 = 12;
    let mut hdr = [0u8; 12];
    hdr[0..4].copy_from_slice(MAGIC_HDR);
    hdr[4..6].copy_from_slice(&1u16.to_le_bytes());
    hdr[8..12].copy_from_slice(&(terms.len() as u32).to_le_bytes());
    f.write_all(&hdr)?;

    let mut meta = Vec::with_capacity(terms.len()); // (postings_off, postings_len, freq)
    let mut buf = Vec::new();
    for (_, ords) in terms.iter() {
        buf.clear();
        let mut prev = 0u32;
        for (i, &o) in ords.iter().enumerate() {
            put_varint(&mut buf, if i == 0 { o } else { o - prev });
            prev = o;
        }
        f.write_all(&buf)?;
        meta.push((pos, buf.len() as u32, ords.len() as u32));
        pos += buf.len() as u64;
    }

    let lexicon_off = pos;
    let mut entry_offs = Vec::with_capacity(terms.len());
    for ((term, _), (poff, plen, freq)) in terms.iter().zip(&meta) {
        entry_offs.push(pos);
        f.write_all(&(term.len() as u16).to_le_bytes())?;
        f.write_all(term)?;
        f.write_all(&poff.to_le_bytes())?;
        f.write_all(&plen.to_le_bytes())?;
        f.write_all(&freq.to_le_bytes())?;
        pos += 2 + term.len() as u64 + 16;
    }
    let offsets_off = pos;
    for o in &entry_offs {
        f.write_all(&o.to_le_bytes())?;
    }
    f.write_all(&lexicon_off.to_le_bytes())?;
    f.write_all(&offsets_off.to_le_bytes())?;
    f.write_all(MAGIC_END)?;
    f.into_inner()?.sync_data()
}

pub struct Tsidx {
    map: memmap2::Mmap,
    term_count: usize,
    offsets_off: usize,
}

pub struct Entry<'a> {
    pub term: &'a [u8],
    pub postings_off: usize,
    pub postings_len: usize,
    pub freq: u32,
}

/// Lazily-decoded ascending ordinal list.
#[derive(Clone)]
pub struct Postings<'a> {
    data: &'a [u8],
    pos: usize,
    prev: u32,
    started: bool,
}

impl<'a> Iterator for Postings<'a> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let d = get_varint(self.data, &mut self.pos)?;
        self.prev = if self.started { self.prev + d } else { d };
        self.started = true;
        Some(self.prev)
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl Tsidx {
    pub fn open(dir: &Path) -> io::Result<Tsidx> {
        let file = std::fs::File::open(dir.join("index.tsidx"))?;
        let map = unsafe { memmap2::Mmap::map(&file)? };
        if map.len() < 32 || &map[0..4] != MAGIC_HDR || &map[map.len() - 4..] != MAGIC_END {
            return Err(bad("bad tsidx magic"));
        }
        let term_count = u32::from_le_bytes(map[8..12].try_into().unwrap()) as usize;
        let t = map.len() - 20;
        let offsets_off = u64::from_le_bytes(map[t + 8..t + 16].try_into().unwrap()) as usize;
        Ok(Tsidx { map, term_count, offsets_off })
    }

    pub fn term_count(&self) -> usize {
        self.term_count
    }

    pub fn entry(&self, rank: usize) -> Entry<'_> {
        let o = self.offsets_off + rank * 8;
        let e = u64::from_le_bytes(self.map[o..o + 8].try_into().unwrap()) as usize;
        let tlen = u16::from_le_bytes(self.map[e..e + 2].try_into().unwrap()) as usize;
        let term = &self.map[e + 2..e + 2 + tlen];
        let m = e + 2 + tlen;
        Entry {
            term,
            postings_off: u64::from_le_bytes(self.map[m..m + 8].try_into().unwrap()) as usize,
            postings_len: u32::from_le_bytes(self.map[m + 8..m + 12].try_into().unwrap()) as usize,
            freq: u32::from_le_bytes(self.map[m + 12..m + 16].try_into().unwrap()),
        }
    }

    fn postings_of(&self, e: &Entry<'_>) -> Postings<'_> {
        Postings { data: &self.map[e.postings_off..e.postings_off + e.postings_len], pos: 0, prev: 0, started: false }
    }

    /// Rank of the first term ≥ `term`.
    fn lower_bound(&self, term: &[u8]) -> usize {
        let (mut lo, mut hi) = (0, self.term_count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.entry(mid).term < term {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    pub fn lookup(&self, term: &[u8]) -> Option<Postings<'_>> {
        let r = self.lower_bound(term);
        if r < self.term_count {
            let e = self.entry(r);
            if e.term == term {
                return Some(self.postings_of(&e));
            }
        }
        None
    }

    /// All (term, postings) with the given prefix — the wildcard `foo*` path.
    pub fn prefix<'s>(&'s self, prefix: &'s [u8]) -> impl Iterator<Item = (&'s [u8], Postings<'s>)> + 's {
        let start = self.lower_bound(prefix);
        (start..self.term_count)
            .map(|r| self.entry(r))
            .take_while(move |e| e.term.starts_with(prefix))
            .map(move |e| (e.term, self.postings_of(&e)))
    }
}

/// Streaming k-way intersection of ascending postings (empty input → empty).
pub fn intersect(mut its: Vec<Postings<'_>>) -> Vec<u32> {
    let mut out = Vec::new();
    if its.is_empty() {
        return out;
    }
    let mut cur: Vec<u32> = Vec::with_capacity(its.len());
    for it in its.iter_mut() {
        match it.next() {
            Some(v) => cur.push(v),
            None => return out,
        }
    }
    'outer: loop {
        let mx = *cur.iter().max().unwrap();
        let mut all_eq = true;
        for (i, it) in its.iter_mut().enumerate() {
            while cur[i] < mx {
                match it.next() {
                    Some(v) => cur[i] = v,
                    None => break 'outer,
                }
            }
            all_eq &= cur[i] == mx;
        }
        if all_eq {
            out.push(mx);
            for (i, it) in its.iter_mut().enumerate() {
                match it.next() {
                    Some(v) => cur[i] = v,
                    None => break 'outer,
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny deterministic RNG (no proptest dep, per project policy).
    pub struct XorShift(u64);
    impl XorShift {
        pub fn new(seed: u64) -> XorShift {
            XorShift(seed.max(1))
        }
        pub fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join(format!("fossick-tsx-{}", std::process::id()))
            .join(format!("{:?}", std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn varint_roundtrip() {
        let mut buf = Vec::new();
        let vals = [0u32, 1, 127, 128, 300, 16_383, 16_384, u32::MAX];
        for v in vals {
            put_varint(&mut buf, v);
        }
        let mut pos = 0;
        for v in vals {
            assert_eq!(get_varint(&buf, &mut pos), Some(v));
        }
        assert_eq!(pos, buf.len());
    }

    #[test]
    fn write_lookup_roundtrip_property() {
        let d = tmp();
        let mut rng = XorShift::new(42);
        // Random term set with random ascending postings.
        let mut terms: Vec<(Box<[u8]>, Vec<u32>)> = (0..500)
            .map(|i| {
                let name = format!("term_{:04}_{}", rng.next() % 10_000, i);
                let mut ords: Vec<u32> = (0..(rng.next() % 200 + 1)).map(|_| (rng.next() % 100_000) as u32).collect();
                ords.sort_unstable();
                ords.dedup();
                (name.into_bytes().into_boxed_slice(), ords)
            })
            .collect();
        // Dedup names (rng collisions would break lookup equality).
        terms.sort_by(|a, b| a.0.cmp(&b.0));
        terms.dedup_by(|a, b| a.0 == b.0);
        let expect = terms.clone();
        write(&d, &mut terms).unwrap();

        let idx = Tsidx::open(&d).unwrap();
        assert_eq!(idx.term_count(), expect.len());
        for (term, ords) in &expect {
            let got: Vec<u32> = idx.lookup(term).unwrap().collect();
            assert_eq!(&got, ords, "term {}", String::from_utf8_lossy(term));
        }
        assert!(idx.lookup(b"zzz_not_there").is_none());
    }

    #[test]
    fn prefix_scan() {
        let d = tmp();
        let mut terms: Vec<(Box<[u8]>, Vec<u32>)> =
            [("apple", vec![1]), ("applet", vec![2]), ("apply", vec![3]), ("banana", vec![4])]
                .into_iter()
                .map(|(t, o)| (t.as_bytes().to_vec().into_boxed_slice(), o))
                .collect();
        write(&d, &mut terms).unwrap();
        let idx = Tsidx::open(&d).unwrap();
        let hits: Vec<String> = idx.prefix(b"appl").map(|(t, _)| String::from_utf8_lossy(t).into_owned()).collect();
        assert_eq!(hits, ["apple", "applet", "apply"]);
        assert_eq!(idx.prefix(b"zzz").count(), 0);
    }

    #[test]
    fn intersection() {
        let d = tmp();
        let mut terms: Vec<(Box<[u8]>, Vec<u32>)> = [
            ("a", (0..1000).collect::<Vec<u32>>()),
            ("b", (0..1000).filter(|v| v % 3 == 0).collect()),
            ("c", (0..1000).filter(|v| v % 5 == 0).collect()),
        ]
        .into_iter()
        .map(|(t, o)| (t.as_bytes().to_vec().into_boxed_slice(), o))
        .collect();
        write(&d, &mut terms).unwrap();
        let idx = Tsidx::open(&d).unwrap();
        let got = intersect(vec![idx.lookup(b"a").unwrap(), idx.lookup(b"b").unwrap(), idx.lookup(b"c").unwrap()]);
        let want: Vec<u32> = (0..1000).filter(|v| v % 15 == 0).collect();
        assert_eq!(got, want);
        assert!(intersect(vec![]).is_empty());
    }
}
