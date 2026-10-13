//! `phoebis check-config` warns about a wide key file mode, refuses a file that sets an unknown or removed key,
//! names the routing keys in its summary, and passes every shipped deploy config. It reads the key file as the start
//! does and proves the key against the registry's canary, or against the sealed rows of a registry from before the
//! canary, refuses a registry or a ledger a newer build wrote and a registry that holds an account in two pools, and
//! warns about an unset secret variable while refusing a set one the start refuses. It reads each listener's
//! certificate pair as the start does, prints each listener's scheme on any address, refuses a pair the start refuses
//! with the loader's line, and refuses a word in the `tls` key's place.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use phoebis_store::{
    AccountKind, AccountRow, AccountState, MemberWrite, NewBundle, PoolRow, Secret, StateCause,
    StorageKey, Store,
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
            state_cause: StateCause::Operator,
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
    // The binary reads its clock between these two, so a midnight that starts an entry while it runs leaves its lines
    // equal to the ones built at one of them.
    let before = jiff::Timestamp::now().as_millisecond();
    let out = phoebis()
        .args(["check-config", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    let after = jiff::Timestamp::now().as_millisecond();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    // Neither default path has a file under the test's working directory, so both lines read the shipped lists. Each
    // expected line is built from its list as the loader reads it, so a new revision edits no test. The counts' rule
    // is proven over the hand-counted tables below.
    let listed = |revisions: usize, in_force: &str, next: Option<&str>, counts: String| {
        let plural = if revisions == 1 { "" } else { "s" };
        let next = next.map_or(String::new(), |date| format!(", next {date}"));
        format!("{revisions} revision{plural}, in force {in_force}{next}, {counts}")
    };
    let prices = phoebis_ledger::PriceRevisions::shipped();
    let costs = phoebis_ledger::CostRevisions::shipped();
    let tables_at = |now: i64| {
        let price = prices.in_force(now);
        let api_prices = format!(
            "api prices: built in ({}), since ./api_prices.yaml was not found",
            listed(
                prices.len(),
                &price.effective,
                prices.pending(now).map(|next| next.effective.as_str()),
                format!("{} models", price.models.len())
            )
        );
        let cost = costs.in_force(now);
        let subscription_costs = format!(
            "subscription costs: built in ({}), since ./subscription_costs.yaml was not found",
            listed(
                costs.len(),
                &cost.effective,
                costs.pending(now).map(|next| next.effective.as_str()),
                format!(
                    "{} weighted models, {} more by model tier, {} account tiers",
                    cost.weighted_model_count(),
                    cost.by_tier_count(),
                    cost.sized_tier_count()
                )
            )
        );
        [api_prices, subscription_costs]
    };
    assert_eq!(
        lines.get(4..8),
        Some(
            &[
                "proxy: body budget 536870912 bytes, body read 60s, key body reads 128, max request 33554432 bytes, \
                 sign cch placeholder off, upstream gzip auto",
                "profile claude-code-2.1.269-linux-x64",
                "routing: cooldown_fallback 60s, model_rejection 3600s, catalog refresh 21600s, startup fetch 30s",
                "ledger at ./data/ledger.sqlite",
            ][..]
        ),
        "{stdout}"
    );
    let printed = lines.get(8..10);
    let (at_before, at_after) = (tables_at(before), tables_at(after));
    assert!(
        [&at_before, &at_after]
            .iter()
            .any(|tables| printed == Some(&[tables[0].as_str(), tables[1].as_str()][..])),
        "{stdout}\nwant {at_before:?} or {at_after:?}"
    );
    assert_eq!(
        lines.get(10..12),
        Some(
            &[
                "retention 90 days, rollups 400 days",
                "keepalive: 128 sessions, 1024 agents, 256 MiB retained",
            ][..]
        ),
        "{stdout}"
    );
}

const COUNTED_PRICES: &str = "\
revisions:
  - effective: 2026-02-01
    models:
      claude-opus-5-5: {input: 10, cache_write_5m: 12.5, cache_write_1h: 20, cache_read: 1, output: 50}
      claude-haiku-4-5:
        input: 1
        cache_write_5m: 1.25
        cache_write_1h: 2
        cache_read: 0.1
        output: 5
        long_context:
          over: 1000
          input: 5
          cache_write_5m: 6.25
          cache_write_1h: 10
          cache_read: 0.5
          output: 25
";

const COUNTED_COSTS: &str = "\
revisions:
  - effective: 2026-03-01
    unit:
      defined_by: {tier: default_claude_max_20x, window: five_hour, band: out_of_band}
      units_per_window: 100
    capacity:
      default_claude_ai:         {five_hour: 5, seven_day: 50}
      default_claude_enterprise: {five_hour: null, seven_day: null}
      default_claude_max_20x:    {five_hour: 100, seven_day: 1000}
      default_claude_team:       {five_hour: 10, seven_day: null}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 0.5, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-opus-5-5: 2
      claude-opus-5: null
      claude-sonnet-5-5: 1
      claude-sonnet-5: null
      claude-haiku-4-5: 0.5
    model_windows: {}
    overrides:
      claude-sonnet-5: {weight_by_window: {seven_day: 1}}
      claude-haiku-4-5: {weight_by_window: {seven_day: 1}}
    tier_order: [opus, sonnet, haiku]
    model_tier:
      claude-opus-5-5: opus
      claude-opus-5: opus
      claude-sonnet-5-5: sonnet
      claude-sonnet-5: sonnet
      claude-haiku-4-5: haiku
";

/// Two tables written for this count: two price rows dated 2026-02-01, the Haiku 4.5 row priced by prompt length,
/// and a cost model dated 2026-03-01 that weighs Opus 5.5, Sonnet 5.5 and Haiku 4.5, leaves Opus 5 and Sonnet 5 at
/// null, and weighs Sonnet 5 and Haiku 4.5 on the week through overrides. Its four account tiers are Pro and Max 20x
/// sized on both windows, Team on the 5-hour window alone and Enterprise on neither. Each file holds one revision.
/// Counted by hand: two models on the price line (a row priced by prompt length is one model), and on the cost line
/// four weighted models (Opus 5 left out, Sonnet 5 in through its override, Haiku 4.5 once), one more by model tier
/// (Opus 5, the one `model_tier` entry neither a weight nor an override prices, in the model tier Opus 5.5 weighs)
/// and three account tiers (Enterprise left out).
#[test]
fn files_at_both_paths_count_each_model_a_weight_or_an_override_prices_once_and_each_tier_sized_on_a_window(
) {
    let dir = tempfile::tempdir().unwrap();
    let prices = dir.path().join("api_prices.yaml");
    let costs = dir.path().join("subscription_costs.yaml");
    std::fs::write(&prices, COUNTED_PRICES).unwrap();
    std::fs::write(&costs, COUNTED_COSTS).unwrap();
    let config = config_file(
        dir.path(),
        "counted",
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
        "api prices: {} (file, 1 revision, in force 2026-02-01, 2 models)",
        prices.display()
    );
    let subscription_costs = format!(
        "subscription costs: {} (file, 1 revision, in force 2026-03-01, 4 weighted models, 1 more by model tier, \
         3 account tiers)",
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

/// A file whose one entry is dated after now still prices today, since an instant before the list reads its first
/// entry, so the line names that entry in force and no next date: a restart before it changes nothing.
#[test]
fn files_whose_one_entry_is_dated_after_now_name_it_in_force_and_no_next_date() {
    let dir = tempfile::tempdir().unwrap();
    let prices = dir.path().join("api_prices.yaml");
    let costs = dir.path().join("subscription_costs.yaml");
    let ahead_prices = COUNTED_PRICES.replace("effective: 2026-02-01", "effective: 2099-02-01");
    let ahead_costs = COUNTED_COSTS.replace("effective: 2026-03-01", "effective: 2099-03-01");
    assert!(ahead_prices != COUNTED_PRICES && ahead_costs != COUNTED_COSTS);
    std::fs::write(&prices, ahead_prices).unwrap();
    std::fs::write(&costs, ahead_costs).unwrap();
    let config = config_file(
        dir.path(),
        "ahead",
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
        "api prices: {} (file, 1 revision, in force 2099-02-01, 2 models)",
        prices.display()
    );
    let subscription_costs = format!(
        "subscription costs: {} (file, 1 revision, in force 2099-03-01, 4 weighted models, 1 more by model tier, \
         3 account tiers)",
        costs.display()
    );
    assert_eq!(
        lines.get(8..10),
        Some(&[api_prices.as_str(), subscription_costs.as_str()][..]),
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
        "revisions:\n  - effective: 2026-10-04\n    models:\n      m: {input: -1, cache_write_5m: 1, cache_write_1h: 1, \
         cache_read: 1, output: 1}\n",
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
        stderr.contains(
            "pricing table: revision 2026-10-04: model m, cell input must be a finite non-negative number"
        ),
        "{}",
        printed(&out)
    );
}

/// The start refuses a price file whose long-context cell an estimate cannot use, so the check refuses it too, naming
/// the file, the model and the cell under its `long_context.` name.
#[test]
fn a_price_file_with_a_bad_long_context_cell_fails_check_config_naming_the_model_and_the_cell() {
    let dir = tempfile::tempdir().unwrap();
    let prices = dir.path().join("api_prices.yaml");
    std::fs::write(
        &prices,
        "revisions:\n  - effective: 2026-10-04\n    models:\n      m: {input: 1, cache_write_5m: 1, cache_write_1h: 1, \
         cache_read: 1, output: 1, long_context: {over: 1000, input: -1, cache_write_5m: 1, cache_write_1h: 1, \
         cache_read: 1, output: 1}}\n",
    )
    .unwrap();
    let config = config_file(
        dir.path(),
        "bad-long-context-cell",
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
        stderr.contains(
            "pricing table: revision 2026-10-04: model m, cell long_context.input must be a finite non-negative number"
        ),
        "{}",
        printed(&out)
    );
}

/// The start refuses a threshold at zero, which would price every prompt at the long-context cells, so the check
/// refuses it too, naming the file, the model and `long_context.over`.
#[test]
fn a_price_file_with_a_zero_threshold_fails_check_config_naming_long_context_over() {
    let dir = tempfile::tempdir().unwrap();
    let prices = dir.path().join("api_prices.yaml");
    std::fs::write(
        &prices,
        "revisions:\n  - effective: 2026-10-04\n    models:\n      m: {input: 1, cache_write_5m: 1, cache_write_1h: 1, \
         cache_read: 1, output: 1, long_context: {over: 0, input: 1, cache_write_5m: 1, cache_write_1h: 1, \
         cache_read: 1, output: 1}}\n",
    )
    .unwrap();
    let config = config_file(
        dir.path(),
        "zero-threshold",
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
        stderr.contains(
            "pricing table: revision 2026-10-04: model m, cell long_context.over must be a whole number of tokens above \
             zero"
        ),
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
        stderr.lines().any(|line| line.contains(
            "plan table: revision 2026-01-01: capacity.default_claude_max_20x.five_hour"
        )),
        "{}",
        printed(&out)
    );
}

/// Two entries on one date leave the table of that date ambiguous, so the start refuses the price file and the check
/// refuses it too, naming the file and the date.
#[test]
fn a_price_file_with_two_entries_dated_the_same_fails_check_config_naming_the_date() {
    let dir = tempfile::tempdir().unwrap();
    let prices = dir.path().join("api_prices.yaml");
    let entry = "  - effective: 2026-10-04\n    models:\n      m: {input: 1, cache_write_5m: 1, cache_write_1h: 1, \
                 cache_read: 1, output: 1}\n";
    std::fs::write(&prices, format!("revisions:\n{entry}{entry}")).unwrap();
    let config = config_file(
        dir.path(),
        "repeated-price-date",
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
        stderr.contains("pricing table: revisions: two entries dated 2026-10-04"),
        "{}",
        printed(&out)
    );
}

/// A price file with no revision has no table to put in force, so the start refuses it and the check refuses it too,
/// naming the file and asking for an entry.
#[test]
fn a_price_file_with_no_revision_fails_check_config_asking_for_one() {
    let dir = tempfile::tempdir().unwrap();
    let prices = dir.path().join("api_prices.yaml");
    std::fs::write(&prices, "revisions: []\n").unwrap();
    let config = config_file(
        dir.path(),
        "no-price-revision",
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
        stderr.contains("pricing table: revisions: at least one entry"),
        "{}",
        printed(&out)
    );
}

/// The cost model takes the price file's refusal of two entries on one date under its own prefix, naming the file
/// and the date.
#[test]
fn a_cost_model_with_two_entries_dated_the_same_fails_check_config_naming_the_date() {
    let dir = tempfile::tempdir().unwrap();
    let costs = dir.path().join("costs.yaml");
    let text = phoebis_ledger::test_tables::SUBSCRIPTION_COSTS
        .replace("effective: 2026-02-01", "effective: 2026-01-01");
    assert_ne!(text, phoebis_ledger::test_tables::SUBSCRIPTION_COSTS);
    std::fs::write(&costs, text).unwrap();
    let config = config_file(
        dir.path(),
        "repeated-cost-date",
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
        stderr.contains("plan table: revisions: two entries dated 2026-01-01"),
        "{}",
        printed(&out)
    );
}

/// An entry dated after now is held rather than refused, so a change the provider announces can be recorded ahead.
/// The line names the entry in force and the held one as `next`, the date a restart takes it at. The file is the cost
/// fixture's two entries with a third dated 2099 appended. Counted by hand on the second entry: four weighted models
/// (Fable 5.1, Opus 5.5, Sonnet 5.5 and Haiku 4.5), two more by model tier (Opus 5 and Opus 4.8, in the model tier
/// Opus 5.5 weighs, while Mystery 2's model tier weighs nothing) and four account tiers.
#[test]
fn a_cost_model_entry_dated_after_now_is_held_and_named_as_next_beside_the_entry_in_force() {
    let dir = tempfile::tempdir().unwrap();
    let costs = dir.path().join("costs.yaml");
    let fixture = phoebis_ledger::test_tables::SUBSCRIPTION_COSTS;
    let (_, second) = fixture
        .split_once("  - effective: 2026-02-01\n")
        .expect("the cost fixture carries a second entry");
    std::fs::write(
        &costs,
        format!("{fixture}  - effective: 2099-01-01\n{second}"),
    )
    .unwrap();
    let config = config_file(
        dir.path(),
        "held-cost-entry",
        &format!("stats:\n  subscription_costs_path: {}\n", costs.display()),
    );
    let out = check_config(&mut phoebis(), &config);
    assert!(out.status.success(), "{}", printed(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let subscription_costs = format!(
        "subscription costs: {} (file, 3 revisions, in force 2026-02-01, next 2099-01-01, 4 weighted models, \
         2 more by model tier, 4 account tiers)",
        costs.display()
    );
    assert_eq!(
        stdout
            .lines()
            .find(|line| line.starts_with("subscription costs: ")),
        Some(subscription_costs.as_str()),
        "{}",
        printed(&out)
    );
}

/// Each removed key with the block it sat in and its lines.
const REMOVED_KEYS: [(&str, &str); 4] = [
    ("routing", "  catalog_cache_seconds: 60\n"), // removed name
    ("usage", "  cache_ttl_seconds: 60\n"),       // removed name
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
                "registry at {} does not exist yet, and the first start creates it",
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
            "registry at {} has no key canary yet. A sealed row opens under the key, and the next start \
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
            state_cause: StateCause::Operator,
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
            "registry at {} does not exist yet, and the first start creates it",
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
fn a_secret_with_a_non_ascii_character_or_a_surrounding_space_fails_check_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), "shape", "");
    // A byte that is not UTF-8 is a character outside the shape too, never an unset variable.
    let not_utf8 = std::ffi::OsStr::from_bytes(b"sixteen-chars-ok-\xff");
    for secret in [
        std::ffi::OsStr::new("sixteen-chars-ok-é"),
        not_utf8,
        std::ffi::OsStr::new(" sixteen-chars-ok "),
        std::ffi::OsStr::new(" sixteen-chars-ok"),
        std::ffi::OsStr::new("sixteen-chars-ok "),
    ] {
        let out = check_config(phoebis().env("TEST_CHECK_CONFIG_ADMIN", secret), &config);
        assert_eq!(out.status.code(), Some(1), "{secret:?}: {}", printed(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(
                "environment variable TEST_CHECK_CONFIG_ADMIN holds a secret outside the allowed shape \
                 (printable ASCII, no surrounding spaces)"
            ),
            "{secret:?}: {}",
            printed(&out)
        );
    }
    let out = check_config(
        phoebis().env("TEST_CHECK_CONFIG_ADMIN", "sixteen-chars-ok"),
        &config,
    );
    assert!(out.status.success(), "{}", printed(&out));
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
                "warning: {} is readable by group or other (mode 0644) where 0600 is expected",
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
