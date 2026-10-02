//! SQLite registry and encrypted credentials.

mod accounts;
mod alerts;
mod budgets;
mod credentials;
mod lock;
mod memory;
mod registry;
mod routing;
mod schema;

pub mod crypto;
pub mod permissions;

use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::{Arc, Condvar};
use std::time::Duration;

use base64::Engine;
use hmac::{Hmac, Mac};
use rand::Rng;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub use accounts::{
    AccountKind, AccountRow, AccountState, Identity, SubscriptionRow, INFERENCE_HOST,
};
pub use alerts::{
    event_deliveries_sql, events_deliveries_sql, lifecycle_deliveries_sql, webhook_host,
    AlertDeliveryRow, AlertEventRow, AlertRuleRow,
};
pub use budgets::{BudgetRow, BudgetScope};
pub use credentials::{
    AttemptOutcome, CredentialBundle, CredentialMetadata, LastAttempt, NewBundle, PendingAttempt,
    ResolvedAttempt, Secret,
};
pub use memory::{MemoryKey, MemoryRecord, MemoryStore};
pub use registry::{
    canonical_id, key_prefix, AccountUpdate, Affinity, AffinityMode, AffinitySource, ApiKeyRow,
    BudgetPeriod, Failover, MemberWrite, ModelRule, OveragePolicy, PoolMember,
    PoolRow, QuotaHeaders, RegistryChange, RegistryLoad, RetiredSecret, RulesMode,
    SessionPlacement, Shedding, Strategy, SubagentPlacement, SubscriptionTerm, WarmSessions,
    ROTATION_GRACE_MS,
};
pub use routing::{
    AffinityChain, AffinityRow, CooldownRow, CreditsRow, OverageRow, PinBurn, PinKey, PinRow,
    RekeyWrite, ResolutionRow, RoutingLoad, RoutingTx, WindowRow,
};

/// The wait on another writer of the registry file (a backup, an operator's `sqlite3`) before a write fails
/// `SQLITE_BUSY`: rusqlite's own default, named so the registry rides no library default.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

// A poisoned store mutex means a prior panic happened while a write was in flight; the store's
// on-disk state may be half-written, so continuing past that point is not safe.
const LOCK_POISONED: &str = "store connection mutex poisoned by a prior panic";

// An attempt still open: unresolved, or resolved `uncertain`, since the upstream may already have rotated the
// refresh token. Shared by every reader that lists or settles open attempts, so they cannot drift apart: a
// commit's supersede and its own resolution, the disconnect and delete sweeps, the listing and the startup
// quarantine that reads it, and `resolve_refresh_attempt`. An `uncertain` row does not block a new refresh:
// `open_refresh_attempt`'s gate reads an attempt in flight alone, and the refresh manager's floor spaces the retry.
const PENDING_WHERE: &str = "resolved_at IS NULL OR outcome = 'uncertain'";

// A known plaintext under its own associated data: the row proves which key sealed the registry without opening a
// credential, and AES-GCM gives a reader of the plaintext nothing toward the key.
const CANARY_AAD: &str = "storage_key_canary";
const CANARY_PLAINTEXT: &[u8] = b"phoebis-storage-key-canary";
// The nonce `crypto::seal` draws, stored as the first bytes of the canary's value.
const CANARY_NONCE_LEN: usize = 12;

#[cfg(test)]
thread_local! {
    /// The `synchronous` value the last `durably` write on this thread committed under, so a test proves each
    /// write's own setting rather than the connection's resting one.
    static DURABLY_SAW: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("account {0} not found")]
    AccountNotFound(String),
    #[error("alert rule {0} not found")]
    AlertRuleNotFound(String),
    #[error("refresh attempt {0} not found")]
    AttemptNotFound(u64),
    #[error("a refresh attempt is already pending for this account")]
    AttemptPending,
    #[error("sealed credential could not be decrypted")]
    Crypto,
    #[error("the slot was disconnected or replaced while the pairing ran")]
    Disconnected,
    #[error("another account already holds this identity")]
    IdentityTaken,
    #[error("storage key file must hold exactly 32 raw bytes or 64 hex characters, with at most one trailing newline")]
    InvalidKey,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The registry's sealed canary did not open under the key the file names, or, with no canary, none of its
    /// sealed rows did.
    #[error("the storage key does not match the registry at {path}")]
    KeyMismatch { path: PathBuf },
    #[error("api key {0} not found")]
    KeyNotFound(String),
    #[error("account {0} already holds credentials, so its kind cannot change")]
    KindChange(String),
    #[error("the data directory is locked by another process")]
    Locked,
    #[error("pool {0} still has api keys assigned to it")]
    PoolInUse(String),
    #[error("pool {0} not found")]
    PoolNotFound(String),
    #[error(
        "the registry's schema version {found} is newer than this build supports ({supported}); \
         restore the backup taken before the upgrade"
    )]
    SchemaTooNew { found: u32, supported: u32 },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("lifecycle or credential generation is stale")]
    Stale,
}

impl StoreError {
    /// Whether the failure is the database's rather than one write's: a lock another connection held past
    /// `BUSY_TIMEOUT`, a failing disk or a full one, which every write after it meets as well.
    pub fn is_database_wide(&self) -> bool {
        matches!(
            self,
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseBusy
                        | rusqlite::ErrorCode::DiskFull
                        | rusqlite::ErrorCode::SystemIoFailure
                )
        )
    }
}

/// What `check-config` learns about a registry file without opening it as the router does.
#[derive(Debug, PartialEq, Eq)]
pub enum CanaryRead {
    /// The registry file does not exist; the first start creates and seals it.
    NoRegistry,
    /// The registry has no canary (a `meta` table without the row, or no `meta` table) and nothing sealed to
    /// prove the key against; the next start seals one.
    Absent,
    /// The registry has no canary yet and a sealed row opened under the key, which is what the next start proves
    /// before it seals one.
    SealedRowMatches,
    /// The canary opened under the key.
    Matches,
}

