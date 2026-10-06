//! `phoebis check-config` warns about a wide key file mode, refuses a file that sets an unknown or removed key,
//! names the routing keys in its summary, and passes every shipped deploy config. It reads the key file as the start
//! does and proves the key against the registry's canary, or against the sealed rows of a registry from before the
//! canary, refuses a registry or a ledger a newer build wrote, and warns about an unset secret variable while
//! refusing a set one the start refuses.

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
    // Neither default path has a file under the test's working directory, so both tables are the shipped ones,
    // counted here from what they load.
    let prices = phoebis_ledger::PriceTable::shipped();
    let costs = phoebis_ledger::PlanTables::shipped().costs;
    let weighted = costs
        .model_weight
        .iter()
        .filter(|(_, weight)| weight.is_some_and(|w| w > 0.0))
        .map(|(model, _)| model)
        .chain(
            costs
                .overrides
                .iter()
                .filter(|(_, o)| o.weight_by_window.as_ref().is_some_and(|w| !w.is_empty()))
                .map(|(model, _)| model),
        )
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let sized = costs
        .capacity
        .values()
        .filter(|c| c.five_hour.is_some() || c.seven_day.is_some())
        .count();
    let api_prices = format!(
        "api prices: built in (effective {}, {} models); ./api_prices.yaml not found",
        prices.effective,
        prices.models.len()
    );
    let subscription_costs = format!(
        "subscription costs: built in (effective {}, {weighted} models, {sized} tiers); \
         ./subscription_costs.yaml not found",
        costs.effective
    );
    assert_eq!(
        lines.get(4..12),
        Some(
            &[
                "proxy: body budget 536870912 bytes, body read 60s, key body reads 128, max request 33554432 bytes, \
                 sign cch placeholder off, upstream gzip auto",
                "profile claude-code-2.1.269-linux-x64",
                "routing: cooldown_fallback 60s, model_rejection 3600s, catalog refresh 21600s, startup fetch 30s",
                "ledger at ./data/ledger.sqlite",
                api_prices.as_str(),
                subscription_costs.as_str(),
                "retention 90 days, rollups 400 days",
                "keepalive: 128 sessions, 1024 agents, 256 MiB retained",
            ][..]
        ),
        "{stdout}"
    );
}

/// The test tables, both dated 2026-01-01: four price rows; weights for Fable 5.1, Opus 5.5, Sonnet 5.5 and Haiku 4.5
/// with Opus 5 at null, and an override that weighs Haiku on the week; four tiers, Team sized on the 5-hour window
/// alone. So the cost model counts four models, the null weight left out and the overridden model counted once.
#[test]
fn files_at_both_paths_print_their_counts_leaving_out_a_null_weight_and_counting_an_overridden_model_once(
) {
    let dir = tempfile::tempdir().unwrap();
    let prices = dir.path().join("api_prices.yaml");
    let costs = dir.path().join("subscription_costs.yaml");
    std::fs::write(&prices, phoebis_ledger::test_tables::API_PRICES).unwrap();
    std::fs::write(&costs, phoebis_ledger::test_tables::SUBSCRIPTION_COSTS).unwrap();
    let config = config_file(
        dir.path(),
        "test-tables",
        &format!(
            "stats:\n  api_prices_path: {}\n  subscription_costs_path: {}\n",
            prices.display(),
            costs.display()
        ),
    );
    let out = check_config(&mut phoebis(), &config);
    assert!(out.status.success(), "{}", printed(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let api_prices = format!(
        "api prices: {} (file, effective 2026-01-01, 4 models)",
        prices.display()
    );
    let subscription_costs = format!(
        "subscription costs: {} (file, effective 2026-01-01, 4 models, 4 tiers)",
        costs.display()
    );
    assert_eq!(
        lines.get(7..10),
        Some(
            &[
                "ledger at ./data/ledger.sqlite",
                api_prices.as_str(),
                subscription_costs.as_str(),
            ][..]
        ),
        "{}",
        printed(&out)
    );
}

/// The start refuses a price file whose cell an estimate cannot use, so the check refuses it too, naming the file, the
/// model and the cell.
#[test]
fn a_price_file_with_a_negative_cell_fails_check_config_naming_the_model_and_the_cell() {
    let dir = tempfile::tempdir().unwrap();
    let prices = dir.path().join("api_prices.yaml");
    std::fs::write(
        &prices,
        "effective: 2026-10-04\nmodels:\n  m: {input: -1, cache_write_5m: 1, cache_write_1h: 1, cache_read: 1, \
         output: 1}\n",
    )
    .unwrap();
    let config = config_file(
        dir.path(),
        "negative-price",
        &format!("stats:\n  api_prices_path: {}\n", prices.display()),
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

/// The start refuses a cost model that breaks a rule of the file, so the check refuses it too, naming the file and
/// the key.
#[test]
fn a_cost_model_with_a_capacity_at_zero_fails_check_config_naming_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let costs = dir.path().join("costs.yaml");
    let text =
        phoebis_ledger::test_tables::SUBSCRIPTION_COSTS.replace("five_hour: 100,", "five_hour: 0,");
    assert_ne!(text, phoebis_ledger::test_tables::SUBSCRIPTION_COSTS);
    std::fs::write(&costs, text).unwrap();
    let config = config_file(
        dir.path(),
        "zero-capacity",
        &format!("stats:\n  subscription_costs_path: {}\n", costs.display()),
    );
    let out = check_config(&mut phoebis(), &config);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.lines().next(),
        Some(
            format!(
                "Error: loading the subscription cost model from {}",
                costs.display()
            )
            .as_str()
        ),
        "{}",
        printed(&out)
    );
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("plan table: capacity.default_claude_max_20x.five_hour")),
        "{}",
        printed(&out)
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

/// The price file's key carries the file's name, and nothing has been released, so the retired name is unknown
/// rather than read as an alias.
#[test]
fn a_configuration_naming_pricing_path_is_refused_as_invalid_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(
        dir.path(),
        "retired-key",
        "stats:\n  pricing_path: ./x.yaml\n",
    );
    let out = check_config(&mut phoebis(), &config);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.lines().next(),
        Some("Error: the configuration is not valid YAML"),
        "{}",
        printed(&out)
    );
    assert!(
        stderr.contains("unknown field `pricing_path`"),
        "{}",
        printed(&out)
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
