//! fossick-index: bucket storage — raw journal, event table, manifest.

pub mod bloom;
pub mod bucket;
pub mod codec;
pub mod journal;
pub mod manifest;
pub mod tokenize;
pub mod tsidx;

use std::path::{Path, PathBuf};

/// A bucket directory found under `<data_dir>/main/`.
#[derive(Debug, Clone)]
pub struct BucketRef {
    pub dir: PathBuf,
    pub id: u64,
    pub hot: bool,
    pub min_time_ms: u64,
    pub max_time_ms: u64,
}

/// List bucket dirs, newest first (hot bucket sorts first).
pub fn list_buckets(data_dir: &Path) -> std::io::Result<Vec<BucketRef>> {
    let main = data_dir.join("main");
    let mut out = Vec::new();
    if !main.exists() {
        return Ok(out);
    }
    for ent in std::fs::read_dir(&main)? {
        let ent = ent?;
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if let Some(id) = name.strip_prefix("hot_").and_then(|s| s.parse().ok()) {
            out.push(BucketRef { dir: ent.path(), id, hot: true, min_time_ms: 0, max_time_ms: u64::MAX });
        } else if let Some(rest) = name.strip_prefix("db_") {
            let p: Vec<&str> = rest.splitn(3, '_').collect();
            if p.len() == 3 {
                if let (Ok(max), Ok(min), Ok(id)) = (p[0].parse(), p[1].parse(), p[2].parse()) {
                    out.push(BucketRef { dir: ent.path(), id, hot: false, min_time_ms: min, max_time_ms: max });
                }
            }
        }
    }
    out.sort_by(|a, b| (b.hot, b.max_time_ms, b.id).cmp(&(a.hot, a.max_time_ms, a.id)));
    Ok(out)
}
