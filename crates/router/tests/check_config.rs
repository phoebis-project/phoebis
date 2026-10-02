//! `phoebis check-config` warns about a wide key file mode, refuses a file that sets an unknown key, names the routing
//! keys in its summary, and passes every shipped deploy config. It reads the key file as the start does and proves the
//! key against the registry's canary, or against the first sealed row of a registry from before the canary, and
//! warns about an unset secret variable while refusing a set one the start refuses.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use phoebis_store::{AccountKind, AccountRow, AccountState, NewBundle, Secret, StorageKey, Store};

const HEX_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn config_file(dir: &Path, name: &str, extra: &str) -> PathBuf {
    let key = dir.join("storage.key");
    std::fs::write(&key, HEX_KEY).unwrap();
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:0\n  plain_http: true\n  secret_env: TEST_CHECK_CONFIG_ADMIN\n\
         compatibility:\n  profile: claude-code-2.1.269-linux-x64\nproxy:\n  listen: 127.0.0.1:0\n\
         storage:\n  encryption_key_file: {}\n  path: {}\nusage:\n  cache_ttl_seconds: 60\n  \
         refresh_concurrency: 2\n{extra}",
        key.display(),
        dir.join("router.sqlite").display()
    );
    let path = dir.join(format!("{name}.yaml"));
    std::fs::write(&path, yaml).unwrap();
    path
}

/// `config_file`'s `extra` lands after `usage:`, so a file naming the viewer's variable inside `admin:` is written
/// whole here.
fn config_with_viewer(dir: &Path, name: &str) -> PathBuf {
    let key = dir.join("storage.key");
    std::fs::write(&key, HEX_KEY).unwrap();
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:0\n  plain_http: true\n  secret_env: TEST_CHECK_CONFIG_ADMIN\n  \
         viewer_secret_env: TEST_CHECK_CONFIG_VIEWER\n\
         compatibility:\n  profile: claude-code-2.1.269-linux-x64\nproxy:\n  listen: 127.0.0.1:0\n\
         storage:\n  encryption_key_file: {}\n  path: {}\nusage:\n  cache_ttl_seconds: 60\n  \
         refresh_concurrency: 2\n",
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

fn check_config(command: &mut Command, config: &Path) -> Output {
    command
        .args(["check-config", "--config"])
        .arg(config)
        .output()
        .unwrap()
}

fn printed(out: &Output) -> String {
    format!(
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn mismatch_line(registry: &Path) -> String {
    format!(
        "the storage key does not match the registry at {}",
        registry.display()
    )
}

/// A registry holding one committed credential bundle and no canary, the shape a registry sealed before the canary
/// existed has.
fn registry_from_before_the_canary(registry: &Path, key: &StorageKey) {
    let store = Store::open(registry, key).unwrap();
    store
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::Oauth,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Unpaired,
        })
        .unwrap();
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("at-test-not-a-token"),
                refresh_token: Some(Secret::new("rt-test-not-a-token")),
                issued_at: 1_799_996_400_000,
                expires_at: 1_800_000_000_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    store.debug_drop_canary().unwrap();
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

/// The shipped key paths do not exist on a test host and an absent key file refuses the check, so each file is
/// copied with its two `storage` members pointed into the temp directory and nothing else changed.
#[test]
fn every_shipped_deploy_config_passes_check_config() {
    let deploy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy");
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("storage.key");
    std::fs::write(&key, HEX_KEY).unwrap();
    for name in [
        "config.container.yaml",
        "config.example.yaml",
    ] {
        let registry = dir.path().join(format!("{name}.sqlite"));
        let shipped = std::fs::read_to_string(deploy.join(name)).unwrap();
        let mut in_storage = false;
        let mut replaced = 0;
        let mut copy = String::new();
        for line in shipped.lines() {
            if !line.starts_with(' ') {
                in_storage = line == "storage:";
            }
            let member = line.trim_start();
            let indent = &line[..line.len() - member.len()];
            let line = if in_storage && member.starts_with("encryption_key_file:") {
                replaced += 1;
                format!("{indent}encryption_key_file: {}", key.display())
            } else if in_storage && member.starts_with("path:") {
                replaced += 1;
                format!("{indent}path: {}", registry.display())
            } else {
                line.to_string()
            };
            copy.push_str(&line);
            copy.push('\n');
        }
        assert_eq!(replaced, 2, "{name}: the storage block's two members");
        let config = dir.path().join(name);
        std::fs::write(&config, copy).unwrap();
        let out = check_config(
            phoebis()
                .env("PHOEBIS_ADMIN_SECRET", "deploy-check-admin-secret")
                .env("PHOEBIS_VIEWER_SECRET", "deploy-check-viewer-secret"),
            &config,
        );
        assert!(out.status.success(), "{name}: {}", printed(&out));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(&format!(
                "registry at {} does not exist yet; the first start creates it",
                registry.display()
            )),
            "{name}: {}",
            printed(&out)
        );
    }
}

#[test]
fn a_registry_sealed_under_another_key_fails_check_config_naming_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "mismatch", "");
    let registry = dir.path().join("router.sqlite");
    drop(Store::open(&registry, &StorageKey::generate()).unwrap());
    let out = check_config(&mut phoebis(), &config);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&mismatch_line(&registry)),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_registry_from_before_the_canary_passes_under_its_key_and_names_the_sealing_to_come() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "legacy", "");
    let registry = dir.path().join("router.sqlite");
    let key = StorageKey::from_file(&dir.path().join("storage.key")).unwrap();
    registry_from_before_the_canary(&registry, &key);
    let out = check_config(&mut phoebis(), &config);
    assert!(out.status.success(), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&format!(
            "registry at {} has no key canary yet; its first sealed row opens under the key, and the next \
             start seals one",
            registry.display()
        )),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_registry_from_before_the_canary_fails_check_config_under_another_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "legacy-mismatch", "");
    let registry = dir.path().join("router.sqlite");
    registry_from_before_the_canary(&registry, &StorageKey::generate());
    let out = check_config(&mut phoebis(), &config);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&mismatch_line(&registry)),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_key_file_with_a_trailing_newline_passes() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "newline", "");
    let key = dir.path().join("storage.key");
    std::fs::write(&key, format!("{HEX_KEY}\n")).unwrap();
    assert_eq!(std::fs::metadata(&key).unwrap().len(), 65);
    let out = check_config(&mut phoebis(), &config);
    assert!(out.status.success(), "{}", printed(&out));
    // The registry line follows the key file's read, so it proves the key was read and accepted.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("does not exist yet"),
        "{}",
        printed(&out)
    );
}

