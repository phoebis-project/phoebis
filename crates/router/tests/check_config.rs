//! `phoebis check-config` warns about a wide key file mode and refuses a file that sets an unknown key.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn config_file(dir: &Path, name: &str, extra: &str) -> PathBuf {
    let key = dir.join("storage.key");
    std::fs::write(
        &key,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:0\n  plain_http: true\n  secret_env: TEST_CHECK_CONFIG_ADMIN\n\
         compatibility:\n  outbound_gzip: false\n  profile: claude-code-2.1.269-linux-x64\n  \
         usage_entrypoint: cli\nproxy:\n  listen: 127.0.0.1:0\nstorage:\n  encryption_key_file: {}\n  \
         path: {}\nusage:\n  cache_ttl_seconds: 60\n  refresh_concurrency: 2\n{extra}",
        key.display(),
        dir.join("router.sqlite").display()
    );
    let path = dir.join(format!("{name}.yaml"));
    std::fs::write(&path, yaml).unwrap();
    path
}

fn phoebis() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_phoebis"));
    command
        .env("TEST_CHECK_CONFIG_ADMIN", "admin-secret-checkconfig")
        .env_remove("PHOEBIS_CONFIG");
    command
}

#[test]
fn a_key_file_readable_by_group_or_other_warns_naming_the_path_and_0600() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "wide-key", "");
    let key = dir.path().join("storage.key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = phoebis()
        .args(["check-config", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let warning = stdout
        .lines()
        .find(|line| line.starts_with("warning:"))
        .unwrap_or_else(|| panic!("no warning line printed: {stdout}"));
    assert!(
        warning.contains(&key.display().to_string()) && warning.contains("0600"),
        "{warning}"
    );
}

#[test]
fn the_summary_names_the_routing_keys() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "defaults", "");
    let out = phoebis()
        .args(["check-config", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("catalog refresh 21600s, startup fetch 5s"),
        "{stdout}"
    );
}

#[test]
fn an_unknown_upstream_key_fails_check_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "unknown-key", "upstream:\n  resolver: 1\n");
    let out = phoebis()
        .args(["check-config", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("resolver"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
