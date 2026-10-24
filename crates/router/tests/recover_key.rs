//! `phoebis recover-key` from the outside: the binary against a registry the test seals under one key, run under
//! another. It refuses a registry another process holds, a key file that opens the registry's canary, a missing
//! registry and an `--owner` it cannot hand a key to, seals the canary of a registry without one under a key file that
//! opens its sealed rows, generates a missing key file, prints what it discards and asks before it does, and leaves a
//! registry the new key opens with every secret gone and every account marked.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use phoebis_store::keepalive::BodyWrite;
use phoebis_store::{
    AccountKind, AccountRow, AccountState, AttemptOutcome, CanaryRead, KeepaliveAgentRow,
    KeepaliveBatch, KeepaliveSessionRow, NewBundle, Secret, StateCause, StorageKey, Store,
    StoreError,
};

/// The key that sealed the registry, the one an operator lost.
const OLD_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// The replacement an operator wrote before the run.
const NEW_KEY: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

const LOCK_LINE: &str = "the registry is locked, stop the service first";
const MATCH_LINE: &str = "the storage key matches the registry, nothing to recover";
const FOLLOW_UPS: [&str; 3] = [
    "  re-enter the API-key secrets in the console",
    "  pair the OAuth accounts again in the console",
    "  rotate every downstream key",
];

/// The bootstrap file naming `storage.key` and `router.sqlite` in `dir`, neither of which it writes.
fn config_file(dir: &Path) -> PathBuf {
    config_naming(dir, &key_file(dir))
}

/// The bootstrap file naming `key` and `router.sqlite` in `dir`.
fn config_naming(dir: &Path, key: &Path) -> PathBuf {
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:0\n  secret_env: TEST_RECOVER_KEY_ADMIN\n\
         proxy:\n  listen: 127.0.0.1:0\n\
         storage:\n  encryption_key_file: {}\n  path: {}\n",
        key.display(),
        registry(dir).display()
    );
    let path = dir.join("config.yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

fn key_file(dir: &Path) -> PathBuf {
    dir.join("storage.key")
}

fn registry(dir: &Path) -> PathBuf {
    dir.join("router.sqlite")
}

fn key(hex: &str) -> StorageKey {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key");
    std::fs::write(&path, hex).unwrap();
    StorageKey::from_file(&path).unwrap()
}

/// The admin secret's variable is removed rather than set: the recovery reads the file without the unit's
/// environment, as `check-config` and `backup` do.
fn phoebis() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_phoebis"));
    command
        .env_remove("TEST_RECOVER_KEY_ADMIN")
        .env_remove("PHOEBIS_CONFIG");
    command
}

/// `phoebis recover-key <args> --config <config>` with `input` on its stdin.
fn recover_key(config: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = phoebis()
        .arg("recover-key")
        .args(args)
        .arg("--config")
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // A run that refuses before its prompt never reads, so a closed pipe here is no failure of the test.
    let _ = stdin.write_all(input);
    drop(stdin);
    child.wait_with_output().unwrap()
}

