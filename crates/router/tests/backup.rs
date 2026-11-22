//! `phoebis backup` copies both databases into a private directory with a manifest that names the key file, while the
//! router's writes go on, and the copies open under the key. It refuses a destination that is not an empty directory
//! or that someone besides the run's user could write into, and a database that does not exist, and a copy that
//! fails leaves nothing behind. A lock on a source is waited out within the bound and fails the run past it, with
//! nothing written.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use phoebis_ledger::{EventRecord, LedgerDb};
use phoebis_router::backup::{self, BackupArgs, RunError};
use phoebis_store::{CanaryRead, PoolRow, StorageKey, Store};

/// The wall clock, for a case that reads no stamp.
fn wall() -> phoebis_store::Clock {
    Arc::new(|| jiff::Timestamp::now().as_millisecond())
}

const HEX_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A data directory as a first start leaves it: the key file, a registry sealed under it with the pool `kept`, and a
/// ledger with one event. The databases stay open for the caller.
struct Data {
    key: StorageKey,
    key_file: PathBuf,
    ledger: PathBuf,
    registry: PathBuf,
}

fn first_start(dir: &Path) -> (Data, Store, LedgerDb) {
    let data = dir.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let key_file = data.join("storage.key");
    std::fs::write(&key_file, format!("{HEX_KEY}\n")).unwrap();
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let key = StorageKey::from_file(&key_file).unwrap();
    let registry = data.join("router.sqlite");
    let ledger = data.join("ledger.sqlite");
    let store = Store::open(&registry, &key, wall()).unwrap();
    store
        .insert_pool(&PoolRow::for_tests("kept"), "test")
        .unwrap();
    let ledger_db = LedgerDb::open(&ledger).unwrap();
    ledger_db.insert_event(&event(0)).unwrap();
    (
        Data {
            key,
            key_file,
            ledger,
            registry,
        },
        store,
        ledger_db,
    )
}

fn event(at: i64) -> EventRecord {
    EventRecord {
        at,
        kind: "backup_test".into(),
        ..Default::default()
    }
}

fn config_file(dir: &Path, data: &Data, ledger: &Path) -> PathBuf {
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:0\n  secret_env: TEST_BACKUP_ADMIN\n\
         proxy:\n  listen: 127.0.0.1:0\n\
         stats:\n  ledger_path: {}\n\
         storage:\n  encryption_key_file: {}\n  path: {}\n",
        ledger.display(),
        data.key_file.display(),
        data.registry.display()
    );
    let path = dir.join("config.yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

fn backup_to(config: &Path, to: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_phoebis"))
        .env_remove("PHOEBIS_CONFIG")
        .env_remove("TEST_BACKUP_ADMIN")
        .arg("backup")
        .arg("--config")
        .arg(config)
        .arg("--to")
        .arg(to)
        .args(extra)
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

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Every entry under `dir`, sorted, so a run can be shown to have left the tree as it found it.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn sha256_hex(path: &Path) -> String {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let mut hasher = boring::sha::Sha256::new();
    hasher.update(&bytes);
    hasher
        .finish()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn pools(store: &Store) -> Vec<String> {
    store
        .pools()
        .unwrap()
        .into_iter()
        .map(|pool| pool.id)
        .collect()
}

fn integrity_check(path: &Path) -> String {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap()
}

/// A second connection holding the registry under `PRAGMA locking_mode = EXCLUSIVE` after one read: on a WAL file the
/// connection keeps the index in its own heap and takes the database file's exclusive lock, which answers every other
/// connection `SQLITE_BUSY`. The lock is held until `release` answers; the holder signals once it holds it.
fn hold_exclusive(
    path: &Path,
    release: impl FnOnce() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let path = path.to_path_buf();
    let (locked, held) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "locking_mode", "EXCLUSIVE")
            .unwrap();
        let _: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .unwrap();
        locked.send(()).unwrap();
        release();
        drop(conn);
    });
    held.recv().unwrap();
    holder
}

