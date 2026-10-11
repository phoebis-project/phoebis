//! `phoebis recover-key` from the outside: the binary against a registry the test seals under one key, run under
//! another. It refuses a registry another process holds, a key file that opens the registry, a missing registry and an
//! `--owner` it cannot hand a key to, generates a missing key file, prints what it discards and asks before it does,
//! and leaves a registry the new key opens with every secret gone and every account marked.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:0\n  secret_env: TEST_RECOVER_KEY_ADMIN\n\
         proxy:\n  listen: 127.0.0.1:0\n\
         storage:\n  encryption_key_file: {}\n  path: {}\n",
        key_file(dir).display(),
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
        state_cause: StateCause::Operator,
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

#[test]
fn a_key_file_that_opens_a_sealed_row_of_a_registry_without_a_canary_is_refused_as_nothing_to_recover(
) {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path());
    let old = key(OLD_KEY);
    seed(&registry(dir.path()), &old);
    Store::open(&registry(dir.path()), &old)
        .unwrap()
        .debug_drop_canary()
        .unwrap();
    std::fs::write(key_file(dir.path()), OLD_KEY).unwrap();
    let out = recover_key(&config, &["--yes"], b"");
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert_eq!(stdout_lines(&out), [MATCH_LINE], "{}", printed(&out));
    assert_eq!(
        Store::read_canary(&registry(dir.path()), &old).unwrap(),
        CanaryRead::SealedRowMatches,
        "the refused run sealed no canary"
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