fn printed(out: &Output) -> String {
    format!(
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout_lines(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Neither stream carries a key, in its file's hex or otherwise.
fn assert_no_key(out: &Output, keys: &[&str]) {
    for key in keys {
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains(key)
                && !String::from_utf8_lossy(&out.stderr).contains(key),
            "a key was printed: {}",
            printed(out)
        );
    }
}

/// Seeded under a cause other than `operator`, so the cause the recovery writes shows in the read-back.
fn account(id: &str, kind: AccountKind) -> AccountRow {
    AccountRow {
        id: id.into(),
        kind,
        label: id.into(),
        enabled: true,
        identity: None,
        lifecycle_generation: 1,
        incarnation: 1,
        revision: 1,
        state: AccountState::Ready,
        state_cause: match kind {
            AccountKind::ApiKey => StateCause::AuthFailed,
            AccountKind::Oauth => StateCause::Paired,
        },
    }
}

fn bundle() -> NewBundle {
    NewBundle {
        access_token: Secret::new("at-test-not-a-token"),
        refresh_token: Some(Secret::new("rt-test-not-a-token")),
        issued_at: 1_799_996_400_000,
        expires_at: 1_800_000_000_000,
        refresh_expires_at: None,
        scopes: "user:profile user:inference".into(),
    }
}

fn agent(agent_id: &str, body: Option<&[u8]>) -> KeepaliveAgentRow {
    KeepaliveAgentRow {
        key_id: "desktop".into(),
        key_generation: 1,
        session_id: "s-1".into(),
        agent_id: agent_id.into(),
        record_version: 1,
        payload: "{}".into(),
        body: body.map(<[u8]>::to_vec),
        body_write: body.map_or(BodyWrite::Keep, |body| BodyWrite::Seal(body.to_vec())),
        headers: "[]".into(),
        body_size: body.map_or(0, <[u8]>::len),
        dispatched_ms: None,
        fast: false,
        updated_at: 1_800_000_000_000,
    }
}

/// Seals a registry under `key`: two oauth accounts with their credential bundles, one with a refresh in flight and
/// one with a refresh resolved uncertain, an api_key account with its secret, and a keepalive session whose two
/// agents hold one recipe body between them. Answers the accounts as seeded.
fn seed(registry: &Path, key: &StorageKey) -> Vec<AccountRow> {
    let store = Store::open(registry, key).unwrap();
    for (id, kind) in [
        ("claude-a", AccountKind::Oauth),
        ("claude-b", AccountKind::Oauth),
        ("console", AccountKind::ApiKey),
    ] {
        store.insert_account(&account(id, kind)).unwrap();
    }
    for id in ["claude-a", "claude-b"] {
        let generation = store
            .commit_credentials(id, 1, 1, None, None, &bundle())
            .unwrap();
        let attempt = store.open_refresh_attempt(id, 1, 1, generation).unwrap();
        if id == "claude-b" {
            assert!(store
                .resolve_refresh_attempt(
                    attempt,
                    1,
                    AttemptOutcome::Uncertain("the token endpoint did not answer".into())
                )
                .unwrap());
        }
    }
    store
        .set_api_key_secret("console", &Secret::new("sk-ant-api03-not-a-key"))
        .unwrap();
    store
        .save_keepalive(&KeepaliveBatch {
            sessions: vec![KeepaliveSessionRow {
                key_id: "desktop".into(),
                key_generation: 1,
                session_id: "s-1".into(),
                record_version: 1,
                payload: "{}".into(),
                updated_at: 1_800_000_000_000,
            }],
            agents: vec![
                agent("main", Some(b"the user's own prompt")),
                agent("helper", None),
            ],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    store.accounts().unwrap()
}

/// What `seed` sealed still opens under `key`, the canary among it.
fn assert_untouched(registry: &Path, key: &StorageKey) {
    assert_eq!(
        Store::read_canary(registry, key).unwrap(),
        CanaryRead::Matches
    );
    let store = Store::open(registry, key).unwrap();
    assert!(store.credentials("claude-a").unwrap().is_some());
    assert!(store.api_key_secret("console").unwrap().is_some());
    let (_, agents, _) = store.load_keepalive().unwrap();
    assert_eq!(
        agents.iter().filter(|agent| agent.body.is_some()).count(),
        1
    );
}

/// The summary a recovery prints over what `seed` sealed, under `discards` and `marks`, one line per count.
fn seed_summary(discards: &str, marks: &str) -> Vec<String> {
    [
        discards,
        "  OAuth credential bundles: 2",
        "  API-key secrets: 1",
        "  keepalive recipe bodies: 1",
        marks,
        "  oauth accounts disconnected: 2",
        "  api_key accounts unpaired: 1",
        "  open refresh attempts failed: 2",
    ]
    .map(str::to_owned)
    .to_vec()
}

fn summary_before() -> Vec<String> {
    seed_summary(
        "the recovery discards what the old key sealed:",
        "and marks the accounts:",
    )
}

fn summary_after() -> Vec<String> {
    seed_summary(
        "discarded what the old key sealed:",
        "and marked the accounts:",
    )
}

fn wrote_line(key: &Path) -> String {
    format!("wrote a new storage key to {} (mode 0600)", key.display())
}

/// Writes the registry's schema marker past the store, as a bad edit or a newer build would leave it.
fn set_schema_marker(registry: &Path, version: u32) {
    rusqlite::Connection::open(registry)
        .unwrap()
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            [version.to_string()],
        )
        .unwrap();
}

#[test]
fn the_recovery_refuses_a_registry_another_process_holds_with_the_lock_line_and_writes_no_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    let held = Store::open(&registry(dir.path()), &old).unwrap();
    let out = recover_key(&config, &["--yes"], b"");
    drop(held);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert_eq!(stdout_lines(&out), [LOCK_LINE], "{}", printed(&out));
    assert!(
        !key_file(dir.path()).exists(),
        "the lock refuses the run before the key file is generated"
    );
    assert_untouched(&registry(dir.path()), &old);
}

#[test]
fn a_key_file_that_opens_the_registrys_canary_is_refused_as_nothing_to_recover() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    std::fs::write(key_file(dir.path()), OLD_KEY).unwrap();
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert_eq!(stdout_lines(&out), [MATCH_LINE], "{}", printed(&out));
    assert_no_key(&out, &[OLD_KEY]);
    assert_untouched(&registry(dir.path()), &old);
}

/// The one line a recovery prints when it seals the canary of a registry without one, over the `readable` sealed rows
/// the key opens.
fn sealed_canary_line(readable: u64) -> String {
    let rows = if readable == 1 { "row" } else { "rows" };
    format!(
        "the storage key opens {readable} sealed {rows} and the registry has no canary, so a canary was sealed under \
         it and nothing was discarded"
    )
}

// Run without `--yes` on an empty input, which a prompt would read as a refusal, so the one line and exit 0 show the
// run never asked.
#[test]
fn a_key_file_that_opens_the_rows_of_a_registry_without_a_canary_seals_one_without_the_prompt_and_discards_nothing(
) {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    let seeded = seed(&registry(dir.path()), &old);
    Store::open(&registry(dir.path()), &old)
        .unwrap()
        .debug_drop_canary()
        .unwrap();
    std::fs::write(key_file(dir.path()), OLD_KEY).unwrap();
    let out = recover_key(&config, &[], b"");
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert_eq!(
        stdout_lines(&out),
        [sealed_canary_line(4)],
        "two credential bundles, an API-key secret and a recipe body open: {}",
        printed(&out)
    );
    assert_no_key(&out, &[OLD_KEY]);
    assert_untouched(&registry(dir.path()), &old);
    let store = Store::open(&registry(dir.path()), &old).unwrap();
    assert!(
        store.credentials("claude-b").unwrap().is_some(),
        "the second bundle is kept as well"
    );
    assert_eq!(store.accounts().unwrap(), seeded, "no account was marked");
    // Released before the second run, which takes the registry's lock.
    drop(store);
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert_eq!(
        stdout_lines(&out),
        [MATCH_LINE],
        "a key that opens the canary is nothing to recover: {}",
        printed(&out)
    );
}

#[test]
fn a_missing_registry_is_refused_as_nothing_to_recover_and_nothing_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert_eq!(
        stdout_lines(&out),
        [format!(
            "no registry at {}, nothing to recover",
            registry(dir.path()).display()
        )],
        "{}",
        printed(&out)
    );
    assert!(!registry(dir.path()).exists());
    assert!(!key_file(dir.path()).exists());
    assert!(!dir.path().join("lock").exists());
}

