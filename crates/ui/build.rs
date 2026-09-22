//! Builds the web bundle into `web/dist` when it is missing or older than its sources, so `cargo build` alone
//! produces a binary with the UI in it. `PHOEBIS_SKIP_UI_BUILD=1` skips the Node step; a tree without a bundle then
//! embeds the placeholder page so the crate still links.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

const PLACEHOLDER_MARKER: &str = "<!-- phoebis-ui placeholder -->";

/// Written inside `node_modules`, so wiping the directory takes the stamp with it.
const CI_STAMP: &str = ".phoebis-npm-ci-stamp";

fn newest(path: &Path) -> Option<SystemTime> {
    let meta = std::fs::metadata(path).ok()?;
    // A directory's own mtime moves when an entry is created, deleted, or renamed, which no surviving file's
    // mtime records; without it a deleted source leaves the bundle looking fresh.
    let mut newest = meta.modified().ok();
    if meta.is_file() {
        return newest;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return newest;
    };
    for entry in entries.flatten() {
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
        // Judged against a stamp written only after `npm ci` succeeds, rather than against node_modules' own
        // mtime, which a half-finished install bumps just as well.
        let stamp = web.join("node_modules").join(CI_STAMP);
        let from_this_lockfile = match (newest(&stamp), newest(&web.join("package-lock.json"))) {
            (Some(stamped), Some(lock)) => stamped >= lock,
            _ => false,
        };
        // `npm ci` needs the network only when the lockfile's packages are not in the npm cache.
        let modules = from_this_lockfile || {
            let installed = run(&["ci", "--no-audit", "--no-fund"]);
            if installed {
                let _ = std::fs::write(&stamp, "");
            }
            installed
        };
        // A lockfile bump makes the bundle stale, so building without the install behind it would embed a bundle
        // made from the modules the lockfile has just replaced.
        let ok = modules && run(&["run", "build"]);
        if !ok {
            // Whatever is in `dist` was built from sources this binary is no longer being built from, so it
            // goes and the placeholder below takes its place: a release that quietly embeds the previous UI
            // reports nothing wrong and ships the wrong console.
            if let Err(e) = std::fs::remove_dir_all(&dist) {
                assert!(
                    e.kind() == std::io::ErrorKind::NotFound,
                    "the stale web bundle in {} could not be removed after the build failed: {e}",
                    dist.display()
                );
            }
            println!("cargo:warning=the web bundle could not be built; web/dist was discarded and the router will serve the placeholder page. Set PHOEBIS_SKIP_UI_BUILD=1 to embed the bundle on disk as it is.");
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
