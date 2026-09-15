//! bloom.bf: per-bucket bloom filter over all indexed terms. Sized at
//! ~10 bits/term with k=7 (~1% FP). Double hashing over two FNV-1a seeds.
//! Layout: "FBL1" | u64 m_bits | u32 k | bit array.

use std::io::{self, Write};
use std::path::Path;

const MAGIC: &[u8; 4] = b"FBL1";
const BITS_PER_TERM: u64 = 10;
const K: u32 = 7;
const FNV_PRIME: u64 = 0x100_0000_01b3;
const BASIS_1: u64 = 0xcbf2_9ce4_8422_2325;
const BASIS_2: u64 = 0x9747_b28c_8421_9bcf;

fn fnv(term: &[u8], basis: u64) -> u64 {
    term.iter().fold(basis, |h, &b| (h ^ b as u64).wrapping_mul(FNV_PRIME))
}

fn set_positions(term: &[u8], m: u64, mut f: impl FnMut(u64)) {
    let h1 = fnv(term, BASIS_1);
    let h2 = fnv(term, BASIS_2) | 1;
    for i in 0..K as u64 {
        f(h1.wrapping_add(i.wrapping_mul(h2)) % m);
    }
}

/// Build and write bloom.bf from the final term set.
pub fn write<'a>(dir: &Path, terms: impl Iterator<Item = &'a [u8]>, term_count: usize) -> io::Result<()> {
    let m = (term_count.max(1) as u64) * BITS_PER_TERM;
    let mut bits = vec![0u8; m.div_ceil(8) as usize];
    for t in terms {
        set_positions(t, m, |p| bits[(p / 8) as usize] |= 1 << (p % 8));
    }
    let mut f = std::fs::File::create(dir.join("bloom.bf"))?;
    f.write_all(MAGIC)?;
    f.write_all(&m.to_le_bytes())?;
    f.write_all(&K.to_le_bytes())?;
    f.write_all(&bits)?;
    f.sync_data()
}

pub struct Bloom {
    map: memmap2::Mmap,
    m: u64,
}

impl Bloom {
    /// None if the bucket has no bloom (hot buckets).
    pub fn open(dir: &Path) -> io::Result<Option<Bloom>> {
        let file = match std::fs::File::open(dir.join("bloom.bf")) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let map = unsafe { memmap2::Mmap::map(&file)? };
        if map.len() < 16 || &map[0..4] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad bloom magic"));
        }
        let m = u64::from_le_bytes(map[4..12].try_into().unwrap());
        if map.len() < 16 + m.div_ceil(8) as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bloom truncated"));
        }
        Ok(Some(Bloom { map, m }))
    }

    pub fn may_contain(&self, term: &[u8]) -> bool {
        let bits = &self.map[16..];
        let mut hit = true;
        set_positions(term, self.m, |p| hit &= bits[(p / 8) as usize] & (1 << (p % 8)) != 0);
        hit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join(format!("fossick-blm-{}", std::process::id()))
            .join(format!("{:?}", std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn no_false_negatives_and_low_fp_rate() {
        let d = tmp();
        let present: Vec<Vec<u8>> = (0..10_000).map(|i| format!("term_{i}").into_bytes()).collect();
        write(&d, present.iter().map(Vec::as_slice), present.len()).unwrap();
        let b = Bloom::open(&d).unwrap().unwrap();
        for t in &present {
            assert!(b.may_contain(t), "false negative: {}", String::from_utf8_lossy(t));
        }
        let fps = (0..10_000).filter(|i| b.may_contain(format!("absent_{i}").as_bytes())).count();
        let rate = fps as f64 / 10_000.0;
        assert!(rate < 0.03, "FP rate {rate} too high"); // theoretical ~1% at 10 bits/term, k=7
    }

    #[test]
    fn missing_file_is_none() {
        let d = tmp();
        assert!(Bloom::open(&d).unwrap().is_none());
    }
}