#[test]
fn an_owner_the_run_cannot_hand_a_generated_key_to_is_refused_before_anything_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    // Off root the run needs root to hand the file over, and as root the user does not exist: either refuses.
    let out = recover_key(
        &config,
        &["--yes", "--owner", "phoebis-recover-key-no-such-user"],
        b"",
    );
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let lines = stdout_lines(&out);
    assert!(
        lines.len() == 1 && lines[0].starts_with("--owner "),
        "{}",
        printed(&out)
    );
    assert!(!key_file(dir.path()).exists());
    assert_untouched(&registry(dir.path()), &old);
}

#[test]
fn a_missing_key_file_is_generated_at_0600_and_the_recovery_discards_every_sealed_row_under_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    let seeded = seed(&registry(dir.path()), &old);
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));

    let written = std::fs::read_to_string(key_file(dir.path())).unwrap();
    let hex = written
        .strip_suffix('\n')
        .expect("the key is written as one line");
    assert!(
        hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "{written:?}"
    );
    assert_eq!(
        std::fs::metadata(key_file(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_no_key(&out, &[hex, OLD_KEY]);
    let lines = stdout_lines(&out);
    assert!(
        lines.contains(&format!(
            "wrote a new storage key to {} (mode 0600)",
            key_file(dir.path()).display()
        )),
        "{}",
        printed(&out)
    );
    for counted in [
        "  OAuth credential bundles: 2",
        "  API-key secrets: 1",
        "  keepalive recipe bodies: 1",
        "  oauth accounts disconnected: 2",
        "  api_key accounts unpaired: 1",
        "  open refresh attempts failed: 2",
    ] {
        assert_eq!(
            lines.iter().filter(|line| *line == counted).count(),
            2,
            "{counted:?} before the run and after it: {}",
            printed(&out)
        );
    }
    assert!(
        lines.ends_with(&FOLLOW_UPS.map(str::to_owned)),
        "{}",
        printed(&out)
    );

    let new = StorageKey::from_file(&key_file(dir.path())).unwrap();
    assert_eq!(
        Store::read_canary(&registry(dir.path()), &new).unwrap(),
        CanaryRead::Matches,
        "the run sealed a canary under the new key"
    );
    assert!(matches!(
        Store::open(&registry(dir.path()), &old),
        Err(StoreError::KeyMismatch { .. })
    ));
    let store = Store::open(&registry(dir.path()), &new).unwrap();
    for id in ["claude-a", "claude-b"] {
        assert!(store.credentials(id).unwrap().is_none(), "{id}");
        let attempt = store.last_refresh_attempt(id, 1).unwrap().unwrap();
        assert!(attempt.resolved_at.is_some(), "{id}");
        assert_eq!(attempt.outcome.as_deref(), Some("failed"), "{id}");
        assert_eq!(attempt.detail.as_deref(), Some("disconnected"), "{id}");
    }
    assert!(store.api_key_secret("console").unwrap().is_none());
    let (sessions, agents, _) = store.load_keepalive().unwrap();
    assert_eq!(sessions.len(), 1, "the session row stays");
    assert_eq!(agents.len(), 2, "the agent rows stay");
    assert!(agents.iter().all(|agent| agent.body.is_none()));
    for before in seeded {
        assert_ne!(before.state_cause, StateCause::Operator, "{}", before.id);
        let after = store.account(&before.id).unwrap().unwrap();
        let expected = match before.kind {
            AccountKind::Oauth => AccountState::Disconnected,
            AccountKind::ApiKey => AccountState::Unpaired,
        };
        assert_eq!(after.state, expected, "{}", before.id);
        assert_eq!(after.state_cause, StateCause::Operator, "{}", before.id);
        assert_eq!(
            after.lifecycle_generation,
            before.lifecycle_generation + 1,
            "{}",
            before.id
        );
        assert_eq!(after.revision, before.revision + 1, "{}", before.id);
    }
}

#[test]
fn a_preparation_that_fails_says_nothing_was_discarded_and_leaves_the_sealed_rows_and_the_key_file()
{
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    std::fs::write(key_file(dir.path()), NEW_KEY).unwrap();
    // The marker one step behind a registry that ran every step: the open admits it, and the last step drops a
    // column that is already gone, which the preparation's migration refuses.
    set_schema_marker(&registry(dir.path()), phoebis_store::SCHEMA_VERSION - 1);
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let lines = stdout_lines(&out);
    let summary = summary_before();
    assert_eq!(lines.len(), summary.len() + 1, "{}", printed(&out));
    assert_eq!(lines[..summary.len()], summary[..], "{}", printed(&out));
    assert!(
        lines[summary.len()].starts_with(
            "the registry's schema could not be prepared for the recovery, so nothing was discarded: "
        ),
        "{}",
        printed(&out)
    );
    assert_no_key(&out, &[OLD_KEY, NEW_KEY]);
    assert_eq!(
        std::fs::read_to_string(key_file(dir.path())).unwrap(),
        NEW_KEY,
        "the operator's key file stays"
    );
    set_schema_marker(&registry(dir.path()), phoebis_store::SCHEMA_VERSION);
    assert_untouched(&registry(dir.path()), &old);
}

#[test]
fn a_registry_from_a_newer_build_is_refused_at_the_open_before_the_key_file_and_keeps_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let lock = dir.path().join("lock");
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    std::fs::remove_file(&lock).unwrap();
    set_schema_marker(&registry(dir.path()), phoebis_store::SCHEMA_VERSION + 1);
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let lines = stdout_lines(&out);
    assert!(
        lines.len() == 1
            && lines[0].starts_with(&format!(
                "opening the registry at {}: ",
                registry(dir.path()).display()
            )),
        "{}",
        printed(&out)
    );
    assert_no_key(&out, &[OLD_KEY]);
    assert!(!key_file(dir.path()).exists(), "{}", printed(&out));
    assert!(
        lock.exists(),
        "the refused open keeps the lock file it created: {}",
        printed(&out)
    );
    set_schema_marker(&registry(dir.path()), phoebis_store::SCHEMA_VERSION);
    assert_untouched(&registry(dir.path()), &old);
}

#[test]
fn the_prompt_takes_yes_alone_and_anything_else_exits_2_with_the_registry_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    std::fs::write(key_file(dir.path()), NEW_KEY).unwrap();
    for answer in [&b"y\n"[..], &b"no\n"[..], &b"YES\n"[..], &b""[..]] {
        let out = recover_key(&config, &[], answer);
        assert_eq!(out.status.code(), Some(2), "{answer:?}: {}", printed(&out));
        let lines = stdout_lines(&out);
        assert!(
            lines.contains(&"  OAuth credential bundles: 2".to_owned()),
            "the summary comes before the prompt: {}",
            printed(&out)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("type yes to discard them: "),
            "{}",
            printed(&out)
        );
        // A piped answer is not echoed, so the line can share the prompt's line.
        assert!(
            lines
                .last()
                .is_some_and(|line| line.ends_with("the registry was not changed")),
            "{}",
            printed(&out)
        );
        assert_no_key(&out, &[OLD_KEY, NEW_KEY]);
        assert_untouched(&registry(dir.path()), &old);
    }
    let out = recover_key(&config, &[], b"yes\n");
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert!(
        stdout_lines(&out).ends_with(&FOLLOW_UPS.map(str::to_owned)),
        "{}",
        printed(&out)
    );
    assert_no_key(&out, &[OLD_KEY, NEW_KEY]);
    assert_eq!(
        Store::read_canary(&registry(dir.path()), &key(NEW_KEY)).unwrap(),
        CanaryRead::Matches
    );
}