#[test]
fn a_backup_taken_while_both_databases_are_written_opens_under_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let (data, store, ledger_db) = first_start(dir.path());
    let config = config_file(dir.path(), &data, &data.ledger);
    let stop = Arc::new(AtomicBool::new(false));
    let (first, wrote) = mpsc::channel();
    // The writer goes through the paths the router's own writers take: the store's pool writes and the ledger's
    // `write` turn, which `insert_event` runs as the writer task does.
    let writer = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut rounds = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let id = format!("churn-{rounds}");
                store.insert_pool(&PoolRow::for_tests(&id), "test").unwrap();
                ledger_db.insert_event(&event(rounds as i64 + 1)).unwrap();
                store.delete_pool(&id, "test").unwrap();
                rounds += 1;
                if rounds == 1 {
                    first.send(()).unwrap();
                }
            }
            rounds
        })
    };
    wrote.recv().unwrap();
    let out = dir.path().join("out");
    let target = out.join("nested");

    let run = backup_to(&config, &target, &[]);

    stop.store(true, Ordering::SeqCst);
    let rounds = writer.join().unwrap();
    assert_eq!(run.status.code(), Some(0), "{}", printed(&run));
    assert!(rounds > 0);

    assert_eq!(mode(&out), 0o700, "a parent the run created");
    assert_eq!(mode(&target), 0o700);
    assert_eq!(
        entries(&target),
        ["ledger.sqlite", "manifest.json", "router.sqlite"]
    );
    for name in ["ledger.sqlite", "manifest.json", "router.sqlite"] {
        assert_eq!(mode(&target.join(name)), 0o600, "{name}");
    }
    let registry_copy = target.join("router.sqlite");
    let ledger_copy = target.join("ledger.sqlite");
    assert_eq!(
        Store::read_canary(&registry_copy, &data.key).unwrap(),
        CanaryRead::Matches
    );
    assert_eq!(
        LedgerDb::read_schema_version(&ledger_copy).unwrap(),
        LedgerDb::read_schema_version(&data.ledger).unwrap()
    );

    let manifest_text = std::fs::read_to_string(target.join("manifest.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
    assert_eq!(manifest["format"], 1);
    assert_eq!(
        manifest["key_file"],
        data.key_file.display().to_string(),
        "{manifest_text}"
    );
    assert_eq!(
        manifest["registry"]["source"],
        data.registry.display().to_string()
    );
    assert_eq!(
        manifest["ledger"]["source"],
        data.ledger.display().to_string()
    );
    assert_eq!(manifest["registry"]["sha256"], sha256_hex(&registry_copy));
    assert_eq!(manifest["ledger"]["sha256"], sha256_hex(&ledger_copy));
    assert_eq!(
        manifest["registry"]["bytes"],
        std::fs::metadata(&registry_copy).unwrap().len()
    );
    assert_eq!(
        manifest["registry"]["schema_version"],
        Store::read_schema_version(&data.registry).unwrap().unwrap()
    );
    assert_eq!(
        manifest["ledger"]["schema_version"],
        LedgerDb::read_schema_version(&data.ledger)
            .unwrap()
            .unwrap()
    );
    let stdout = String::from_utf8(run.stdout.clone()).unwrap();
    let stderr = String::from_utf8(run.stderr.clone()).unwrap();
    for (name, text) in [
        ("manifest", &manifest_text),
        ("stdout", &stdout),
        ("stderr", &stderr),
    ] {
        assert!(!text.contains(HEX_KEY), "the key's hex in {name}");
    }
    assert!(stdout.is_empty(), "{}", printed(&run));
    let last = stderr.lines().last().unwrap();
    assert!(
        last.contains(&data.key_file.display().to_string())
            && last.contains("is not in this backup"),
        "{}",
        printed(&run)
    );

    let restore = dir.path().join("restore");
    std::fs::create_dir(&restore).unwrap();
    for name in ["ledger.sqlite", "router.sqlite"] {
        std::fs::copy(target.join(name), restore.join(name)).unwrap();
    }
    let restored = Store::open(&restore.join("router.sqlite"), &data.key, wall()).unwrap();
    // Each round inserts its pool and deletes it in two transactions, so the instant the copy saw holds at most one.
    let restored_pools = pools(&restored);
    let churn: Vec<&String> = restored_pools
        .iter()
        .filter(|id| id.as_str() != "kept")
        .collect();
    assert!(
        restored_pools.iter().any(|id| id == "kept"),
        "{restored_pools:?}"
    );
    assert!(
        churn.len() <= 1 && churn.iter().all(|id| id.starts_with("churn-")),
        "{restored_pools:?}"
    );
    drop(restored);
    drop(LedgerDb::open(&restore.join("ledger.sqlite")).unwrap());
    for name in ["ledger.sqlite", "router.sqlite"] {
        assert_eq!(integrity_check(&restore.join(name)), "ok", "{name}");
    }
}

#[test]
fn a_destination_that_holds_a_file_is_refused_and_left_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let (data, store, ledger_db) = first_start(dir.path());
    drop((store, ledger_db));
    let config = config_file(dir.path(), &data, &data.ledger);
    let target = dir.path().join("out");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("notes.txt"), "mine").unwrap();

    let run = backup_to(&config, &target, &[]);

    assert_eq!(run.status.code(), Some(1), "{}", printed(&run));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains(&format!("{} is not an empty directory", target.display())),
        "{}",
        printed(&run)
    );
    assert_eq!(entries(&target), ["notes.txt"]);
    assert_eq!(
        std::fs::read_to_string(target.join("notes.txt")).unwrap(),
        "mine"
    );
}

