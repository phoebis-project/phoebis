//! `phoebis check-config` warns about a wide key file mode, refuses a file that sets an unknown or removed key,
//! names the routing keys in its summary, and passes every shipped deploy config.

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
         proxy:\n  listen: 127.0.0.1:0\n\
         storage:\n  encryption_key_file: {}\n  path: {}\n{extra}",
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
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.get(4..9),
        Some(
            &[
                "profile claude-code-2.1.269-linux-x64",
                "routing: max_attempts 3, cooldown_fallback 60s, model_rejection 3600s, catalog refresh 21600s, \
                 startup fetch 30s",
                "ledger at ./data/ledger.sqlite",
                "retention 90 days, rollups 400 days",
                "keepalive: 128 sessions, 1024 agents, 256 MiB retained",
            ][..]
        ),
        "{stdout}"
    );
}

const REMOVED_KEY: &str = "routing:\n  catalog_cache_seconds: 60\n"; // removed name

#[test]
fn a_removed_key_fails_check_config_as_invalid_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "removed-key", REMOVED_KEY);
    let out = phoebis()
        .args(["check-config", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("the configuration is not valid YAML") && stderr.contains("unknown field"),
        "{stderr}"
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

#[test]
fn every_shipped_deploy_config_passes_check_config() {
    let deploy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy");
    for name in [
        "config.container.yaml",
        "config.example.yaml",
    ] {
        let out = phoebis()
            .env("PHOEBIS_ADMIN_SECRET", "deploy-check-admin-secret")
            .args(["check-config", "--config"])
            .arg(deploy.join(name))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