#[test]
fn a_declined_run_on_a_registry_without_a_key_file_writes_no_key_and_keeps_the_lock_it_created() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let lock = dir.path().join("lock");
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    // A registry restored onto a new host comes without the lock file the service's own open leaves beside it.
    std::fs::remove_file(&lock).unwrap();
    let out = recover_key(&config, &[], b"no\n");
    assert_eq!(out.status.code(), Some(2), "{}", printed(&out));
    let mut expected = summary_before();
    // A piped answer is not echoed, so the line shares the prompt's line.
    expected.push("type yes to discard them: the registry was not changed".to_owned());
    assert_eq!(stdout_lines(&out), expected, "{}", printed(&out));
    assert!(!key_file(dir.path()).exists(), "{}", printed(&out));
    assert!(lock.exists(), "{}", printed(&out));
    assert_no_key(&out, &[OLD_KEY]);
    // The key the run generated stayed in memory, so its hex is unknown here: stdout is pinned whole above, and
    // stderr carries nothing.
    assert!(out.stderr.is_empty(), "{}", printed(&out));
    assert_untouched(&registry(dir.path()), &old);
}

#[test]
fn a_start_that_opened_the_lock_file_before_a_declined_run_exited_still_shuts_out_the_next_start() {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let lock = dir.path().join("lock");
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    // A registry restored onto a new host comes without the lock file the service's own open leaves beside it.
    std::fs::remove_file(&lock).unwrap();
    let mut child = phoebis()
        .arg("recover-key")
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut shown = Vec::new();
    let mut chunk = [0u8; 4096];
    while !String::from_utf8_lossy(&shown).contains("type yes to discard them: ") {
        let read = stdout.read(&mut chunk).unwrap();
        assert!(
            read > 0,
            "the run ended before its prompt: {}",
            String::from_utf8_lossy(&shown)
        );
        shown.extend_from_slice(&chunk[..read]);
    }
    // A start that opened the lock file while the run held it, and takes the lock once the run has exited.
    let first = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"no\n").unwrap();
    drop(stdin);
    stdout.read_to_end(&mut shown).unwrap();
    assert_eq!(
        child.wait().unwrap().code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&shown)
    );
    assert_eq!(
        unsafe { libc::flock(first.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "the first start takes the lock the run released"
    );
    assert!(
        matches!(
            Store::open(&registry(dir.path()), &old),
            Err(StoreError::Locked)
        ),
        "a second start opens the file the first one holds and is refused"
    );
    assert!(lock.exists(), "{}", String::from_utf8_lossy(&shown));
}

#[test]
fn a_run_under_yes_writes_the_key_file_after_the_summary_and_keeps_it_with_the_lock_it_created() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let lock = dir.path().join("lock");
    seed(&registry(dir.path()), &key(OLD_KEY));
    std::fs::remove_file(&lock).unwrap();
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    let mut expected = summary_before();
    expected.push(wrote_line(&key_file(dir.path())));
    expected.extend(summary_after());
    expected.push(format!(
        "sealed a new canary under the key at {}",
        key_file(dir.path()).display()
    ));
    expected.push("start the service, then:".to_owned());
    expected.extend(FOLLOW_UPS.map(str::to_owned));
    assert_eq!(stdout_lines(&out), expected, "{}", printed(&out));
    let written = std::fs::read_to_string(key_file(dir.path())).unwrap();
    assert_no_key(&out, &[written.trim_end(), OLD_KEY]);
    assert!(
        lock.exists(),
        "the service's next start opens the lock: {}",
        printed(&out)
    );
    assert_eq!(
        Store::read_canary(
            &registry(dir.path()),
            &StorageKey::from_file(&key_file(dir.path())).unwrap()
        )
        .unwrap(),
        CanaryRead::Matches
    );
}