/// The AES-256 key sealing every credential and API-key secret in the store.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct StorageKey([u8; 32]);

impl StorageKey {
    /// Generates a fresh random key, for tests and first-run bootstrap.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Reads a key file holding exactly 32 raw bytes, or 64 ASCII hex characters with at most one trailing `\n`
    /// or `\r\n`.
    pub fn from_file(path: &Path) -> Result<Self, StoreError> {
        for warning in permissions::check_private_file(path, 0o600) {
            tracing::warn!("{warning}");
        }
        let raw = std::fs::read(path)?;
        if raw.len() == 32 {
            let mut bytes = [0u8; 32];
            bytes.copy_from_slice(&raw);
            return Ok(Self(bytes));
        }
        if let Ok(text) = std::str::from_utf8(&raw) {
            // `openssl rand -hex 32` and most editors end the line they write. The raw form takes no such
            // allowance, since a trailing newline byte there is part of the key.
            let text = text
                .strip_suffix("\r\n")
                .or_else(|| text.strip_suffix('\n'))
                .unwrap_or(text);
            if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
                let mut bytes = [0u8; 32];
                for (i, byte) in bytes.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                        .map_err(|_| StoreError::InvalidKey)?;
                }
                return Ok(Self(bytes));
            }
        }
        Err(StoreError::InvalidKey)
    }
}

impl Clone for StorageKey {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

pub struct Store {
    conn: Mutex<Connection>,
    key: StorageKey,
    _lock: Option<lock::ProcessLock>,
    #[cfg(any(test, feature = "debug-hooks"))]
    fail_next_credential_commit: AtomicBool,
    #[cfg(any(test, feature = "debug-hooks"))]
    fail_next_load: AtomicBool,
    #[cfg(any(test, feature = "debug-hooks"))]
    routing_commits: AtomicU64,
    #[cfg(any(test, feature = "debug-hooks"))]
    routing_hold: Mutex<Option<Arc<HoldGate>>>,
}

impl Store {
    /// Opens (creating if needed) the on-disk registry at `db_path`: the directory holding it at mode 0700,
    /// the process lock beside it, then the database file at mode 0600.
    pub fn open(db_path: &Path, key: &StorageKey) -> Result<Store, StoreError> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

        let dir = permissions::data_dir_of(db_path);
        let dir_existed = dir.exists();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        if dir_existed {
            for warning in permissions::check_private_dir(dir) {
                tracing::warn!("{warning}");
            }
        }
        let process_lock = lock::acquire(dir)?;

        if !db_path.exists() {
            // SQLite creates new database files honoring the process umask; create it ourselves first
            // so credentials never sit in a file readable by other local users, even briefly.
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(db_path)?;
        } else {
            for warning in permissions::check_private_file(db_path, 0o600) {
                tracing::warn!("{warning}");
            }
        }

        let mut conn = Connection::open(db_path)?;
        prepare(&mut conn, true)?;
        check_or_seal_canary(&mut conn, key, db_path)?;
        Ok(Store {
            conn: Mutex::new(conn),
            key: key.clone(),
            _lock: Some(process_lock),
            #[cfg(any(test, feature = "debug-hooks"))]
            fail_next_credential_commit: AtomicBool::new(false),
            #[cfg(any(test, feature = "debug-hooks"))]
            fail_next_load: AtomicBool::new(false),
            #[cfg(any(test, feature = "debug-hooks"))]
            routing_commits: AtomicU64::new(0),
            #[cfg(any(test, feature = "debug-hooks"))]
            routing_hold: Mutex::new(None),
        })
    }

    /// Opens an in-memory registry for tests: no directory, no lock, no on-disk file.
    pub fn open_in_memory(key: &StorageKey) -> Result<Store, StoreError> {
        let mut conn = Connection::open_in_memory()?;
        prepare(&mut conn, false)?;
        check_or_seal_canary(&mut conn, key, Path::new(":memory:"))?;
        Ok(Store {
            conn: Mutex::new(conn),
            key: key.clone(),
            _lock: None,
            #[cfg(any(test, feature = "debug-hooks"))]
            fail_next_credential_commit: AtomicBool::new(false),
            #[cfg(any(test, feature = "debug-hooks"))]
            fail_next_load: AtomicBool::new(false),
            #[cfg(any(test, feature = "debug-hooks"))]
            routing_commits: AtomicU64::new(0),
            #[cfg(any(test, feature = "debug-hooks"))]
            routing_hold: Mutex::new(None),
        })
    }

    pub fn schema_version(&self) -> Result<u32, StoreError> {
        read_version(&self.conn.lock().expect(LOCK_POISONED))
    }