#[test]
fn a_ledger_that_does_not_exist_is_refused_by_name_and_nothing_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let (data, store, ledger_db) = first_start(dir.path());
    drop((store, ledger_db));
    let absent = dir.path().join("data").join("absent.sqlite");
    let config = config_file(dir.path(), &data, &absent);
    let target = dir.path().join("out");

    let run = backup_to(&config, &target, &[]);

    assert_eq!(run.status.code(), Some(1), "{}", printed(&run));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains(&format!(
            "a backup carries both databases, but {} does not exist",
            absent.display()
        )),
        "{}",
        printed(&run)
    );
    assert!(!target.exists());
}

#[test]
fn a_ledger_path_that_names_a_directory_fails_the_second_copy_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (data, store, ledger_db) = first_start(dir.path());
    drop((store, ledger_db));
    let not_a_file = dir.path().join("data").join("ledger-dir");
    std::fs::create_dir(&not_a_file).unwrap();
    let config = config_file(dir.path(), &data, &not_a_file);
    let out = dir.path().join("out");
    let target = out.join("nested");

    let run = backup_to(&config, &target, &[]);

    assert_eq!(run.status.code(), Some(1), "{}", printed(&run));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("backup failed:"), "{}", printed(&run));
    assert!(
        stderr.contains(&not_a_file.display().to_string()),
        "{}",
        printed(&run)
    );
    assert!(!stderr.contains("wrote "), "{}", printed(&run));
    assert!(!target.exists(), "{}", printed(&run));
    assert!(!out.exists(), "the parent this run created");
}

/// The refusal `backup` prints for an existing destination someone besides the run's user could write into.
fn refused_line(target: &Path) -> String {
    format!(
        "{}: group or other can write it, or another user owns it. Nothing was written.",
        target.display()
    )
}

#[test]
fn a_destination_group_or_other_can_write_is_refused_and_nothing_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let (data, store, ledger_db) = first_start(dir.path());
    drop((store, ledger_db));
    let config = config_file(dir.path(), &data, &data.ledger);
    for (name, wide) in [("group", 0o770), ("other", 0o707)] {
        let target = dir.path().join(name);
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(wide)).unwrap();

        let run = backup_to(&config, &target, &[]);

        assert_eq!(run.status.code(), Some(1), "{name}: {}", printed(&run));
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            stderr.contains(&refused_line(&target)),
            "{name}: {}",
            printed(&run)
        );
        assert!(entries(&target).is_empty(), "{name}: {}", printed(&run));
        assert_eq!(mode(&target), wide, "{name}");
    }
}

#[test]
#[ignore = "needs a second uid; the owner runs it"]
fn a_destination_another_uid_owns_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (data, store, ledger_db) = first_start(dir.path());
    drop((store, ledger_db));
    let config = config_file(dir.path(), &data, &data.ledger);
    let target = dir.path().join("out");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    let entry = unsafe { libc::getpwnam(c"nobody".as_ptr()) };
    assert!(!entry.is_null(), "this host has no user nobody");
    let (uid, gid) = unsafe { ((*entry).pw_uid, (*entry).pw_gid) };
    assert_ne!(
        uid,
        unsafe { libc::geteuid() },
        "the destination must belong to another uid than the run's"
    );
    std::os::unix::fs::chown(&target, Some(uid), Some(gid))
        .expect("handing the directory to nobody needs root");

    let run = backup_to(&config, &target, &[]);

    assert_eq!(run.status.code(), Some(1), "{}", printed(&run));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains(&refused_line(&target)), "{}", printed(&run));
    assert!(entries(&target).is_empty(), "{}", printed(&run));
}