#[test]
fn a_discard_that_does_not_land_removes_the_key_file_and_its_directory_and_keeps_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    let generated = keys.join("storage.key");
    let lock = dir.path().join("lock");
    let config = config_naming(dir.path(), &generated);
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    std::fs::remove_file(&lock).unwrap();
    // SQLite locks the whole database, so a write transaction held from a second connection outlasts the discard's
    // busy timeout, while the preparation of a registry at the current version writes nothing and lands.
    let writer = rusqlite::Connection::open(registry(dir.path())).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let out = recover_key(&config, &["--yes"], b"");
    drop(writer);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let lines = stdout_lines(&out);
    let mut expected = summary_before();
    expected.push(wrote_line(&generated));
    assert_eq!(lines.len(), expected.len() + 2, "{}", printed(&out));
    assert_eq!(lines[..expected.len()], expected[..], "{}", printed(&out));
    assert!(
        lines[expected.len()].starts_with(&format!(
            "the recovery did not land, so the registry at {} is unchanged: ",
            registry(dir.path()).display()
        )),
        "{}",
        printed(&out)
    );
    assert_eq!(
        lines[expected.len() + 1],
        format!(
            "removed the key file {} this run created",
            generated.display()
        ),
        "{}",
        printed(&out)
    );
    assert!(!generated.exists(), "{}", printed(&out));
    assert!(!keys.exists(), "{}", printed(&out));
    assert!(lock.exists(), "{}", printed(&out));
    assert_no_key(&out, &[OLD_KEY]);
    // The key file the run wrote is gone, so its hex is unknown here: stdout is pinned above up to the error, and
    // stderr carries nothing.
    assert!(out.stderr.is_empty(), "{}", printed(&out));
    assert_untouched(&registry(dir.path()), &old);
}

