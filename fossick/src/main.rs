use fossick::{conf::Conf, loggen, serve, spl, tstamp};
use std::process::exit;

const USAGE: &str = "usage:
  fossick serve  [--conf <path>]
  fossick search \"<spl>\" [--conf <path>]
  fossick loggen --port <p> --count <n>

spl: terms, field=value, earliest=/latest=, then pipes:
  where eval stats sort head tail reverse dedup table fields rename rex regex top rare fillnull
conf resolution: --conf, else $FOSSICK_CONF, else ./fossick.conf, else built-in defaults";

fn load_conf(explicit: Option<String>) -> Conf {
    let path = explicit
        .or_else(|| std::env::var("FOSSICK_CONF").ok())
        .map(std::path::PathBuf::from);
    match path {
        Some(p) => Conf::load(&p).unwrap_or_else(|e| {
            eprintln!("fossick: cannot load {}: {e}", p.display());
            exit(2);
        }),
        None => {
            let default = std::path::Path::new("fossick.conf");
            if default.exists() {
                Conf::load(default).unwrap_or_else(|e| {
                    eprintln!("fossick: {e}");
                    exit(2);
                })
            } else {
                Conf::default()
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let mut positional = Vec::new();
    let mut conf_path = None;
    let (mut port, mut count) = (0u16, 0u64);
    while let Some(a) = args.next() {
        let mut flag = |name: &str| -> Option<String> {
            if a == name {
                args.next().or_else(|| {
                    eprintln!("fossick: {name} needs a value");
                    exit(2);
                })
            } else {
                None
            }
        };
        if let Some(v) = flag("--conf") {
            conf_path = Some(v);
        } else if let Some(v) = flag("--port") {
            port = v.parse().unwrap_or(0);
        } else if let Some(v) = flag("--count") {
            count = v.parse().unwrap_or(0);
        } else {
            positional.push(a);
        }
    }
    let conf = load_conf(conf_path);
    let result = match cmd.as_str() {
        "serve" => serve::run(conf),
        "search" => {
            let qstr = positional.first().map(String::as_str).unwrap_or("");
            let req = spl::SplRequest { spl: qstr, earliest: None, latest: None };
            match spl::run(&conf.data_dir, &conf, &req, tstamp::now_ms()) {
                Ok(res) => {
                    println!("{}", res.table.fields.join("\t"));
                    for row in &res.table.rows {
                        let cells: Vec<String> = row.iter().map(fossick_spl::render_cell).collect();
                        println!("{}", cells.join("\t"));
                    }
                    eprintln!("{} rows ({} events matched)", res.table.rows.len(), res.matched);
                    Ok(())
                }
                Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                    eprintln!("fossick: {e}");
                    exit(2);
                }
                Err(e) => Err(e),
            }
        }
        "loggen" => {
            if port == 0 || count == 0 {
                eprintln!("fossick: loggen needs --port and --count");
                exit(2);
            }
            loggen::run(port, count)
        }
        _ => {
            eprintln!("{USAGE}");
            exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("fossick: {e}");
        exit(1);
    }
}
