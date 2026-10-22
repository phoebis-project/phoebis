//! Names the commit the binary is built from as `PHOEBIS_BUILD_COMMIT`, which `BUILD_COMMIT` reads.

use std::path::Path;

#[path = "build_commit.rs"]
mod build_commit;

fn main() {
    // An empty variable reads as unset, so a workflow that exports it blank still falls to git.
    let forced = std::env::var("PHOEBIS_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.is_empty());
    let (commit, inputs) =
        build_commit::build_commit(Path::new(env!("CARGO_MANIFEST_DIR")), forced.as_deref());
    println!("cargo:rerun-if-env-changed=PHOEBIS_BUILD_COMMIT");
    for input in inputs {
        println!("cargo:rerun-if-changed={}", input.display());
    }
    println!("cargo:rustc-env=PHOEBIS_BUILD_COMMIT={commit}");
}