/// The uid and the primary gid of `nobody`.
fn nobody() -> (u32, u32) {
    let entry = unsafe { libc::getpwnam(c"nobody".as_ptr()) };
    assert!(!entry.is_null(), "this host has no user nobody");
    unsafe { ((*entry).pw_uid, (*entry).pw_gid) }
}

#[test]
#[ignore = "needs uid 0, so the owner runs it"]
fn a_root_run_hands_the_owner_the_key_file_the_directory_and_the_lock_it_created() {
    use std::os::unix::fs::MetadataExt;

    assert_eq!(unsafe { libc::geteuid() }, 0, "run this test as root");
    let (uid, gid) = nobody();
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    let generated = keys.join("storage.key");
    let lock = dir.path().join("lock");
    let config = config_naming(dir.path(), &generated);
    seed(&registry(dir.path()), &key(OLD_KEY));
    // A registry restored onto a new host comes without the lock file the service's own open leaves beside it.
    std::fs::remove_file(&lock).unwrap();
    let out = recover_key(&config, &["--yes", "--owner", "nobody"], b"");
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    for path in [&keys, &generated, &lock] {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        assert_eq!(
            (metadata.uid(), metadata.gid()),
            (uid, gid),
            "{}",
            path.display()
        );
    }
    assert_eq!(
        std::fs::metadata(registry(dir.path())).unwrap().uid(),
        0,
        "the registry keeps the owner it had"
    );
}

#[test]
#[ignore = "needs uid 0, so the owner runs it"]
fn a_root_run_the_open_refuses_leaves_the_owner_the_lock_file_it_created() {
    use std::os::unix::fs::MetadataExt;

    assert_eq!(unsafe { libc::geteuid() }, 0, "run this test as root");
    let (uid, gid) = nobody();
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let lock = dir.path().join("lock");
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    // A registry restored onto a new host comes without the lock file the service's own open leaves beside it.
    std::fs::remove_file(&lock).unwrap();
    set_schema_marker(&registry(dir.path()), phoebis_store::SCHEMA_VERSION + 1);
    let out = recover_key(&config, &["--yes", "--owner", "nobody"], b"");
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let metadata = std::fs::symlink_metadata(&lock).unwrap();
    assert_eq!(
        (metadata.uid(), metadata.gid()),
        (uid, gid),
        "the service's user can open the lock the refused run kept: {}",
        printed(&out)
    );
    set_schema_marker(&registry(dir.path()), phoebis_store::SCHEMA_VERSION);
    assert_untouched(&registry(dir.path()), &old);
}

