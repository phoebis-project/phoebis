//! `phoebis check-config` warns about a wide key file mode, refuses a file that sets an unknown or removed key,
//! names the routing keys in its summary, and passes every shipped deploy config. It reads the key file as the start
//! does and proves the key against the registry's canary, or against the sealed rows of a registry from before the
//! canary, refuses a registry or a ledger a newer build wrote and a registry that holds an account in two pools, and
//! warns about an unset secret variable while refusing a set one the start refuses. It reads each listener's
//! certificate pair as the start does, prints each listener's scheme on any address, refuses a pair the start refuses
//! with the loader's line, and refuses a word in the `tls` key's place.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use phoebis_store::{
    AccountKind, AccountRow, AccountState, MemberWrite, NewBundle, PoolRow, Secret, StorageKey,
    Store,
};

const HEX_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn config_file(dir: &Path, name: &str, extra: &str) -> PathBuf {
    let key = dir.join("storage.key");
    std::fs::write(&key, HEX_KEY).unwrap();
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:0\n  secret_env: TEST_CHECK_CONFIG_ADMIN\n\
         proxy:\n  listen: 127.0.0.1:0\n\
         storage:\n  encryption_key_file: {}\n  path: {}\n{extra}",
        key.display(),
        dir.join("router.sqlite").display()
    );
    let path = dir.join(format!("{name}.yaml"));
    std::fs::write(&path, yaml).unwrap();
    path
}

/// `config_file`'s template with `admin_lines` inside its `admin` block: a second top-level `admin` would be refused
/// as a duplicate key, never as the unknown field a removed admin key has to prove.
fn config_file_with_admin(dir: &Path, name: &str, admin_lines: &str) -> PathBuf {
    let path = config_file(dir, name, "");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replacen("admin:\n", &format!("admin:\n{admin_lines}"), 1),
    )
    .unwrap();
    path
}

/// `config_file`'s template with `proxy_lines` inside its `proxy` block.
fn config_file_with_proxy(dir: &Path, name: &str, proxy_lines: &str) -> PathBuf {
    let path = config_file(dir, name, "");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replacen("proxy:\n", &format!("proxy:\n{proxy_lines}"), 1),
    )
    .unwrap();
    path
}

/// A pair valid between the two dates, written where an operator's would sit, both files at `0600`.
fn pair_files(
    dir: &Path,
    hosts: &[&str],
    not_before: (i32, u8, u8),
    not_after: (i32, u8, u8),
) -> (PathBuf, PathBuf) {
    let (cert, key) = fake_upstream::generate_pair_valid_between(hosts, not_before, not_after)
        .write_to(dir, "proxy");
    for file in [&cert, &key] {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    (cert, key)
}

/// The `proxy.tls` mapping naming the pair.
fn pair_lines(cert: &Path, key: &Path) -> String {
    format!(
        "  tls:\n    cert_file: {}\n    key_file: {}\n",
        cert.display(),
        key.display()
    )
}

/// The leaf's `not_after` as BoringSSL prints it, read from the file rather than from the router's loader.
fn not_after(cert: &Path) -> String {
    boring::x509::X509::from_pem(&std::fs::read(cert).unwrap())
        .unwrap()
        .not_after()
        .to_string()
}

/// A date `days` from today in UTC, as the fake upstream's generator takes it.
fn days_from_today(days: i64) -> (i32, u8, u8) {
    let date = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .checked_add(jiff::Span::new().days(days))
        .unwrap();
    (i32::from(date.year()), date.month() as u8, date.day() as u8)
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

/// The mismatch is the whole finding, so it stands on the `Error:` line itself rather than under a context line.
fn assert_refused_as_a_mismatch(out: &Output, registry: &Path) {
    assert_eq!(out.status.code(), Some(1), "{}", printed(out));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).lines().next(),
        Some(format!("Error: {}", mismatch_line(registry)).as_str()),
        "{}",
        printed(out)
    );
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
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.get(4..10),
        Some(
            &[
                "proxy: body budget 536870912 bytes, body read 60s, key body reads 128, max request 33554432 bytes, \
                 sign cch placeholder off, upstream gzip auto",
                "profile claude-code-2.1.269-linux-x64",
                "routing: cooldown_fallback 60s, model_rejection 3600s, catalog refresh 21600s, startup fetch 30s",
                "ledger at ./data/ledger.sqlite",
                "retention 90 days, rollups 400 days",
                "keepalive: 128 sessions, 1024 agents, 256 MiB retained",
            ][..]
        ),
        "{stdout}"
    );
}

