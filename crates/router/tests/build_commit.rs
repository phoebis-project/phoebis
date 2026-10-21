//! The commit a build names and the Git paths its build scripts rerun on, read in a repository of the test's own
//! under a temporary directory: a checkout on one branch and a linked worktree on another, their refs loose and then
//! packed, a call from a subdirectory of the checkout, and a detached `HEAD`.

#[path = "../build_commit.rs"]
mod build_commit;

use std::path::{Path, PathBuf};
use std::process::Command;

use build_commit::build_commit;

/// `git -C dir <args>`, the answer trimmed. The host's own configuration stays out, since a global file may sign every
/// commit or name no identity.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=phoebis",
            "-c",
            "user.email=phoebis@example.invalid",
        ])
        .args(["-c", "commit.gpgsign=false", "-C"])
        .arg(dir)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(tree: &Path, message: &str) -> String {
    git(tree, &["commit", "-q", "--allow-empty", "-m", message]);
    git(tree, &["rev-parse", "HEAD"])
}

/// A checkout on `main` with one commit and a linked worktree on `side` from it, both under `root`.
fn repository(root: &Path) -> (PathBuf, PathBuf) {
    let checkout = root.join("checkout");
    let worktree = root.join("worktree");
    git(
        root,
        &["init", "-q", "-b", "main", checkout.to_str().unwrap()],
    );
    commit(&checkout, "first");
    git(
        &checkout,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            worktree.to_str().unwrap(),
        ],
    );
    (checkout, worktree)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// A watched path as cargo's rerun check sees it change: a file's bytes, or every file under a directory with its
/// bytes.
fn contents(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    if path.is_dir() {
        let mut files: Vec<(PathBuf, Vec<u8>)> = std::fs::read_dir(path)
            .unwrap()
            .flat_map(|entry| contents(&entry.unwrap().path()))
            .collect();
        files.sort();
        files
    } else {
        vec![(path.to_owned(), std::fs::read(path).unwrap())]
    }
}

/// The paths the tree's build would watch, canonical, each one present on disk.
fn watched(tree: &Path) -> Vec<PathBuf> {
    let (_, inputs) = build_commit(tree, None);
    inputs.iter().map(|input| canonical(input)).collect()
}

/// `tree`'s inputs name `head` and `branch_ref` (each canonical) and the common directory's `packed-refs` where it
/// stands, and a commit on the branch moves the ref's contents while `HEAD`'s stay, so cargo reruns the script, whose
/// answer moves to the new commit.
fn a_commit_reruns_the_build_and_moves_its_answer(
    tree: &Path,
    head: &Path,
    branch_ref: &Path,
    packed_refs: &Path,
) {
    let before = git(tree, &["rev-parse", "HEAD"]);
    assert_eq!(build_commit(tree, None).0, before);
    let inputs = watched(tree);
    assert!(inputs.contains(&canonical(head)), "{head:?} in {inputs:?}");
    assert!(
        inputs.contains(&canonical(branch_ref)),
        "the branch's ref {branch_ref:?} in {inputs:?}"
    );
    if packed_refs.exists() {
        assert!(
            inputs.contains(&canonical(packed_refs)),
            "{packed_refs:?} in {inputs:?}"
        );
    }
    let head_before = contents(head);
    let ref_before = contents(branch_ref);

    let after = commit(tree, "second");
    assert_ne!(after, before);
    assert_eq!(
        contents(head),
        head_before,
        "a commit on the branch leaves HEAD as it was"
    );
    assert_ne!(
        contents(branch_ref),
        ref_before,
        "a commit on the branch moves its ref"
    );
    assert_eq!(build_commit(tree, None).0, after);
}

#[test]
fn a_commit_on_the_branch_reruns_the_build_through_its_loose_ref_in_a_checkout_and_a_linked_worktree(
) {
    let root = tempfile::tempdir().unwrap();
    let (checkout, worktree) = repository(root.path());
    let git_dir = checkout.join(".git");
    let packed_refs = git_dir.join("packed-refs");
    a_commit_reruns_the_build_and_moves_its_answer(
        &checkout,
        &git_dir.join("HEAD"),
        &git_dir.join("refs/heads/main"),
        &packed_refs,
    );
    a_commit_reruns_the_build_and_moves_its_answer(
        &worktree,
        &git_dir.join("worktrees/worktree/HEAD"),
        &git_dir.join("refs/heads/side"),
        &packed_refs,
    );
}