    /// The key an in-memory snapshot digests presented secrets with, so a comparison never needs the raw
    /// storage key outside the store.
    pub fn digest_key(&self) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key.0)
            .expect("HMAC-SHA256 accepts a 32 byte key");
        mac.update(b"api-key-digest");
        mac.finalize().into_bytes().into()
    }

    /// A database copy without the key file cannot use this to test guessed secrets: the digest is keyed,
    /// not a plain hash.
    pub fn api_key_digest(&self, secret: &str) -> [u8; 32] {
        digest_with(&self.digest_key(), secret.as_bytes())
    }

    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_fail_next_load(&self) {
        self.fail_next_load.store(true, Ordering::SeqCst);
    }

    /// Reads the canary through a read-only connection: no process lock, no `prepare`, no migration, no write,
    /// so it is safe beside a running router. With no canary it tries the key on the sealed rows, as the one-time
    /// path at the next start does, so a registry from before the canary fails the check under a key the start
    /// would refuse. `KeyMismatch` names `db_path`. Beside a stopped router SQLite leaves the `-wal` and `-shm`
    /// files a reader of a WAL database needs; the database file itself is never written.
    pub fn read_canary(db_path: &Path, key: &StorageKey) -> Result<CanaryRead, StoreError> {
        if !db_path.try_exists()? {
            return Ok(CanaryRead::NoRegistry);
        }
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let mismatch = || StoreError::KeyMismatch {
            path: db_path.to_path_buf(),
        };
        let stored = if has_table(&conn, "meta")? {
            stored_canary(&conn)?
        } else {
            None
        };
        if let Some(value) = stored {
            return if canary_matches(key, &value) {
                Ok(CanaryRead::Matches)
            } else {
                Err(mismatch())
            };
        }
        match sealed_rows_open_under(&conn, key)? {
            None => Ok(CanaryRead::Absent),
            Some(true) => Ok(CanaryRead::SealedRowMatches),
            Some(false) => Err(mismatch()),
        }
    }

    /// Transactions committed through `write_routing` and autocommits of the per-write methods since the
    /// store opened; the proof that a burst of reports lands in one commit.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_routing_commits(&self) -> u64 {
        self.routing_commits.load(Ordering::SeqCst)
    }

    /// Parks the next `write_routing` before its transaction until the hold is released, so a test can queue a
    /// burst of batches behind one wake.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_hold_next_routing_write(&self) -> RoutingWriteHold {
        let gate = Arc::new(HoldGate::default());
        *self.routing_hold.lock().expect(HOLD_POISONED) = Some(gate.clone());
        RoutingWriteHold(gate)
    }

    /// Before the connection's lock, so the store stays readable while the writer waits.
    #[cfg(any(test, feature = "debug-hooks"))]
    fn park_on_routing_hold(&self) {
        let armed = self.routing_hold.lock().expect(HOLD_POISONED).take();
        if let Some(gate) = armed {
            gate.parked.store(true, Ordering::SeqCst);
            let mut released = gate.released.lock().expect(HOLD_POISONED);
            while !*released {
                released = gate.opened.wait(released).expect(HOLD_POISONED);
            }
        }
    }

    /// Test support: drops the canary row, the shape of a registry sealed before the canary existed.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_drop_canary(&self) -> Result<(), StoreError> {
        self.conn
            .lock()
            .expect(LOCK_POISONED)
            .execute("DELETE FROM meta WHERE key = 'storage_key_canary'", [])?;
        Ok(())
    }
}

#[cfg(any(test, feature = "debug-hooks"))]
const HOLD_POISONED: &str = "routing write hold mutex poisoned by a prior panic";

/// A test's hold on the next `write_routing`, from `Store::debug_hold_next_routing_write`.
#[cfg(any(test, feature = "debug-hooks"))]
#[doc(hidden)]
pub struct RoutingWriteHold(Arc<HoldGate>);

#[cfg(any(test, feature = "debug-hooks"))]
#[derive(Default)]
struct HoldGate {
    parked: AtomicBool,
    released: Mutex<bool>,
    opened: Condvar,
}

#[cfg(any(test, feature = "debug-hooks"))]
impl RoutingWriteHold {
    /// Whether a `write_routing` waits on the hold; a test polls it before it queues what should land behind.
    pub fn is_parked(&self) -> bool {
        self.0.parked.load(Ordering::SeqCst)
    }

    pub fn release(self) {
        drop(self);
    }
}

/// Dropping the hold releases it too, so a test that fails with the writer parked does not leave a blocking
/// thread its runtime waits on at shutdown.
#[cfg(any(test, feature = "debug-hooks"))]
impl Drop for RoutingWriteHold {
    fn drop(&mut self) {
        *self.0.released.lock().expect(HOLD_POISONED) = true;
        self.0.opened.notify_all();
    }
}

/// Proves the key against the registry before any sealed row is read, so a wrong key refuses the open
/// instead of serving a registry it cannot read; a registry sealed before the canary existed is proved
/// against its sealed rows, so a wrong key never seals a canary over a registry it does not match.
///
/// The canary takes its own transaction after `prepare` rather than a migration's: it is no schema change, and a
/// crash between the two leaves a registry the next open seals through the sealed-row proof.
fn check_or_seal_canary(
    conn: &mut Connection,
    key: &StorageKey,
    path: &Path,
) -> Result<(), StoreError> {
    let mismatch = || StoreError::KeyMismatch {
        path: path.to_path_buf(),
    };
    if let Some(value) = stored_canary(conn)? {
        return if canary_matches(key, &value) {
            Ok(())
        } else {
            Err(mismatch())
        };
    }
    if sealed_rows_open_under(conn, key)? == Some(false) {
        return Err(mismatch());
    }
    let (nonce, sealed) = crypto::seal(key, CANARY_AAD, CANARY_PLAINTEXT)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('storage_key_canary', ?1) ON CONFLICT(key) DO NOTHING",
        params![encode_canary(&nonce, &sealed)],
    )?;
    tx.commit()?;
    Ok(())
}

fn stored_canary(conn: &Connection) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'storage_key_canary'",
            [],
            |row| row.get(0),
        )
        .optional()?)
}

fn canary_matches(key: &StorageKey, value: &str) -> bool {
    open_canary(key, value).is_ok_and(|plaintext| plaintext == CANARY_PLAINTEXT)
}

/// A sealed row with the associated data it was sealed under.
struct SealedRow {
    aad: String,
    nonce: Vec<u8>,
    sealed: Vec<u8>,
}

