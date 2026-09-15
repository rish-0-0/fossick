//! Per-bucket `manifest`: plain key=value text (same syntax as fossick.conf).

use std::io::{self, Write};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub id: u64,
    pub state: State,
    pub event_count: u64,
    pub min_time_ms: u64,
    pub max_time_ms: u64,
    /// Index-time field names; a field's position is its key_id in journal records.
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Hot,
    Warm,
    Cold,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Hot => "hot",
            State::Warm => "warm",
            State::Cold => "cold",
        }
    }
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

impl Manifest {
    pub fn new(id: u64, fields: Vec<String>) -> Manifest {
        Manifest { version: 1, id, state: State::Hot, event_count: 0, min_time_ms: u64::MAX, max_time_ms: 0, fields }
    }

    pub fn field_id(&self, name: &str) -> Option<u8> {
        self.fields.iter().position(|f| f == name).map(|i| i as u8)
    }

    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let mut s = String::new();
        s.push_str(&format!("version = {}\n", self.version));
        s.push_str(&format!("id = {}\n", self.id));
        s.push_str(&format!("state = {}\n", self.state.as_str()));
        s.push_str(&format!("event_count = {}\n", self.event_count));
        s.push_str(&format!("min_time_ms = {}\n", self.min_time_ms));
        s.push_str(&format!("max_time_ms = {}\n", self.max_time_ms));
        for (i, f) in self.fields.iter().enumerate() {
            s.push_str(&format!("field.{i} = {f}\n"));
        }
        // Write-to-temp + rename so a concurrent searcher never sees a torn manifest.
        let tmp = dir.join("manifest.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(s.as_bytes())?;
        f.sync_data()?;
        drop(f);
        let dst = dir.join("manifest");
        if dst.exists() {
            std::fs::remove_file(&dst)?;
        }
        std::fs::rename(&tmp, &dst)
    }

    pub fn read(dir: &Path) -> io::Result<Manifest> {
        let text = std::fs::read_to_string(dir.join("manifest"))?;
        let mut m = Manifest::new(0, Vec::new());
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| bad(format!("manifest: bad line: {line}")))?;
            let (k, v) = (k.trim(), v.trim());
            let num = || v.parse::<u64>().map_err(|_| bad(format!("manifest: bad number for {k}: {v}")));
            match k {
                "version" => m.version = num()? as u32,
                "id" => m.id = num()?,
                "event_count" => m.event_count = num()?,
                "min_time_ms" => m.min_time_ms = num()?,
                "max_time_ms" => m.max_time_ms = num()?,
                "state" => {
                    m.state = match v {
                        "hot" => State::Hot,
                        "warm" => State::Warm,
                        "cold" => State::Cold,
                        _ => return Err(bad(format!("manifest: bad state: {v}"))),
                    }
                }
                _ if k.starts_with("field.") => {
                    let i: usize = k[6..].parse().map_err(|_| bad(format!("manifest: bad field key: {k}")))?;
                    if m.fields.len() != i {
                        return Err(bad(format!("manifest: field.{i} out of order")));
                    }
                    m.fields.push(v.to_string());
                }
                _ => return Err(bad(format!("manifest: unknown key: {k}"))),
            }
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip() {
        let d = std::env::temp_dir().join(format!("fossick-man-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut m = Manifest::new(7, vec!["source".into(), "sourcetype".into(), "host".into(), "level".into()]);
        m.event_count = 123;
        m.min_time_ms = 5;
        m.max_time_ms = 9;
        m.state = State::Warm;
        m.write(&d).unwrap();
        assert_eq!(Manifest::read(&d).unwrap(), m);
        assert_eq!(m.field_id("host"), Some(2));
        assert_eq!(m.field_id("nope"), None);
    }
}