#[test]
fn a_commit_on_the_branch_reruns_the_build_through_the_refs_directory_where_the_refs_are_packed() {
    let root = tempfile::tempdir().unwrap();
    let (checkout, worktree) = repository(root.path());
    git(&checkout, &["pack-refs", "--all"]);
    let git_dir = checkout.join(".git");
    let packed_refs = git_dir.join("packed-refs");
    assert!(packed_refs.is_file() && !git_dir.join("refs/heads/main").exists());
    for tree in [&checkout, &worktree] {
        let inputs = watched(tree);
        assert!(
            inputs.contains(&canonical(&packed_refs)),
            "packed-refs in {inputs:?}"
        );
    }
    a_commit_reruns_the_build_and_moves_its_answer(
        &checkout,
        &git_dir.join("HEAD"),
        &git_dir.join("refs/heads"),
        &packed_refs,
    );
    git(&checkout, &["pack-refs", "--all"]);
    a_commit_reruns_the_build_and_moves_its_answer(
        &worktree,
        &git_dir.join("worktrees/worktree/HEAD"),
        &git_dir.join("refs/heads"),
        &packed_refs,
    );
}

#[test]
fn a_build_from_a_subdirectory_names_the_checkouts_head_and_its_branch_ref() {
    let root = tempfile::tempdir().unwrap();
    let (checkout, _worktree) = repository(root.path());
    // One level down, as the console's script calls it from `web/`, where git answers each path relative to it.
    let subdirectory = checkout.join("web");
    std::fs::create_dir(&subdirectory).unwrap();
    let git_dir = checkout.join(".git");
    let (answer, inputs) = build_commit(&subdirectory, None);
    assert_eq!(answer, git(&checkout, &["rev-parse", "HEAD"]));
    assert!(inputs.iter().all(|input| input.exists()), "{inputs:?}");
    let inputs: Vec<PathBuf> = inputs.iter().map(|input| canonical(input)).collect();
    for path in [git_dir.join("HEAD"), git_dir.join("refs/heads/main")] {
        assert!(inputs.contains(&canonical(&path)), "{path:?} in {inputs:?}");
    }
}

#[test]
fn a_detached_head_is_watched_itself_and_a_commit_on_it_reruns_the_build_and_moves_its_answer() {
    let root = tempfile::tempdir().unwrap();
    let (checkout, _worktree) = repository(root.path());
    git(&checkout, &["checkout", "-q", "--detach"]);
    let git_dir = checkout.join(".git");
    let head = git_dir.join("HEAD");
    let before = git(&checkout, &["rev-parse", "HEAD"]);
    assert_eq!(build_commit(&checkout, None).0, before);
    let inputs = watched(&checkout);
    assert!(inputs.contains(&canonical(&head)), "HEAD in {inputs:?}");
    let refs = canonical(&git_dir.join("refs"));
    assert!(
        !inputs.iter().any(|input| input.starts_with(&refs)),
        "a detached HEAD names no branch ref to watch: {inputs:?}"
    );
    let head_before = contents(&head);
    let after = commit(&checkout, "detached");
    assert_ne!(after, before);
    assert_ne!(
        contents(&head),
        head_before,
        "a commit on a detached HEAD moves HEAD itself"
    );
    assert_eq!(build_commit(&checkout, None).0, after);
}

#[test]
fn the_variable_names_the_build_ahead_of_git_and_a_tree_without_git_names_unknown() {
    let root = tempfile::tempdir().unwrap();
    let (checkout, _worktree) = repository(root.path());
    let forced = "fedcba9876543210fedcba9876543210fedcba98";
    assert_eq!(
        build_commit(&checkout, Some(forced)),
        (forced.to_owned(), Vec::new())
    );
    let bare = tempfile::tempdir().unwrap();
    assert_eq!(
        build_commit(bare.path(), None),
        ("unknown".to_owned(), Vec::new())
    );
}
