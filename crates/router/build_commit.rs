//! The commit a build names, shared by the router's and the console's build scripts so the binary and its bundle
//! name one commit, and by the router's test of it. The standard library alone, so neither script takes a build
//! dependency.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The commit to name and the paths cargo reruns the script on: `forced` (`PHOEBIS_BUILD_COMMIT`) where set, else
/// `git rev-parse HEAD` in `dir`, else `unknown`, with `HEAD`, the current branch's ref (its loose file, or its
/// directory where the ref is packed) and `packed-refs`, each resolved through `git rev-parse --git-path`, so a linked
/// worktree names its own `HEAD` and the common directory's refs.
pub fn build_commit(dir: &Path, forced: Option<&str>) -> (String, Vec<PathBuf>) {
    if let Some(commit) = forced {
        return (commit.to_owned(), Vec::new());
    }
    let Some(commit) = git(dir, &["rev-parse", "HEAD"]) else {
        return ("unknown".to_owned(), Vec::new());
    };
    // A commit on a branch rewrites the branch's ref and leaves `HEAD` as it was, so `HEAD` alone never reruns the
    // script. A detached `HEAD` names no branch, and there `HEAD` itself moves with a commit.
    let branch = git(dir, &["symbolic-ref", "-q", "HEAD"]);
    let path = |name: &str| git(dir, &["rev-parse", "--git-path", name]).map(|path| dir.join(path));
    // Cargo reruns a script on every build while a watched path is missing, so each is named only where it stands. A
    // packed ref has no file of its own until the next commit writes one, which moves the directory that receives it,
    // so the nearest existing directory stands in for it, the whole `refs/heads` where nothing else stands. Any
    // branch's new loose ref there, a `git branch` or a commit in another worktree, then reruns both build scripts and
    // recompiles `phoebis-router` until the next run names the loose file, the answer staying right.
    let mut inputs: Vec<PathBuf> = ["HEAD", "packed-refs"]
        .into_iter()
        .filter_map(&path)
        .filter(|input| input.exists())
        .collect();
    if let Some(branch_ref) = branch.as_deref().and_then(path) {
        inputs.extend(
            branch_ref
                .ancestors()
                .find(|input| input.exists())
                .map(Path::to_owned),
        );
    }
    (commit, inputs)
}

/// The trimmed answer of `git -C dir <args>`, or `None` where git is missing or refuses.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    let answer = String::from_utf8(output.stdout).ok()?;
    let answer = answer.trim();
    (output.status.success() && !answer.is_empty()).then(|| answer.to_owned())
}
