//! Stage the built web UI (ui/dist) into OUT_DIR so http.rs can embed it with
//! include_bytes!. If the UI hasn't been built, embed a placeholder page so
//! the binary still compiles and explains what to do.

use std::fs;
use std::path::Path;

const PLACEHOLDER: &str = "<!doctype html><title>fossick</title>\
<body style=\"font-family:sans-serif;margin:4rem\"><h1>fossick</h1>\
<p>The web UI is not embedded in this build.</p>\
<p>Run <code>npm install &amp;&amp; npm run build</code> in <code>ui/</code>, \
then rebuild fossick.</p></body>";

fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let dist = Path::new("../ui/dist");
    println!("cargo:rerun-if-changed=../ui/dist");
    fs::create_dir_all(format!("{out}/ui/assets")).unwrap();
    for (name, placeholder) in [
        ("index.html", PLACEHOLDER),
        ("assets/app.js", ""),
        ("assets/app.css", ""),
    ] {
        let src = dist.join(name);
        let dst = format!("{out}/ui/{name}");
        if src.exists() {
            fs::copy(&src, &dst).unwrap();
        } else {
            fs::write(&dst, placeholder).unwrap();
        }
    }
}
