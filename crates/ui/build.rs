//! Builds the web bundle into `web/dist` when it is missing or older than its sources, so `cargo build` alone
//! produces a binary with the UI in it. `PHOEBIS_SKIP_UI_BUILD=1` skips the Node step; a tree without a bundle then
//! embeds the placeholder page so the crate still links.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

const PLACEHOLDER_MARKER: &str = "<!-- phoebis-ui placeholder -->";

fn newest(path: &Path) -> Option<SystemTime> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.is_file() {
        return meta.modified().ok();
    }
    let mut newest = None;
    for entry in std::fs::read_dir(path).ok()?.flatten() {
        if let Some(t) = self::newest(&entry.path()) {
            newest = Some(newest.map_or(t, |n: SystemTime| n.max(t)));
        }
    }
    newest
}

fn main() {
    let web = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web");
    let dist = web.join("dist");
    let inputs = [
        "src",
        "public",
        "index.html",
        "package.json",
        "package-lock.json",
        "tsconfig.json",
        "vite.config.ts",
    ];
    for input in inputs {
        println!("cargo:rerun-if-changed={}", web.join(input).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        dist.join("index.html").display()
    );
    println!("cargo:rerun-if-env-changed=PHOEBIS_SKIP_UI_BUILD");
    let built = newest(&dist.join("index.html"));
    let sources = inputs.iter().filter_map(|i| newest(&web.join(i))).max();
    // A placeholder is newer than every source the moment it is copied, so freshness alone would keep it for
    // ever; the marker makes it stale on every run that may build.
    let placeholder = std::fs::read_to_string(dist.join("index.html"))
        .is_ok_and(|page| page.contains(PLACEHOLDER_MARKER));
    let stale = placeholder
        || match (built, sources) {
            (None, _) => true,
            (Some(b), Some(s)) => s > b,
            (Some(_), None) => false,
        };
    let skip = std::env::var_os("PHOEBIS_SKIP_UI_BUILD").is_some_and(|v| v == "1");
    if stale && !skip {
        let run = |args: &[&str]| {
            Command::new("npm")
                .args(args)
                .current_dir(&web)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        // `npm ci` needs the network only when the lockfile's packages are not in the npm cache.
        let ok = (web.join("node_modules").exists() || run(&["ci", "--no-audit", "--no-fund"]))
            && run(&["run", "build"]);
        if !ok {
            println!("cargo:warning=the web bundle could not be built; the router will serve a placeholder page. Set PHOEBIS_SKIP_UI_BUILD=1 to silence this.");
        }
    }
    if !dist.join("index.html").exists() {
        std::fs::create_dir_all(&dist).expect("web/dist");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("placeholder/index.html"),
            dist.join("index.html"),
        )
        .expect("the placeholder page");
    }
}