#[test]
fn an_absent_key_file_fails_check_config_as_the_start_does() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "no-key", "");
    let key = dir.path().join("storage.key");
    std::fs::remove_file(&key).unwrap();
    let out = check_config(&mut phoebis(), &config);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&format!(
            "reading storage.encryption_key_file {}",
            key.display()
        )),
        "{}",
        printed(&out)
    );
}

#[test]
fn an_absent_registry_prints_that_the_first_start_creates_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "fresh", "");
    let out = check_config(&mut phoebis(), &config);
    assert!(out.status.success(), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&format!(
            "registry at {} does not exist yet; the first start creates it",
            dir.path().join("router.sqlite").display()
        )),
        "{}",
        printed(&out)
    );
}

#[test]
fn unset_secret_variables_warn_and_the_file_still_passes() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "unset", "");
    let out = check_config(phoebis().env_remove("TEST_CHECK_CONFIG_ADMIN"), &config);
    assert!(out.status.success(), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains("warning: environment variable TEST_CHECK_CONFIG_ADMIN is unset or empty"),
        "{}",
        printed(&out)
    );
}

/// A guard: the start's refusal of a short secret holds in `check-config` too.
#[test]
fn a_set_secret_shorter_than_sixteen_characters_fails_check_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "short", "");
    let out = check_config(phoebis().env("TEST_CHECK_CONFIG_ADMIN", "short"), &config);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(
            "environment variable TEST_CHECK_CONFIG_ADMIN holds a secret shorter than 16 characters"
        ),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_set_secret_the_start_refuses_fails_check_config_beside_an_unset_one() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_viewer(dir.path(), "placeholder");
    let out = check_config(
        phoebis().env_remove("TEST_CHECK_CONFIG_ADMIN").env(
            "TEST_CHECK_CONFIG_VIEWER",
            "replace-with-a-different-long-random-string",
        ),
        &config,
    );
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(
            "environment variable TEST_CHECK_CONFIG_VIEWER still holds the example placeholder"
        ),
        "{}",
        printed(&out)
    );
}

#[test]
fn two_equal_secrets_still_fail() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_viewer(dir.path(), "equal");
    let out = check_config(
        phoebis().env("TEST_CHECK_CONFIG_VIEWER", "admin-secret-checkconfig"),
        &config,
    );
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("the admin secret and the viewer secret resolve to the same secret"),
        "{}",
        printed(&out)
    );
}