/// The start refuses a price file whose cell an estimate cannot use, so the check refuses it too, naming the file, the
/// model and the cell.
#[test]
fn a_price_file_with_a_negative_cell_fails_check_config_naming_the_model_and_the_cell() {
    let dir = tempfile::tempdir().unwrap();
    let prices = dir.path().join("pricing.yaml");
    std::fs::write(
        &prices,
        "effective: 2026-10-04\nmodels:\n  m: {input: -1, cache_write_5m: 1, cache_write_1h: 1, cache_read: 1, \
         output: 1}\n",
    )
    .unwrap();
    let config = config_file(
        dir.path(),
        "negative-price",
        &format!("stats:\n  pricing_path: {}\n", prices.display()),
    );
    let out = check_config(&mut phoebis(), &config);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.lines().next(),
        Some(format!("Error: loading the price table from {}", prices.display()).as_str()),
        "{}",
        printed(&out)
    );
    assert!(
        stderr.contains("pricing table: model m, cell input must be a finite non-negative number"),
        "{}",
        printed(&out)
    );
}

/// Each removed key with the block it sat in and its lines.
const REMOVED_KEYS: [(&str, &str); 3] = [
    ("routing", "  catalog_cache_seconds: 60\n"), // removed name
    (
        "admin",
        include_str!("fixtures/removed-keys/admin-origin.yaml"),
    ),
    (
        "admin",
        include_str!("fixtures/removed-keys/admin-plain-http.yaml"),
    ),
];

#[test]
fn a_removed_key_fails_check_config_as_invalid_yaml() {
    let dir = tempfile::tempdir().unwrap();
    for (n, (block, lines)) in REMOVED_KEYS.into_iter().enumerate() {
        let name = format!("removed-key-{n}");
        let config = match block {
            "admin" => config_file_with_admin(dir.path(), &name, lines),
            _ => config_file(dir.path(), &name, &format!("{block}:\n{lines}")),
        };
        let key = lines
            .lines()
            .next()
            .and_then(|line| line.split_once(':'))
            .map(|(key, _)| key.trim())
            .unwrap();
        let out = check_config(&mut phoebis(), &config);
        assert!(!out.status.success(), "{block}: {lines}{}", printed(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("the configuration is not valid YAML")
                && stderr.contains(&format!("unknown field `{key}`")),
            "{block}: {lines}{}",
            printed(&out)
        );
    }
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
            phoebis().env("PHOEBIS_ADMIN_SECRET", "deploy-check-admin-secret"),
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
    assert_refused_as_a_mismatch(&out, &registry);
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
            "registry at {} has no key canary yet; a sealed row opens under the key, and the next start \
             seals one",
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
    assert_refused_as_a_mismatch(&out, &registry);
}

#[test]
fn a_registry_that_cannot_be_read_fails_check_config_naming_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "not-a-database", "");
    let registry = dir.path().join("router.sqlite");
    std::fs::write(&registry, "this text is not a SQLite database\n".repeat(8)).unwrap();
    let out = check_config(&mut phoebis(), &config);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.lines().next(),
        Some(format!("Error: reading the registry at {}", registry.display()).as_str()),
        "{}",
        printed(&out)
    );
    assert!(stderr.contains("not a database"), "{}", printed(&out));
}

