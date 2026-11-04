//! The one walk `backup`, `init` and `recover-key` take to create the missing directories on the way to a path, so
//! the three agree on what counts as present and on which directories a run may remove again.

use std::fs::DirBuilder;
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// Where the walk stopped: the path it was checking or creating, and the error it met there.
#[derive(Debug, thiserror::Error)]
#[error("{step} {}: {error}", path.display())]
pub(crate) struct WalkError {
    pub(crate) step: &'static str,
    pub(crate) path: PathBuf,
    pub(crate) error: std::io::Error,
}

/// Creates `dir` and each missing parent of it at 0700, one component at a time from the top, recording each on
/// `created` as it lands. The walk up stops at anything `symlink_metadata` finds, a dangling link included, so a
/// link on the path is never followed into a directory the run then creates. A directory another process makes
/// between the walk and its create reads as found and stays off `created`, since it is not the run's to remove.
pub(crate) fn create_missing(dir: &Path, created: &mut Vec<PathBuf>) -> Result<(), WalkError> {
    create(&missing(dir)?, created)
}

/// `dir` and each parent of it that nothing stands at, from the top down.
fn missing(dir: &Path) -> Result<Vec<&Path>, WalkError> {
    let mut missing = Vec::new();
    for path in dir.ancestors() {
        if path.as_os_str().is_empty() {
            break;
        }
        match std::fs::symlink_metadata(path) {
            Ok(_) => break,
            Err(error) if error.kind() == ErrorKind::NotFound => missing.push(path),
            Err(error) => {
                return Err(WalkError {
                    step: "checking",
                    path: path.to_path_buf(),
                    error,
                })
            }
        }
    }
    missing.reverse();
    Ok(missing)
}

fn create(missing: &[&Path], created: &mut Vec<PathBuf>) -> Result<(), WalkError> {
    for path in missing {
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => created.push(path.to_path_buf()),
            Err(error)
                if error.kind() == ErrorKind::AlreadyExists
                    && std::fs::symlink_metadata(path).is_ok_and(|found| found.is_dir()) => {}
            Err(error) => {
                return Err(WalkError {
                    step: "creating",
                    path: path.to_path_buf(),
                    error,
                })
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_another_process_creates_between_the_scan_and_the_create_is_found_and_left_unrecorded(
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("a").join("b");
        let missing = missing(&target).unwrap();
        assert_eq!(missing, [tmp.path().join("a"), target.clone()]);
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        let mut created = Vec::new();
        create(&missing, &mut created).unwrap();
        assert_eq!(created, [target.clone()]);
        assert!(target.is_dir());
    }

    #[test]
    fn a_dangling_link_on_the_path_refuses_the_child_under_it_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        let nowhere = tmp.path().join("nowhere");
        std::os::unix::fs::symlink(&nowhere, &link).unwrap();
        let child = link.join("child");
        let mut created = Vec::new();
        let error = create_missing(&child, &mut created).unwrap_err();
        assert_eq!(error.error.kind(), ErrorKind::NotFound, "{error}");
        assert_eq!(error.path, child, "{error}");
        assert!(created.is_empty(), "{created:?}");
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!nowhere.exists());
    }
}
