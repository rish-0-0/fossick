use fossick::{conf::Conf, search, serve, tstamp};
use std::process::exit;

const USAGE: &str = "usage:
  fossick serve  [--conf <path>]
  fossick search \"<query>\" [--conf <path>]

query: bare terms (substring match), field=value, earliest=/latest= (-15m, now, ISO, epoch)
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
    while let Some(a) = args.next() {
        if a == "--conf" {
            conf_path = args.next();
            if conf_path.is_none() {
                eprintln!("fossick: --conf needs a path");
                exit(2);
            }
        } else {
            positional.push(a);
        }
    }
    let conf = load_conf(conf_path);
    let result = match cmd.as_str() {
        "serve" => serve::run(conf),
        "search" => {
            let qstr = positional.first().map(String::as_str).unwrap_or("");
            let q = match search::parse_query(qstr, tstamp::now_ms()) {
                Ok(q) => q,
                Err(e) => {
                    eprintln!("fossick: {e}");
                    exit(2);
                }
            };
            search::run(&conf.data_dir, &q, |ev, _| {
                println!("{}  {}", tstamp::to_iso(ev.time_ms), String::from_utf8_lossy(&ev.raw));
            })
            .map(|hits| eprintln!("{hits} events"))
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