#[test]
fn a_registry_a_newer_build_wrote_fails_check_config_naming_the_path_and_both_versions() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "newer-registry", "");
    let registry = dir.path().join("router.sqlite");
    let key = StorageKey::from_file(&dir.path().join("storage.key")).unwrap();
    Store::open(&registry, &key)
        .unwrap()
        .debug_set_schema_version(99)
        .unwrap();
    let out = check_config(&mut phoebis(), &config);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).lines().next(),
        Some(
            format!(
                "Error: the registry at {} is at schema 99, newer than the {} this build reads",
                registry.display(),
                phoebis_store::SCHEMA_VERSION
            )
            .as_str()
        ),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_registry_with_an_account_in_two_pools_fails_check_config_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "two-pools", "");
    let registry = dir.path().join("router.sqlite");
    let key = StorageKey::from_file(&dir.path().join("storage.key")).unwrap();
    let store = Store::open(&registry, &key).unwrap();
    store
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::ApiKey,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
        .unwrap();
    for id in ["p1", "p2"] {
        store.insert_pool(&PoolRow::for_tests(id), "test").unwrap();
    }
    store
        .set_pool_members(
            "p1",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    drop(store);
    // A registry written before the one-pool index, holding the overlap that index forbids.
    rusqlite::Connection::open(&registry)
        .unwrap()
        .execute_batch(
            "DROP INDEX pool_members_one_pool; UPDATE meta SET value = '45' WHERE key = 'schema_version'; \
             INSERT INTO pool_members (pool_id, account_id, priority, budget_daily_usd, budget_monthly_usd) \
             VALUES ('p2', 'a', 0, NULL, NULL);",
        )
        .unwrap();
    let out = check_config(&mut phoebis(), &config);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).lines().any(|line| line
            == "account a is a member of pools p1 and p2, and an account belongs to one pool"),
        "{}",
        printed(&out)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).lines().next(),
        Some(
            format!(
                "Error: the registry at {} holds an account in more than one pool, and an account belongs to one",
                registry.display()
            )
            .as_str()
        ),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_ledger_a_newer_build_wrote_fails_check_config_naming_the_path_and_both_versions() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("ledger.sqlite");
    let config = config_file(
        dir.path(),
        "newer-ledger",
        &format!("stats:\n  ledger_path: {}\n", ledger.display()),
    );
    phoebis_ledger::LedgerDb::open(&ledger)
        .unwrap()
        .write(|c| {
            c.execute(
                "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let out = check_config(&mut phoebis(), &config);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).lines().next(),
        Some(
            format!(
                "Error: the ledger at {} is at schema 99, newer than the {} this build reads",
                ledger.display(),
                phoebis_ledger::LedgerDb::supported_schema_version()
            )
            .as_str()
        ),
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
fn a_set_secret_holding_the_example_placeholder_fails_check_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "placeholder", "");
    let out = check_config(
        phoebis().env(
            "TEST_CHECK_CONFIG_ADMIN",
            "replace-with-a-long-random-string",
        ),
        &config,
    );
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(
            "environment variable TEST_CHECK_CONFIG_ADMIN still holds the example placeholder"
        ),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_plain_listener_prints_over_plain_http() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "plain", "");
    let out = check_config(&mut phoebis(), &config);
    assert!(out.status.success(), "{}", printed(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.get(1..4),
        Some(
            &[
                "proxy listens on 127.0.0.1:0 over plain HTTP",
                "admin listens on 127.0.0.1:0 over plain HTTP",
                "metrics listens on 127.0.0.1:9464",
            ][..]
        ),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_pair_prints_over_tls_with_its_certificate_and_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = pair_files(dir.path(), &["localhost"], (2020, 1, 1), (2100, 1, 1));
    let config = config_file_with_proxy(dir.path(), "pair", &pair_lines(&cert, &key));
    let out = check_config(&mut phoebis(), &config);
    assert!(out.status.success(), "{}", printed(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.get(1..3),
        Some(
            &[
                format!(
                    "proxy listens on 127.0.0.1:0 over TLS (certificate {}, expires {})",
                    cert.display(),
                    not_after(&cert)
                )
                .as_str(),
                "admin listens on 127.0.0.1:0 over plain HTTP",
            ][..]
        ),
        "{}",
        printed(&out)
    );
    // A key at 0600 and a leaf decades from expiry earn neither TLS warning. The storage key is written at the umask,
    // so its own warning may stand.
    let key = key.display().to_string();
    assert!(
        !stdout
            .lines()
            .any(|line| line.contains(&key) || line.contains("expires in")),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_pair_within_thirty_days_of_expiry_warns_and_passes() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = pair_files(
        dir.path(),
        &["localhost"],
        (2020, 1, 1),
        days_from_today(10),
    );
    let config = config_file_with_proxy(dir.path(), "near-expiry", &pair_lines(&cert, &key));
    let out = check_config(&mut phoebis(), &config);
    assert!(out.status.success(), "{}", printed(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let prefix = format!(
        "warning: proxy.tls.cert_file {} expires in ",
        cert.display()
    );
    // Midnight UTC ten days on is nine whole days and some hours from any moment of today, and eight when the check
    // runs past the midnight that followed the date's reading.
    let days = stdout
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no expiry warning: {}", printed(&out)));
    assert!(
        ["8 days (", "9 days (", "10 days ("]
            .iter()
            .any(|count| days.starts_with(count)),
        "{}",
        printed(&out)
    );
    assert!(
        days.ends_with(&format!("({})", not_after(&cert))),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_key_file_readable_by_others_warns() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = pair_files(dir.path(), &["localhost"], (2020, 1, 1), (2100, 1, 1));
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let config = config_file_with_proxy(dir.path(), "wide-tls-key", &pair_lines(&cert, &key));
    let out = check_config(&mut phoebis(), &config);
    assert!(out.status.success(), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).lines().any(|line| line
            == format!(
                "warning: {} is readable by group or other (mode 0644); expected 0600",
                key.display()
            )),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_refused_pair_fails_check_config_with_the_loaders_line() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = pair_files(dir.path(), &["localhost"], (2020, 1, 1), (2100, 1, 1));
    let missing = dir.path().join("absent.pem");
    let other = tempfile::tempdir().unwrap();
    let (_, other_key) = pair_files(other.path(), &["localhost"], (2020, 1, 1), (2100, 1, 1));
    let expired_dir = tempfile::tempdir().unwrap();
    let (expired, expired_key) = pair_files(
        expired_dir.path(),
        &["localhost"],
        (2020, 1, 1),
        (2021, 1, 1),
    );
    let early_dir = tempfile::tempdir().unwrap();
    let (early, early_key) = pair_files(
        early_dir.path(),
        &["localhost"],
        days_from_today(2),
        (2100, 1, 1),
    );
    let cases = [
        (
            "missing-cert",
            &missing,
            &key,
            "proxy.tls.cert_file ",
            missing.clone(),
            ": ",
        ),
        (
            "other-key",
            &cert,
            &other_key,
            "proxy.tls.key_file ",
            other_key.clone(),
            "does not match",
        ),
        (
            "expired",
            &expired,
            &expired_key,
            "proxy.tls.cert_file ",
            expired.clone(),
            "expired on",
        ),
        (
            "early",
            &early,
            &early_key,
            "proxy.tls.cert_file ",
            early.clone(),
            "not valid before",
        ),
    ];
    for (name, cert, key, named_key, path, reason) in cases {
        let config = config_file_with_proxy(dir.path(), name, &pair_lines(cert, key));
        let out = check_config(&mut phoebis(), &config);
        assert_eq!(out.status.code(), Some(1), "{name}: {}", printed(&out));
        assert!(out.stdout.is_empty(), "{name}: {}", printed(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stderr.lines().next().unwrap_or_default();
        assert!(
            line.starts_with(&format!("Error: {named_key}{}", path.display()))
                && line.contains(reason),
            "{name}: {}",
            printed(&out)
        );
    }
}

#[test]
fn a_listener_without_tls_on_an_unspecified_address_prints_over_plain_http() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "unspecified", "");
    let text = std::fs::read_to_string(&config).unwrap();
    // `check-config` opens no socket, so neither address is bound by the test.
    std::fs::write(
        &config,
        text.replacen(
            "admin:\n  listen: 127.0.0.1:0\n",
            "admin:\n  listen: \"[::]:0\"\n",
            1,
        )
        .replacen(
            "proxy:\n  listen: 127.0.0.1:0\n",
            "proxy:\n  listen: 0.0.0.0:0\n",
            1,
        ),
    )
    .unwrap();
    let out = check_config(&mut phoebis(), &config);
    assert!(out.status.success(), "{}", printed(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.get(1..4),
        Some(
            &[
                "proxy listens on 0.0.0.0:0 over plain HTTP",
                "admin listens on [::]:0 over plain HTTP",
                "metrics listens on 127.0.0.1:9464",
            ][..]
        ),
        "{}",
        printed(&out)
    );
}

#[test]
fn a_tls_word_fails_check_config_naming_the_two_keys() {
    let dir = tempfile::tempdir().unwrap();
    for word in ["self_signed", "plain_http"] {
        let config = config_file_with_proxy(dir.path(), word, &format!("  tls: {word}\n"));
        let out = check_config(&mut phoebis(), &config);
        assert_eq!(out.status.code(), Some(1), "{word}: {}", printed(&out));
        assert!(out.stdout.is_empty(), "{word}: {}", printed(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Error: the configuration is not valid YAML")
                && stderr.contains("proxy.tls: must be a mapping with cert_file and key_file"),
            "{word}: {}",
            printed(&out)
        );
    }
}
