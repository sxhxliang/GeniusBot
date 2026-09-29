//! Stage the web console for embedding.
//!
//! `web/dist` (the Vite build) is copied to `$OUT_DIR/web`, which
//! `rust-embed` compiles into the binary. Without a build, a placeholder page
//! explains how to make one, so the API still compiles and runs.
//! `GNS_SERVER_BUILD_WEB=1` runs `npm install` + `npm run build` first.

use std::path::Path;
use std::process::Command;

const PLACEHOLDER: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Genius Bot console</title></head>
<body style="font-family: system-ui, sans-serif; max-width: 40rem; margin: 4rem auto; line-height: 1.5">
<h1>Genius Bot console is not built</h1>
<p>The API is running (try <a href="/api/info">/api/info</a>), but this binary was compiled without the web console.</p>
<pre>cd crates/gns-server/web
npm install
npm run build
cd ../../..
cargo run -p gns-server</pre>
<p>Or build both in one go: <code>GNS_SERVER_BUILD_WEB=1 cargo build -p gns-server</code>.</p>
</body></html>
"#;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let web = Path::new(&manifest_dir).join("web");
    let dist = web.join("dist");
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-env-changed=GNS_SERVER_BUILD_WEB");

    if std::env::var("GNS_SERVER_BUILD_WEB").is_ok_and(|v| v == "1") {
        let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
        if !web.join("node_modules").exists() {
            run(Command::new(npm).arg("install").current_dir(&web));
        }
        run(Command::new(npm).args(["run", "build"]).current_dir(&web));
    }

    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("web");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("create $OUT_DIR/web");
    if dist.join("index.html").is_file() {
        copy_dir(&dist, &out).expect("copy web/dist");
    } else {
        println!("cargo:warning=gns-server: web/dist not found; embedding a placeholder page (see crates/gns-server/README.md)");
        std::fs::write(out.join("index.html"), PLACEHOLDER).expect("write placeholder");
    }
}

fn run(cmd: &mut Command) {
    let status = cmd.status().unwrap_or_else(|e| panic!("running {cmd:?}: {e}"));
    assert!(status.success(), "{cmd:?} failed with {status}");
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