impl SealedRow {
    /// The plaintext is a credential or a secret, so it is wiped as soon as the open has proved the key.
    fn opens_under(&self, key: &StorageKey) -> bool {
        crypto::open(key, &self.aad, &self.nonce, &self.sealed)
            .map(Zeroizing::new)
            .is_ok()
    }
}

/// Whether the key opens any of the registry's first sealed rows, or `None` when it holds nothing sealed. A row
/// that does not open is no proof of a wrong key, since the runtime survives a corrupt row too; a wrong key opens
/// none of them.
fn sealed_rows_open_under(conn: &Connection, key: &StorageKey) -> Result<Option<bool>, StoreError> {
    let rows = first_sealed_rows(conn)?;
    if rows.is_empty() {
        return Ok(None);
    }
    Ok(Some(rows.iter().any(|row| row.opens_under(key))))
}

/// The first sealed row of each kind: a credential bundle, an Anthropic-key secret, an alert secret.
/// `read_canary` meets the file as it is, without `prepare`, so a table the registry does not have yet holds
/// nothing sealed rather than failing the read.
fn first_sealed_rows(conn: &Connection) -> Result<Vec<SealedRow>, StoreError> {
    let mut rows = Vec::new();
    if has_table(conn, "credentials")? {
        rows.extend(
            conn.query_row(
                "SELECT account_id, generation, nonce, sealed FROM credentials LIMIT 1",
                [],
                |row| {
                    Ok(SealedRow {
                        aad: credentials::credential_aad(&row.get::<_, String>(0)?, row.get(1)?),
                        nonce: row.get(2)?,
                        sealed: row.get(3)?,
                    })
                },
            )
            .optional()?,
        );
    }
    if has_table(conn, "api_key_secrets")? {
        rows.extend(
            conn.query_row(
                "SELECT account_id, nonce, sealed FROM api_key_secrets LIMIT 1",
                [],
                |row| {
                    Ok(SealedRow {
                        aad: credentials::api_key_aad(&row.get::<_, String>(0)?),
                        nonce: row.get(1)?,
                        sealed: row.get(2)?,
                    })
                },
            )
            .optional()?,
        );
    }
    if has_table(conn, "alert_rules")? {
        rows.extend(
            conn.query_row(
                "SELECT id, secret_nonce, secret_sealed FROM alert_rules \
                 WHERE secret_sealed IS NOT NULL AND secret_nonce IS NOT NULL LIMIT 1",
                [],
                |row| {
                    Ok(SealedRow {
                        aad: alerts::secret_aad(&row.get::<_, String>(0)?),
                        nonce: row.get(1)?,
                        sealed: row.get(2)?,
                    })
                },
            )
            .optional()?,
        );
    }
    Ok(rows)
}

fn has_table(conn: &Connection, name: &str) -> Result<bool, StoreError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn encode_canary(nonce: &[u8], sealed: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode([nonce, sealed].concat())
}

/// Answers the canary's plaintext. A value that is not base64, or too short to hold a nonce, is refused like a
/// wrong key: no key this build holds sealed it.
fn open_canary(key: &StorageKey, value: &str) -> Result<Vec<u8>, StoreError> {
    let joined = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| StoreError::Crypto)?;
    let (nonce, sealed) = joined
        .split_at_checked(CANARY_NONCE_LEN)
        .ok_or(StoreError::Crypto)?;
    crypto::open(key, CANARY_AAD, nonce, sealed)
}

