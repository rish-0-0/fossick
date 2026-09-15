//! Block compression seam. The rest of the crate only calls these two
//! functions — swapping zstd for another codec is a change to this file only.

use std::io;

const ZSTD_LEVEL: i32 = 3;

pub fn compress(src: &[u8]) -> io::Result<Vec<u8>> {
    zstd::bulk::compress(src, ZSTD_LEVEL)
}

/// Decompress into `out` (cleared first; reused by callers to bound memory).
pub fn decompress_into(src: &[u8], out: &mut Vec<u8>, uncomp_len: usize) -> io::Result<()> {
    out.resize(uncomp_len, 0);
    let n = zstd::bulk::decompress_to_buffer(src, &mut out[..])?;
    out.truncate(n);
    Ok(())
}