/// The one line a recovery refuses a corrupt canary with while `readable` sealed rows still open under the key.
fn corrupt_canary_line(readable: u64) -> String {
    let rows = if readable == 1 { "row" } else { "rows" };
    format!(
        "the storage key opens {readable} sealed {rows} but not the corrupt canary, so nothing was discarded: \
         restore the canary from a backup"
    )
}

/// The registry's canary as `sqlite3` would leave it after a bad edit: still base64, opened by no key. Answers the
/// value it replaced, the copy a backup holds.
fn corrupt_canary(registry: &Path) -> String {
    let conn = rusqlite::Connection::open(registry).unwrap();
    let value: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'storage_key_canary'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'storage_key_canary'",
        ["A".repeat(value.len())],
    )
    .unwrap();
    value
}

fn restore_canary(registry: &Path, value: &str) {
    rusqlite::Connection::open(registry)
        .unwrap()
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'storage_key_canary'",
            [value],
        )
        .unwrap();
}

#[test]
fn a_corrupt_canary_over_rows_the_key_still_opens_is_refused_with_their_count_and_discards_nothing()
{
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    let canary = corrupt_canary(&registry(dir.path()));
    std::fs::write(key_file(dir.path()), OLD_KEY).unwrap();
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert_eq!(
        stdout_lines(&out),
        [corrupt_canary_line(4)],
        "two credential bundles, an API-key secret and a recipe body open: {}",
        printed(&out)
    );
    assert_no_key(&out, &[OLD_KEY]);
    restore_canary(&registry(dir.path()), &canary);
    assert_untouched(&registry(dir.path()), &old);
}

#[test]
fn a_registry_without_a_canary_whose_one_readable_row_follows_32_unreadable_ones_gets_a_canary_the_next_start_opens(
) {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    let store = Store::open(&registry(dir.path()), &old).unwrap();
    for index in 0..33 {
        let id = format!("claude-{index:02}");
        store
            .insert_account(&account(&id, AccountKind::Oauth))
            .unwrap();
        store
            .commit_credentials(&id, 1, 1, None, None, &bundle())
            .unwrap();
    }
    store.debug_drop_canary().unwrap();
    drop(store);
    rusqlite::Connection::open(registry(dir.path()))
        .unwrap()
        .execute(
            "UPDATE credentials SET sealed = zeroblob(length(sealed)) \
             WHERE rowid IN (SELECT rowid FROM credentials ORDER BY rowid LIMIT 32)",
            [],
        )
        .unwrap();
    assert!(
        matches!(
            Store::open(&registry(dir.path()), &old),
            Err(StoreError::KeyMismatch { .. })
        ),
        "the bounded proof refuses the right key at the start"
    );
    std::fs::write(key_file(dir.path()), OLD_KEY).unwrap();
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert_eq!(
        stdout_lines(&out),
        [sealed_canary_line(1)],
        "{}",
        printed(&out)
    );
    let left: i64 = rusqlite::Connection::open(registry(dir.path()))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM credentials", [], |row| row.get(0))
        .unwrap();
    assert_eq!(left, 33, "nothing was discarded");
    assert_eq!(
        Store::read_canary(&registry(dir.path()), &old).unwrap(),
        CanaryRead::Matches
    );
    assert!(
        Store::open(&registry(dir.path()), &old).is_ok(),
        "the start the bounded proof refused passes through the canary"
    );
}

