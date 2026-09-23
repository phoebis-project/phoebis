//! Mode and owner checks on the files the store's secrets live in. Warnings, never refusals: a compose secret
//! is bind-mounted with the host file's owner and mode, and refusing it would take the container down over a
//! file the operator cannot chmod from inside it.

use std::os::unix::fs::MetadataExt;
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
            "{} is readable by group or other (mode {mode:04o}); expected {expected:04o}",
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
    fn a_directory_readable_by_group_or_other_warns_naming_0700() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let warnings = check_private_dir(dir.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("0700"));
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