#[test]
fn a_wide_empty_destination_and_a_missing_key_file_warn_and_the_backup_proceeds() {
    let dir = tempfile::tempdir().unwrap();
    let (data, store, ledger_db) = first_start(dir.path());
    drop((store, ledger_db));
    std::fs::remove_file(&data.key_file).unwrap();
    let config = config_file(dir.path(), &data, &data.ledger);
    let target = dir.path().join("out");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

    let run = backup_to(&config, &target, &[]);

    assert_eq!(run.status.code(), Some(0), "{}", printed(&run));
    let stderr = String::from_utf8_lossy(&run.stderr);
    for line in [
        format!(
            "warning: {} is readable by group or other (mode 0755) where 0700 is expected",
            target.display()
        ),
        format!(
            "warning: the key file {} does not exist, and the registry copy cannot be opened without the key that \
             sealed it",
            data.key_file.display()
        ),
    ] {
        assert!(stderr.lines().any(|printed| printed == line), "{}", printed(&run));
    }
    assert_eq!(
        mode(&target),
        0o755,
        "a directory the run did not create keeps its mode"
    );
    assert_eq!(
        entries(&target),
        ["ledger.sqlite", "manifest.json", "router.sqlite"]
    );
}

#[test]
fn json_prints_the_manifest_with_its_seven_members() {
    let dir = tempfile::tempdir().unwrap();
    let (data, store, ledger_db) = first_start(dir.path());
    drop((store, ledger_db));
    let config = config_file(dir.path(), &data, &data.ledger);
    let target = dir.path().join("out");

    let run = backup_to(&config, &target, &["--json"]);

    assert_eq!(run.status.code(), Some(0), "{}", printed(&run));
    let printed_manifest: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    let members: Vec<&str> = printed_manifest
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        members,
        [
            "format",
            "phoebis_version",
            "taken_at",
            "config",
            "key_file",
            "registry",
            "ledger"
        ]
    );
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(target.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(printed_manifest, written);
}

#[test]
fn a_copy_waits_out_an_exclusive_lock_on_the_registry() {
    let dir = tempfile::tempdir().unwrap();
    let (data, store, ledger_db) = first_start(dir.path());
    // The store's own connection holds a shared lock on the WAL file for as long as it is open, which would keep the
    // second connection from its exclusive one.
    drop((store, ledger_db));
    let config = config_file(dir.path(), &data, &data.ledger);
    let target = dir.path().join("out");
    // The one wall-time wait of this case: no test clock drives another connection's lock.
    let holder = hold_exclusive(&data.registry, || {
        std::thread::sleep(Duration::from_millis(300))
    });
    let started = Instant::now();

    let run = backup_to(&config, &target, &[]);

    let took = started.elapsed();
    holder.join().unwrap();
    assert_eq!(run.status.code(), Some(0), "{}", printed(&run));
    assert!(
        took >= Duration::from_millis(250),
        "the run did not wait for the lock: {took:?}"
    );
    let copy = target.join("router.sqlite");
    assert_eq!(
        Store::read_canary(&copy, &data.key).unwrap(),
        CanaryRead::Matches
    );
    let restore = dir.path().join("restore");
    std::fs::create_dir(&restore).unwrap();
    std::fs::copy(&copy, restore.join("router.sqlite")).unwrap();
    let restored = Store::open(&restore.join("router.sqlite"), &data.key, wall()).unwrap();
    assert_eq!(pools(&restored), ["kept"]);
}

#[test]
fn a_lock_held_past_the_deadline_fails_the_run_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (data, store, ledger_db) = first_start(dir.path());
    drop((store, ledger_db));
    let config = config_file(dir.path(), &data, &data.ledger);
    let target = dir.path().join("out");
    let (release, released) = mpsc::channel::<()>();
    let holder = hold_exclusive(&data.registry, move || released.recv().unwrap());
    let args = BackupArgs {
        json: false,
        to: Some(target.clone()),
    };
    let mut report = Vec::new();

    let result = backup::run(
        &config,
        &args,
        jiff::Timestamp::now(),
        Duration::from_millis(300),
        &mut report,
    );

    release.send(()).unwrap();
    holder.join().unwrap();
    let report = String::from_utf8(report).unwrap();
    match result {
        Err(RunError::Failed { dir, reason }) => {
            assert_eq!(dir, target);
            assert!(
                reason.contains(&data.registry.display().to_string())
                    && reason.contains("held the database locked"),
                "{reason}"
            );
        }
        other => panic!("expected a failed run, got {other:?}"),
    }
    assert!(!target.exists());
    assert!(!report.contains("wrote"), "{report}");
}