#[test]
fn a_registry_whose_only_sealed_rows_are_recipe_bodies_is_judged_by_them() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    let store = Store::open(&registry(dir.path()), &old).unwrap();
    store
        .save_keepalive(&KeepaliveBatch {
            sessions: vec![KeepaliveSessionRow {
                key_id: "desktop".into(),
                key_generation: 1,
                session_id: "s-1".into(),
                record_version: 1,
                payload: "{}".into(),
                updated_at: 1_800_000_000_000,
            }],
            agents: vec![agent("main", Some(b"the user's own prompt"))],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    store.debug_drop_canary().unwrap();
    drop(store);
    std::fs::write(key_file(dir.path()), OLD_KEY).unwrap();
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert_eq!(
        stdout_lines(&out),
        [sealed_canary_line(1)],
        "the body opens under the key: {}",
        printed(&out)
    );
    std::fs::write(key_file(dir.path()), NEW_KEY).unwrap();
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert!(
        stdout_lines(&out).contains(&"  keepalive recipe bodies: 1".to_owned()),
        "a key the body does not open discards it: {}",
        printed(&out)
    );
    let store = Store::open(&registry(dir.path()), &key(NEW_KEY)).unwrap();
    let (_, agents, _) = store.load_keepalive().unwrap();
    assert!(agents.iter().all(|agent| agent.body.is_none()));
}

const UNDO_V62: &str = "";

/// The reverse of the step before it, the plan-ended column gone.
const UNDO_V61: &str = "ALTER TABLE accounts DROP COLUMN plan_ended_at; \
     UPDATE meta SET value = '60' WHERE key = 'schema_version';";

/// The reverse of the step before it, the flag's column back under its old name. The marker last, since it restores an
/// old name, and the trailing `\n` ends the marker's comment.
const UNDO_V60: &str = "ALTER TABLE accounts RENAME COLUMN canceled TO cancelled; \
     UPDATE meta SET value = '59' WHERE key = 'schema_version'; -- rollback\n";

/// The reverse of the step before it, the pin's shed reason column gone.
const UNDO_V59: &str =
    "ALTER TABLE pins DROP COLUMN shed_reason; UPDATE meta SET value = '58' WHERE key = 'schema_version';";

/// The reverse of the step before it, the pin's five unmetered burn columns and its `last_classifier` gone.
const UNDO_V58: &str = "ALTER TABLE pins DROP COLUMN burn_unmetered_cache_read; \
     ALTER TABLE pins DROP COLUMN burn_unmetered_input; \
     ALTER TABLE pins DROP COLUMN burn_unmetered_output; \
     ALTER TABLE pins DROP COLUMN burn_unmetered_write_1h; \
     ALTER TABLE pins DROP COLUMN burn_unmetered_write_5m; \
     ALTER TABLE pins DROP COLUMN last_classifier; \
     UPDATE meta SET value = '57' WHERE key = 'schema_version';";

/// The reverse of the step before it, the guardrail tables gone.
const UNDO_V57: &str = "DROP TABLE IF EXISTS guardrail_attachments; \
     DROP TABLE IF EXISTS guardrail_tombstones; \
     DROP TABLE IF EXISTS guardrails; \
     UPDATE meta SET value = '56' WHERE key = 'schema_version';";

/// The reverse of the step before it, the pin's entrypoint column gone.
const UNDO_V56: &str =
    "ALTER TABLE pins DROP COLUMN entrypoint; UPDATE meta SET value = '55' WHERE key = 'schema_version';";

/// The reverse of the step before it, the slow-down's three columns and the return-to-plan mode and dwell gone.
const UNDO_V55: &str = "ALTER TABLE pools DROP COLUMN slow_down; \
     ALTER TABLE pools DROP COLUMN slow_down_stretch_seconds; \
     ALTER TABLE pools DROP COLUMN slow_down_hold_seconds; \
     ALTER TABLE pools DROP COLUMN return_to_plan; \
     ALTER TABLE pools DROP COLUMN return_to_plan_dwell_seconds; \
     UPDATE meta SET value = '54' WHERE key = 'schema_version';";

#[test]
fn a_run_that_stops_short_of_the_discard_leaves_the_registry_at_the_version_it_found() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    rusqlite::Connection::open(registry(dir.path()))
        .unwrap()
        .execute_batch(&format!(
            "{UNDO_V62} {UNDO_V61} {UNDO_V60} {UNDO_V59} {UNDO_V58} {UNDO_V57} {UNDO_V56} {UNDO_V55}"
        ))
        .unwrap();
    std::fs::write(key_file(dir.path()), OLD_KEY).unwrap();
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert_eq!(stdout_lines(&out), [MATCH_LINE], "{}", printed(&out));
    assert_eq!(
        Store::read_schema_version(&registry(dir.path())).unwrap(),
        Some(54),
        "a matching key migrates nothing"
    );
    std::fs::write(key_file(dir.path()), NEW_KEY).unwrap();
    let out = recover_key(&config, &[], b"no\n");
    assert_eq!(out.status.code(), Some(2), "{}", printed(&out));
    assert_eq!(
        Store::read_schema_version(&registry(dir.path())).unwrap(),
        Some(54),
        "a declined run migrates nothing, so the build that wrote the registry still opens it"
    );
    let out = recover_key(&config, &[], b"yes\n");
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert_eq!(
        Store::read_schema_version(&registry(dir.path())).unwrap(),
        Some(phoebis_store::SCHEMA_VERSION),
        "the recovery that proceeds prepares the schema"
    );
}