/// Runs the v1 `SCHEMA`, then every migration above the stored `schema_version`, each inside its own
/// transaction, so a crash mid-migration leaves the version at the last one that actually committed.
fn prepare(conn: &mut Connection, on_disk: bool) -> Result<(), StoreError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // A per-connection setting, never written to the database file, so it costs nothing to set before the
    // version is even known.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let current = read_version(conn)?;
    if current > schema::SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: current,
            supported: schema::SCHEMA_VERSION,
        });
    }
    if on_disk {
        // WAL lets the router task's reads run beside an admin write; an in-memory database has no journal.
        // Unlike `foreign_keys` this rewrites the database header, so it waits behind the version check too.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // A power loss may lose the last transactions not yet synced: routing state the next requests re-derive
        // (affinity, cooldowns, credits, memory, overage, pins, windows) and the operator's other changes, made
        // again by hand. A write whose loss would restore what the operator removed sets `FULL` for itself
        // through `durably`: every access-control change (an account's deletion or disconnect; a key's deletion,
        // edit, revocation or rotation), the credential and secret writes, and the refresh-attempt records, since
        // a rotation lost there retires a refresh token the endpoint already replaced.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
    }
    conn.execute_batch(schema::SCHEMA)?;
    for (version, sql) in schema::MIGRATIONS {
        if *version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![version.to_string()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// Runs `f` with `synchronous = FULL` on this connection and `NORMAL` after it, so a write whose loss would cost
/// a re-pairing or restore an access the operator removed is synced to disk before its commit returns.
pub(crate) fn durably<T>(
    conn: &mut Connection,
    f: impl FnOnce(&mut Connection) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    conn.pragma_update(None, "synchronous", "FULL")?;
    let result = f(conn);
    #[cfg(test)]
    DURABLY_SAW.set(
        conn.pragma_query_value(None, "synchronous", |row| row.get(0))
            .ok(),
    );
    // The write has landed or failed by now, and its answer is the caller's; a connection left at `FULL` only
    // syncs more than it needs to.
    if let Err(error) = conn.pragma_update(None, "synchronous", "NORMAL") {
        tracing::warn!(%error, "the registry connection stays at synchronous = FULL after a durable write");
    }
    result
}

/// A connection that has never run `prepare` has no `meta` table yet, which reads as version 0 so the check
/// above runs before `schema::SCHEMA` creates it. A database from before `meta.schema_version` existed is
/// schema version 1: the v1 `SCHEMA` created every table it needs, just none of the migrations above it.
fn read_version(conn: &Connection) -> Result<u32, StoreError> {
    let has_meta: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_meta {
        return Ok(0);
    }
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stored.and_then(|v| v.parse().ok()).unwrap_or(1))
}

/// Shared by `Store::api_key_digest` and the router task's in-memory snapshot, so both digest a presented
/// secret with the exact same function.
pub fn digest_with(key: &[u8; 32], secret: &[u8]) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC-SHA256 accepts a 32 byte key");
    mac.update(secret);
    mac.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn canary_of(store: &Store) -> Option<String> {
        store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM meta WHERE key = 'storage_key_canary'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    /// An account holding one committed credential bundle, the first sealed row the one-time path proves.
    fn with_a_credential(store: &Store) {
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
                    access_token: Secret::new("at-1"),
                    refresh_token: Some(Secret::new("rt-1")),
                    issued_at: 1_799_996_400_000,
                    expires_at: 1_800_000_000_000,
                    refresh_expires_at: None,
                    scopes: "user:profile user:inference".into(),
                },
            )
            .unwrap();
    }

    #[test]
    fn a_fresh_registry_seals_the_canary_at_its_first_open() {
        let key = StorageKey::generate();
        let store = Store::open_in_memory(&key).unwrap();
        let value = canary_of(&store).expect("the canary row");
        assert_eq!(open_canary(&key, &value).unwrap(), CANARY_PLAINTEXT);
    }

    #[test]
    fn a_registry_sealed_under_one_key_refuses_another_key_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        drop(Store::open(&db, &key).unwrap());
        match Store::open(&db, &StorageKey::generate()) {
            Err(StoreError::KeyMismatch { path }) => {
                assert_eq!(path, db);
                assert_eq!(
                    StoreError::KeyMismatch { path }.to_string(),
                    format!(
                        "the storage key does not match the registry at {}",
                        db.display()
                    )
                );
            }
            Err(other) => panic!("refused for another reason: {other}"),
            Ok(_) => panic!("opened under another key"),
        }
        assert!(
            Store::open(&db, &key).is_ok(),
            "the refusal left the canary as it was"
        );
    }

    #[test]
    fn a_registry_sealed_before_the_canary_proves_the_key_against_its_first_sealed_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        let store = Store::open(&db, &key).unwrap();
        with_a_credential(&store);
        store.debug_drop_canary().unwrap();
        assert_eq!(canary_of(&store), None);
        drop(store);
        match Store::open(&db, &StorageKey::generate()) {
            Err(StoreError::KeyMismatch { path }) => assert_eq!(path, db),
            Err(other) => panic!("refused for another reason: {other}"),
            Ok(_) => panic!("opened under another key"),
        }
        let store = Store::open(&db, &key).unwrap();
        let value = canary_of(&store).expect("the start under the right key sealed the canary");
        assert_eq!(open_canary(&key, &value).unwrap(), CANARY_PLAINTEXT);
        assert_eq!(
            store
                .credentials("a")
                .unwrap()
                .unwrap()
                .access_token
                .expose(),
            "at-1"
        );
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
            state: AccountState::Unpaired,
        }
    }

    /// A registry with no canary: another key is refused by path, at the read and at the open, and seals nothing;
    /// the key that sealed the rows reads `SealedRowMatches`, opens, and seals the canary.
    fn assert_the_sealed_rows_prove_the_key(db: &Path, key: &StorageKey) {
        let other = StorageKey::generate();
        match Store::read_canary(db, &other) {
            Err(StoreError::KeyMismatch { path }) => assert_eq!(path, db),
            answer => panic!("read under another key: {answer:?}"),
        }
        match Store::open(db, &other) {
            Err(StoreError::KeyMismatch { path }) => assert_eq!(path, db),
            Err(other) => panic!("refused for another reason: {other}"),
            Ok(_) => panic!("opened under another key"),
        }
        assert_eq!(
            Store::read_canary(db, key).unwrap(),
            CanaryRead::SealedRowMatches
        );
        let store = Store::open(db, key).unwrap();
        let value = canary_of(&store).expect("the start under the right key sealed the canary");
        assert_eq!(open_canary(key, &value).unwrap(), CANARY_PLAINTEXT);
    }

    #[test]
    fn a_registry_whose_only_sealed_row_is_an_anthropic_key_secret_proves_the_key_against_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        let store = Store::open(&db, &key).unwrap();
        store
            .insert_account(&account("vendor", AccountKind::ApiKey))
            .unwrap();
        store
            .set_api_key_secret("vendor", &Secret::new("sk-ant-test-not-a-key"))
            .unwrap();
        store.debug_drop_canary().unwrap();
        drop(store);
        assert_the_sealed_rows_prove_the_key(&db, &key);
    }

    #[test]
    fn a_registry_whose_only_sealed_row_is_an_alert_secret_proves_the_key_against_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        let store = Store::open(&db, &key).unwrap();
        let rule = AlertRuleRow {
            id: "r1".into(),
            kind: "reauth_required".into(),
            scope_type: "any_account".into(),
            scope_id: None,
            threshold: None,
            window: None,
            webhook_url: "http://127.0.0.1:1/hook".into(),
            has_secret: false,
            enabled: true,
            created_at: 1_000,
            updated_at: 1_000,
        };
        store
            .upsert_alert_rule(
                &rule,
                Some(Some(&Secret::new("webhook-test-not-a-secret"))),
                "test",
            )
            .unwrap();
        store.debug_drop_canary().unwrap();
        drop(store);
        assert_the_sealed_rows_prove_the_key(&db, &key);
    }

    #[test]
    fn a_sealed_row_that_does_not_open_refuses_no_key_another_sealed_row_proves() {
        let dir = tempfile::tempdir().unwrap();
        let key = StorageKey::generate();

        let db = dir.path().join("router.sqlite");
        let store = Store::open(&db, &key).unwrap();
        with_a_credential(&store);
        store.debug_corrupt_credentials("a").unwrap();
        store
            .insert_account(&account("vendor", AccountKind::ApiKey))
            .unwrap();
        store
            .set_api_key_secret("vendor", &Secret::new("sk-ant-test-not-a-key"))
            .unwrap();
        store.debug_drop_canary().unwrap();
        drop(store);
        assert_the_sealed_rows_prove_the_key(&db, &key);

        // With no row that opens there is nothing to prove the key by, so even the key that sealed them is refused.
        let only_corrupt = dir.path().join("only-corrupt.sqlite");
        let store = Store::open(&only_corrupt, &key).unwrap();
        with_a_credential(&store);
        store.debug_corrupt_credentials("a").unwrap();
        store.debug_drop_canary().unwrap();
        drop(store);
        match Store::read_canary(&only_corrupt, &key) {
            Err(StoreError::KeyMismatch { path }) => assert_eq!(path, only_corrupt),
            answer => panic!("{answer:?}"),
        }
        assert!(matches!(
            Store::open(&only_corrupt, &key),
            Err(StoreError::KeyMismatch { .. })
        ));
    }

    #[test]
    fn a_canary_that_is_not_base64_or_too_short_for_a_nonce_is_a_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let key = StorageKey::generate();
        for (name, value) in [
            ("empty", ""),
            ("not-base64", "not base64!"),
            ("short", "AAAA"),
        ] {
            let db = dir.path().join(format!("{name}.sqlite"));
            drop(Store::open(&db, &key).unwrap());
            Connection::open(&db)
                .unwrap()
                .execute(
                    "UPDATE meta SET value = ?1 WHERE key = 'storage_key_canary'",
                    params![value],
                )
                .unwrap();
            match Store::read_canary(&db, &key) {
                Err(StoreError::KeyMismatch { path }) => assert_eq!(path, db, "{name}"),
                answer => panic!("{name}: {answer:?}"),
            }
            match Store::open(&db, &key) {
                Err(StoreError::KeyMismatch { path }) => assert_eq!(path, db, "{name}"),
                Err(other) => panic!("{name}: refused for another reason: {other}"),
                Ok(_) => panic!("{name}: opened"),
            }
        }
    }

    #[test]
    fn read_canary_names_the_four_states() {
        let dir = tempfile::tempdir().unwrap();
        let key = StorageKey::generate();
        let other = StorageKey::generate();

        let missing = dir.path().join("missing.sqlite");
        assert_eq!(
            Store::read_canary(&missing, &key).unwrap(),
            CanaryRead::NoRegistry
        );
        assert!(!missing.exists(), "the read created no file");

        // An empty file is what a first open leaves when it dies before `prepare`, and a v1 registry predates
        // `alert_rules`: neither has every table the sealed-row proof reads.
        let empty_file = dir.path().join("empty-file.sqlite");
        std::fs::write(&empty_file, b"").unwrap();
        let v1 = dir.path().join("v1.sqlite");
        Connection::open(&v1)
            .unwrap()
            .execute_batch(schema::SCHEMA)
            .unwrap();
        let unsealed = dir.path().join("unsealed.sqlite");
        let store = Store::open(&unsealed, &key).unwrap();
        store.debug_drop_canary().unwrap();
        drop(store);
        let legacy = dir.path().join("legacy.sqlite");
        let store = Store::open(&legacy, &key).unwrap();
        with_a_credential(&store);
        store.debug_drop_canary().unwrap();
        drop(store);
        let sealed = dir.path().join("sealed.sqlite");
        drop(Store::open(&sealed, &key).unwrap());

        let files = [&empty_file, &v1, &unsealed, &legacy, &sealed];
        let modified = || files.map(|path| std::fs::metadata(path).unwrap().modified().unwrap());
        let before = modified();
        for path in [&empty_file, &v1, &unsealed] {
            assert_eq!(
                Store::read_canary(path, &key).unwrap(),
                CanaryRead::Absent,
                "{}",
                path.display()
            );
        }
        assert_eq!(
            Store::read_canary(&unsealed, &key).unwrap(),
            CanaryRead::Absent,
            "the first read sealed nothing"
        );
        assert_eq!(
            Store::read_canary(&legacy, &key).unwrap(),
            CanaryRead::SealedRowMatches
        );
        assert_eq!(
            Store::read_canary(&sealed, &key).unwrap(),
            CanaryRead::Matches
        );
        for path in [&legacy, &sealed] {
            match Store::read_canary(path, &other) {
                Err(StoreError::KeyMismatch { path: named }) => assert_eq!(&named, path),
                answer => panic!("{}: {answer:?}", path.display()),
            }
        }
        assert_eq!(modified(), before, "no read wrote a database file");
    }

    #[test]
    fn read_canary_beside_an_open_store_takes_no_lock_and_waits_for_no_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        let store = Store::open(&db, &key).unwrap();
        // The store holds the directory's process lock and its connection sits inside a write transaction, as a
        // running router's writer would.
        let conn = store.conn.lock().unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (sender, answers) = std::sync::mpsc::channel();
        let (path, reader_key) = (db.clone(), key.clone());
        std::thread::spawn(move || {
            let _ = sender.send(Store::read_canary(&path, &reader_key));
        });
        let answer = answers
            .recv_timeout(Duration::from_secs(10))
            .expect("read_canary answered while the store was open");
        assert_eq!(answer.unwrap(), CanaryRead::Matches);
        conn.execute_batch("ROLLBACK").unwrap();
    }

    /// On disk: `synchronous` on an in-memory registry syncs nothing, so only a file proves the setting.
    fn on_disk(dir: &tempfile::TempDir) -> Store {
        Store::open(&dir.path().join("router.sqlite"), &StorageKey::generate()).unwrap()
    }

    fn pragma(store: &Store, name: &str) -> i64 {
        store
            .conn
            .lock()
            .unwrap()
            .pragma_query_value(None, name, |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn an_opened_registry_reads_synchronous_normal_and_a_five_second_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let store = on_disk(&dir);
        assert_eq!(pragma(&store, "synchronous"), 1, "NORMAL");
        // rusqlite sets five seconds at every open as well, so this half guards the named value rather than
        // proving a change.
        assert_eq!(pragma(&store, "busy_timeout"), 5_000);
    }

    #[test]
    fn a_credential_commit_runs_at_full_and_leaves_normal_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = on_disk(&dir);
        with_a_credential(&store);
        assert_eq!(DURABLY_SAW.take(), Some(2), "FULL under the commit");
        assert_eq!(pragma(&store, "synchronous"), 1, "NORMAL after it");
    }

    fn pool(id: &str) -> PoolRow {
        PoolRow {
            id: id.into(),
            label: id.into(),
            enabled: true,
            strategy: Strategy::Priority,
            affinity_ttl_seconds: 3600,
            overage_policy: OveragePolicy::PreferPlan,
            migrate_above_utilization: None,
            select_below_utilization: None,
            model_rules: Vec::new(),
            keepalive_json: "{}".into(),
            body_capture_hours: 0,
            prefer_subscription: false,
            shedding: Shedding::default(),
            failover: Failover::default(),
            cache_ttl_json: "{}".into(),
            cache_diagnostics: None,
            auto_mode_server: true,
            affinity: Affinity::default(),
            quota_headers: QuotaHeaders::Pass,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn api_key(store: &Store, id: &str, pool_id: &str) -> ApiKeyRow {
        ApiKeyRow {
            id: id.into(),
            label: id.into(),
            notes: None,
            pool_id: pool_id.into(),
            enabled: true,
            revoked_at: None,
            expires_at: None,
            model_rules: Vec::new(),
            model_rules_mode: RulesMode::Extend,
            long_context_twins: true,
            keepalive_opt_out: false,
            body_capture_hours: None,
            secret_digest: store.api_key_digest("phoebis-test-not-a-secret").to_vec(),
            prefix: "test0000".into(),
            created_at: 0,
            rotated_at: None,
            last_used_at: None,
            retired_secret_last_used_at: None,
            in_flight_cap: None,
            affinity_source: AffinitySource::Auto,
            auto_mode_server: None,
        }
    }

    #[test]
    fn a_revocation_and_a_refresh_attempts_record_run_at_full_and_leave_normal_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = on_disk(&dir);
        with_a_credential(&store);
        store.insert_pool(&pool("p"), "test").unwrap();
        store
            .insert_api_key(&api_key(&store, "k", "p"), "test")
            .unwrap();
        DURABLY_SAW.take();

        store.revoke_api_key("k", 2_000, "test").unwrap();
        assert_eq!(DURABLY_SAW.take(), Some(2), "FULL under the revocation");
        assert_eq!(pragma(&store, "synchronous"), 1, "NORMAL after it");
        assert!(matches!(
            store.revoke_api_key("gone", 2_000, "test"),
            Err(StoreError::KeyNotFound(_))
        ));
        assert_eq!(
            DURABLY_SAW.take(),
            Some(2),
            "FULL under a refused revocation"
        );
        assert_eq!(
            pragma(&store, "synchronous"),
            1,
            "NORMAL after the refusal too"
        );

        let generation = store.credential_generation("a").unwrap().unwrap();
        let attempt = store.open_refresh_attempt("a", 1, 1, generation).unwrap();
        assert_eq!(
            DURABLY_SAW.take(),
            Some(2),
            "FULL under the attempt's intent"
        );
        assert_eq!(pragma(&store, "synchronous"), 1, "NORMAL after it");
        store
            .resolve_refresh_attempt(attempt, 1, AttemptOutcome::Failed("test".into()))
            .unwrap();
        assert_eq!(
            DURABLY_SAW.take(),
            Some(2),
            "FULL under the attempt's resolution"
        );
        assert_eq!(pragma(&store, "synchronous"), 1, "NORMAL after it");
    }

    /// Reads and clears the seam: `Some(2)` after a write `durably` ran, `None` after one it did not, and the
    /// connection at `NORMAL` after either.
    #[track_caller]
    fn assert_synced(store: &Store, at_full: bool, what: &str) {
        assert_eq!(DURABLY_SAW.take(), at_full.then_some(2), "{what}");
        assert_eq!(pragma(store, "synchronous"), 1, "{what}: NORMAL after it");
    }

    /// An on-disk registry holding account `a` with a credential, pool `p` and key `k`, the seam cleared.
    fn keyed(dir: &tempfile::TempDir) -> Store {
        let store = on_disk(dir);
        with_a_credential(&store);
        store.insert_pool(&pool("p"), "test").unwrap();
        store
            .insert_api_key(&api_key(&store, "k", "p"), "test")
            .unwrap();
        DURABLY_SAW.take();
        store
    }

    #[test]
    fn a_key_edit_runs_at_full_and_leaves_normal_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = keyed(&dir);
        store
            .update_api_key(
                &ApiKeyRow {
                    enabled: false,
                    expires_at: Some(5_000),
                    ..api_key(&store, "k", "p")
                },
                "test",
            )
            .unwrap();
        assert_synced(&store, true, "a key disabled and given an expiry");
    }

    #[test]
    fn a_key_rotation_runs_at_full_and_leaves_normal_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = keyed(&dir);
        let digest = store.api_key_digest("phoebis-test-rotated-not-a-secret");
        store
            .rotate_api_key("k", &digest, "test0001", 2_000, "test")
            .unwrap();
        assert_synced(&store, true, "a key rotation");
    }

    #[test]
    fn a_key_deletion_runs_at_full_and_leaves_normal_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = keyed(&dir);
        store.delete_api_key("k", "test").unwrap();
        assert_synced(&store, true, "a key deletion");
    }

    #[test]
    fn a_disconnect_runs_at_full_and_leaves_normal_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = keyed(&dir);
        store.bump_lifecycle("a").unwrap();
        assert_synced(&store, true, "a disconnect");
    }

    #[test]
    fn an_account_deletion_runs_at_full_and_leaves_normal_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = keyed(&dir);
        store.delete_account("a", "test").unwrap();
        assert_synced(&store, true, "an account deletion");
    }

    #[test]
    fn an_anthropic_key_secret_write_runs_at_full_and_leaves_normal_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = keyed(&dir);
        store
            .insert_account(&account("vendor", AccountKind::ApiKey))
            .unwrap();
        store
            .set_api_key_secret("vendor", &Secret::new("sk-ant-test-not-a-key"))
            .unwrap();
        assert_synced(&store, true, "an Anthropic key's secret");
    }

    #[test]
    fn an_account_created_with_a_secret_runs_at_full_and_one_without_stays_at_normal() {
        let dir = tempfile::tempdir().unwrap();
        let store = keyed(&dir);
        let secret = Secret::new("sk-ant-test-not-a-key");
        store
            .create_account(
                &account("vendor", AccountKind::ApiKey),
                Some(&secret),
                2_000,
                "test",
            )
            .unwrap();
        assert_synced(&store, true, "a creation carrying a secret");
        store
            .create_account(&account("other", AccountKind::ApiKey), None, 2_000, "test")
            .unwrap();
        assert_synced(&store, false, "a creation without one");
    }

    #[test]
    fn an_account_update_with_a_secret_runs_at_full_and_one_without_stays_at_normal() {
        let dir = tempfile::tempdir().unwrap();
        let store = keyed(&dir);
        let secret = Secret::new("sk-ant-test-not-a-key");
        store
            .create_account(
                &account("vendor", AccountKind::ApiKey),
                Some(&secret),
                2_000,
                "test",
            )
            .unwrap();
        DURABLY_SAW.take();
        let update = |secret: Option<&Secret>| {
            store
                .commit_account_update(&AccountUpdate {
                    id: "vendor",
                    expected_revision: store.account("vendor").unwrap().unwrap().revision,
                    lifecycle_generation: 1,
                    label: "vendor",
                    secret,
                    state: AccountState::Ready,
                    at: 3_000,
                    actor: "test",
                })
                .unwrap();
        };
        update(Some(&secret));
        assert_synced(&store, true, "an update carrying a secret");
        update(None);
        assert_synced(&store, false, "an update without one");
    }

    #[test]
    fn a_held_lock_a_failing_disk_or_a_full_one_is_database_wide_and_a_refused_row_is_not() {
        let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
        let refused = store
            .upsert_cooldown(&CooldownRow {
                account_id: "ghost".into(),
                scope: "account".into(),
                until: 10_000,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
            })
            .unwrap_err();
        assert!(!refused.is_database_wide(), "{refused}");
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_FULL,
            rusqlite::ffi::SQLITE_IOERR_WRITE,
        ] {
            let error = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            assert!(error.is_database_wide(), "{error}");
        }
    }

    #[test]
    fn schema_version_is_the_last_entry_of_migrations() {
        let last = schema::MIGRATIONS.last().expect("at least one migration").0;
        assert_eq!(schema::SCHEMA_VERSION, last);
    }

    #[test]
    fn from_file_takes_a_trailing_newline_after_the_hex_and_refuses_anything_longer() {
        let dir = tempfile::tempdir().unwrap();
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let read = |name: &str, contents: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, contents).unwrap();
            StorageKey::from_file(&path)
        };
        let bare = read("bare", hex.as_bytes()).expect("64 hex characters");
        for (name, contents) in [("lf", format!("{hex}\n")), ("crlf", format!("{hex}\r\n"))] {
            let key = read(name, contents.as_bytes()).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(key.0, bare.0, "{name}");
        }
        let mut raw_and_newline = vec![b'Z'; 32];
        raw_and_newline.push(b'\n');
        for (name, contents) in [
            ("two-newlines", format!("{hex}\n\n").into_bytes()),
            ("66-hex", format!("{hex}ab").into_bytes()),
            ("raw-and-newline", raw_and_newline),
        ] {
            assert!(
                matches!(read(name, &contents), Err(StoreError::InvalidKey)),
                "{name}"
            );
        }
    }

    #[test]
    fn a_registry_stamped_by_a_newer_build_refuses_to_open_before_any_table_exists() {
        // Hand-written rather than built by a first `prepare` call: a schema already at `SCHEMA_VERSION` would
        // make every statement in `schema::SCHEMA` a no-op regardless of where the version check ran, so this
        // is the only setup that actually proves the check runs before any DDL does.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
            params![(schema::SCHEMA_VERSION + 1).to_string()],
        )
        .unwrap();
        let error = prepare(&mut conn, false).unwrap_err();
        assert!(
            matches!(
                error,
                StoreError::SchemaTooNew { found, supported }
                if found == schema::SCHEMA_VERSION + 1 && supported == schema::SCHEMA_VERSION
            ),
            "{error:?}"
        );
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            tables,
            vec!["meta".to_string()],
            "no statement of schema::SCHEMA or a migration ran before the refusal"
        );
    }
}
