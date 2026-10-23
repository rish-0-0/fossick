//! Hand-rolled fossick.conf parser (ini-style). Unknown keys are startup
//! errors so typos surface immediately.

use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Conf {
    pub data_dir: PathBuf,
    /// Search REST API port on 127.0.0.1; 0 disables.
    pub api_port: u16,
    pub hot_max_mb: u64,
    pub hot_max_events: u64,
    pub hot_max_hours: u64,
    pub warm_max_gb: u64,
    pub inputs: Vec<TcpInput>,
    /// Field names extracted as key=value from _raw at index time.
    pub indexed_fields: Vec<String>,
    pub sort_max: usize,
    pub stats_max_groups: usize,
    pub dedup_max: usize,
    /// Cap on events returned by a non-aggregated search.
    pub max_results: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TcpInput {
    pub port: u16,
    pub source: String,
    pub sourcetype: String,
    /// `%REMOTE_IP%` expands to the peer address at connect time.
    pub host: String,
}

impl TcpInput {
    fn new(port: u16) -> TcpInput {
        TcpInput { port, source: format!("tcp:{port}"), sourcetype: "default".into(), host: "%REMOTE_IP%".into() }
    }
}

impl Default for Conf {
    fn default() -> Conf {
        Conf {
            data_dir: PathBuf::from("fossick-data"),
            api_port: 8089,
            hot_max_mb: 270,
            hot_max_events: 4_000_000,
            hot_max_hours: 24,
            warm_max_gb: 25,
            inputs: vec![TcpInput::new(5514)],
            indexed_fields: Vec::new(),
            sort_max: 10_000,
            stats_max_groups: 100_000,
            dedup_max: 100_000,
            max_results: 10_000,
        }
    }
}

fn bad(line_no: usize, msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("fossick.conf:{line_no}: {msg}"))
}

impl Conf {
    pub fn parse(text: &str) -> io::Result<Conf> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text); // tolerate UTF-8 BOM
        let mut c = Conf::default();
        c.inputs.clear(); // config-defined inputs replace the default
        let mut section = String::from("general");
        for (i, raw_line) in text.lines().enumerate() {
            let n = i + 1;
            let line = match raw_line.find('#') {
                Some(p) => &raw_line[..p],
                None => raw_line,
            }
            .trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                let name = rest.strip_suffix(']').ok_or_else(|| bad(n, "unterminated section".into()))?;
                section = name.trim().to_string();
                if let Some(port) = section.strip_prefix("input:tcp:") {
                    let port: u16 = port.parse().map_err(|_| bad(n, format!("bad port: {port}")))?;
                    c.inputs.push(TcpInput::new(port));
                } else if !matches!(section.as_str(), "general" | "bucket" | "indexed_fields" | "limits") {
                    return Err(bad(n, format!("unknown section: [{section}]")));
                }
                continue;
            }
            let (k, v) = line.split_once('=').ok_or_else(|| bad(n, format!("expected key = value: {line}")))?;
            let (k, v) = (k.trim(), v.trim());
            let num = |v: &str| v.parse::<u64>().map_err(|_| bad(n, format!("bad number for {k}: {v}")));
            match (section.as_str(), k) {
                ("general", "data_dir") => c.data_dir = PathBuf::from(v),
                ("general", "api_port") => c.api_port = num(v)? as u16,
                ("bucket", "hot_max_mb") => c.hot_max_mb = num(v)?,
                ("bucket", "hot_max_events") => c.hot_max_events = num(v)?,
                ("bucket", "hot_max_hours") => c.hot_max_hours = num(v)?,
                ("bucket", "warm_max_gb") => c.warm_max_gb = num(v)?,
                ("limits", "sort_max") => c.sort_max = num(v)? as usize,
                ("limits", "stats_max_groups") => c.stats_max_groups = num(v)? as usize,
                ("limits", "dedup_max") => c.dedup_max = num(v)? as usize,
                ("limits", "max_results") => c.max_results = num(v)? as usize,
                ("indexed_fields", name) => {
                    if v != "kv" {
                        return Err(bad(n, format!("indexed field {name}: only 'kv' extraction is supported, got {v}")));
                    }
                    c.indexed_fields.push(name.to_string());
                }
                (s, _) if s.starts_with("input:tcp:") => {
                    let inp = c.inputs.last_mut().unwrap();
                    match k {
                        "source" => inp.source = v.to_string(),
                        "sourcetype" => inp.sourcetype = v.to_string(),
                        "host" => inp.host = v.to_string(),
                        _ => return Err(bad(n, format!("unknown key in [{s}]: {k}"))),
                    }
                }
                (s, _) => return Err(bad(n, format!("unknown key in [{s}]: {k}"))),
            }
        }
        if c.inputs.is_empty() {
            c.inputs.push(TcpInput::new(5514));
        }
        Ok(c)
    }

    pub fn load(path: &std::path::Path) -> io::Result<Conf> {
        Conf::parse(&std::fs::read_to_string(path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_conf() {
        let c = Conf::parse(
            "# comment\n[general]\ndata_dir = C:\\tmp\\fx\n[bucket]\nhot_max_mb = 1\n\
             [input:tcp:6000]\nsourcetype = app\nhost = web01\n[indexed_fields]\nlevel = kv\n\
             [limits]\nsort_max = 5\n",
        )
        .unwrap();
        assert_eq!(c.data_dir, PathBuf::from("C:\\tmp\\fx"));
        assert_eq!(c.hot_max_mb, 1);
        assert_eq!(c.inputs.len(), 1);
        assert_eq!(c.inputs[0].port, 6000);
        assert_eq!(c.inputs[0].sourcetype, "app");
        assert_eq!(c.inputs[0].host, "web01");
        assert_eq!(c.inputs[0].source, "tcp:6000"); // default kept
        assert_eq!(c.indexed_fields, vec!["level"]);
        assert_eq!(c.sort_max, 5);
    }

    #[test]
    fn unknown_key_is_error() {
        for text in ["[general]\ndata_dirr = x\n", "[bucket]\nhot = 1\n", "[nope]\n", "[input:tcp:1]\nfoo = 1\n"] {
            assert!(Conf::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn tolerates_utf8_bom() {
        let c = Conf::parse("\u{feff}[bucket]\nhot_max_mb = 7\n").unwrap();
        assert_eq!(c.hot_max_mb, 7);
    }

    #[test]
    fn empty_conf_gets_defaults() {
        let c = Conf::parse("").unwrap();
        assert_eq!(c.hot_max_mb, 270);
        assert_eq!(c.inputs[0].port, 5514);
    }
}
