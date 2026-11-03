//! Mode and owner checks on the files the store's secrets live in. Warnings, never refusals: a compose secret
//! is bind-mounted with the host file's owner and mode, and refusing it would take the container down over a
//! file the operator cannot chmod from inside it. The two creators are for files the router writes once and never
//! overwrites.

use std::fs::{DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

/// Warns when `path` is readable or writable by group or other, or owned by a different uid than this
/// process. A path that cannot be read yields no warning; the open that follows reports that instead.
pub fn check_private_file(path: &Path, expected: u32) -> Vec<String> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        warnings.push(format!(
            "{} is readable by group or other (mode {mode:04o}) where {expected:04o} is expected",
            path.display()
        ));
    }
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        warnings.push(format!(
            "{} is owned by uid {}, not the process's uid {euid}",
            path.display(),
            metadata.uid()
        ));
    }
    warnings
}

pub fn check_private_dir(path: &Path) -> Vec<String> {
    check_private_file(path, 0o700)
}

/// Creates `path` at `mode`, refusing an existing file (`create_new`), under a directory created at 0700 when
/// missing. The open can only come out narrower than `mode`, since it honors the umask, so the mode is set on the new
/// descriptor before any byte is written: a caller's `umask 077` cannot narrow `init`'s `0644` configuration file to
/// 0600, no window exists in which the file is wider than its mode, and nothing is overwritten.
pub fn create_private_file(path: &Path, mode: u32) -> std::io::Result<File> {
    use std::os::unix::fs::PermissionsExt;

    create_private_dir(data_dir_of(path))?;
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(file)
}

/// Creates `path` at `mode` when it is missing, before SQLite opens it: SQLite creates a new database file under the
/// process umask, so a file it created could be readable by other local users, even briefly. An existing file is
/// opened without truncation and left as it is, which `create_private_file`'s `create_new` would refuse.
pub fn create_private_if_missing(path: &Path, mode: u32) -> std::io::Result<()> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(mode)
        .open(path)?;
    Ok(())
}

/// Creates `dir` at 0700 when missing; an existing directory is left as it is (its mode is the caller's to warn about).
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// The directory holding `path`: its parent, or `.` for a bare file name (an empty parent). Shared by
/// `Store::open`, which creates the directory, and `check-config`, which only checks it, so a database path
/// resolves to the same directory before either one exists.
pub fn data_dir_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_file_readable_by_group_or_other_warns_naming_the_path_and_the_expected_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, b"secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let warnings = check_private_file(&path, 0o600);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains(&path.display().to_string()));
        assert!(warnings[0].contains("0600"));
    }

    #[test]
    fn a_file_already_at_the_expected_mode_warns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, b"secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_private_file(&path, 0o600).is_empty());
    }

    #[test]
    fn a_private_file_is_created_at_its_mode_under_a_0700_directory() {
        let dir = tempfile::tempdir().unwrap();
        for (name, mode) in [("key", 0o600), ("config.yaml", 0o644)] {
            let path = dir.path().join("conf").join(name);
            create_private_file(&path, mode).unwrap();
            let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, mode, "{name}");
        }
        let dir_mode = std::fs::metadata(dir.path().join("conf"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn a_private_file_keeps_its_mode_under_a_narrower_umask() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        // An operator may run `init` under `umask 077`; the process-wide mask is restored before any assertion.
        // SAFETY: `umask` swaps the process-wide mask and cannot fail. Other tests in this binary run beside it, but
        // each sets its modes explicitly or creates at 0600 or 0700, which a 0077 mask leaves as they are.
        let previous = unsafe { libc::umask(0o077) };
        let created = create_private_file(&path, 0o644);
        // SAFETY: puts back the mask the call above returned, with the same reasoning.
        unsafe { libc::umask(previous) };
        created.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn a_private_file_is_never_overwritten() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        create_private_file(&path, 0o600)
            .unwrap()
            .write_all(b"first")
            .unwrap();
        let second = create_private_file(&path, 0o600).unwrap_err();
        assert_eq!(second.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
    }

    #[test]
    fn a_bare_file_names_the_current_directory() {
        assert_eq!(data_dir_of(Path::new("router.sqlite")), Path::new("."));
        assert_eq!(
            data_dir_of(Path::new("data/router.sqlite")),
            Path::new("data")
        );
    }
}
