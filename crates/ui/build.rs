//! Builds the web bundle into `web/dist` when it is missing, older than its sources or built for another commit, so
//! `cargo build` alone produces a binary with the UI in it. `PHOEBIS_SKIP_UI_BUILD=1` skips the Node step, and a
//! tree without a bundle then embeds the placeholder page so the crate still links.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

#[path = "build_support.rs"]
mod build_support;

#[path = "../router/build_commit.rs"]
mod build_commit;

use build_support::{bundle_stale, node_supported, parse_node_version, NODE_RANGES};

const PLACEHOLDER_MARKER: &str = "<!-- phoebis-ui placeholder -->";

/// Written inside `node_modules`, so wiping the directory takes the stamp with it.
const CI_STAMP: &str = ".phoebis-npm-ci-stamp";

/// The commit the bundle in `web/dist` was last built for, written in `OUT_DIR` after a build succeeds.
const COMMIT_STAMP: &str = "phoebis-bundle-commit";

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
        "api-reference",
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
    // The router's build script names the same commit from the same inputs, so the binary and the bundle it embeds
    // name one commit, and a commit that changes no file under `web/` still rebuilds the bundle.
    let forced = std::env::var("PHOEBIS_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.is_empty());
    let (commit, git_inputs) = build_commit::build_commit(&web, forced.as_deref());
    println!("cargo:rerun-if-env-changed=PHOEBIS_BUILD_COMMIT");
    for input in git_inputs {
        println!("cargo:rerun-if-changed={}", input.display());
    }
    let commit_stamp =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join(COMMIT_STAMP);
    let stamped = std::fs::read_to_string(&commit_stamp).ok();
    let built = newest(&dist.join("index.html"));
    let sources = inputs.iter().filter_map(|i| newest(&web.join(i))).max();
    // A placeholder is newer than every source the moment it is copied, so freshness alone would keep it for
    // ever; the marker makes it stale on every run that may build.
    let placeholder = std::fs::read_to_string(dist.join("index.html"))
        .is_ok_and(|page| page.contains(PLACEHOLDER_MARKER));
    let stale = bundle_stale(placeholder, built, sources, stamped.as_deref(), &commit);
    let skip = std::env::var_os("PHOEBIS_SKIP_UI_BUILD").is_some_and(|v| v == "1");
    if stale && !skip {
        let run = |args: &[&str]| {
            Command::new("npm")
                .args(args)
                .current_dir(&web)
                .env("PHOEBIS_BUILD_COMMIT", &commit)
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
        // Another Node fails deep inside the bundler with a message that names nothing an operator can act on, so
        // the ranges are checked and named before npm runs.
        let version = Command::new("node")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let found = if version.is_empty() {
            "no node on the path"
        } else {
            version.as_str()
        };
        assert!(
            parse_node_version(&version).is_some_and(node_supported),
            "the console build needs Node {NODE_RANGES}, and {found} was found"
        );
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
        if ok {
            // A stamp that could not be written costs one rebuild on the next run, nothing more.
            let _ = std::fs::write(&commit_stamp, &commit);
        } else {
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
            println!(
                "cargo:warning=the web bundle could not be built, so web/dist was discarded and the router will \
                 serve the placeholder page. Set PHOEBIS_SKIP_UI_BUILD=1 to embed the bundle on disk as it is."
            );
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
