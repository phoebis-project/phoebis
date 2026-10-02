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

use std::path::Path;
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use hmac::{Hmac, Mac};
use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

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
    canonical_id, key_prefix, AccountUpdate, Affinity, AffinityMode, ApiKeyRow, BudgetPeriod,
    Failover, MemberWrite, ModelRule, OveragePolicy, PoolMember, PoolRow,
    RegistryChange, RegistryLoad, RetiredSecret, RulesMode, SessionPlacement, Shedding, Strategy,
    SubagentPlacement, SubscriptionTerm, WarmSessions, ROTATION_GRACE_MS,
};
pub use routing::{
    AffinityChain, AffinityRow, CooldownRow, CreditsRow, OverageRow, PinBurn, PinKey, PinRow,
    RekeyWrite, ResolutionRow, RoutingLoad, WindowRow,
};

// A poisoned store mutex means a prior panic happened while a write was in flight; the store's
// on-disk state may be half-written, so continuing past that point is not safe.
const LOCK_POISONED: &str = "store connection mutex poisoned by a prior panic";

// Shared by `open_refresh_attempt`'s gate and `pending_refresh_attempts`'s listing, so the two queries
// cannot drift apart: an unresolved attempt, or one resolved `uncertain`, still blocks a new refresh,
// because the upstream may already have rotated the refresh token.
const PENDING_WHERE: &str = "resolved_at IS NULL OR outcome = 'uncertain'";

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
    #[error("storage key file must hold exactly 32 raw bytes or 64 hex characters")]
    InvalidKey,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
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

    /// Reads a key file holding exactly 32 raw bytes, or exactly 64 ASCII hex characters.
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
    fail_next_load: AtomicBool,
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
        Ok(Store {
            conn: Mutex::new(conn),
            key: key.clone(),
            _lock: Some(process_lock),
            #[cfg(any(test, feature = "debug-hooks"))]
            fail_next_load: AtomicBool::new(false),
        })
    }

    /// Opens an in-memory registry for tests: no directory, no lock, no on-disk file.
    pub fn open_in_memory(key: &StorageKey) -> Result<Store, StoreError> {
        let mut conn = Connection::open_in_memory()?;
        prepare(&mut conn, false)?;
        Ok(Store {
            conn: Mutex::new(conn),
            key: key.clone(),
            _lock: None,
            #[cfg(any(test, feature = "debug-hooks"))]
            fail_next_load: AtomicBool::new(false),
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
}

/// Runs the v1 `SCHEMA`, then every migration above the stored `schema_version`, each inside its own
/// transaction, so a crash mid-migration leaves the version at the last one that actually committed.
fn prepare(conn: &mut Connection, on_disk: bool) -> Result<(), StoreError> {
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
    use super::*;

    #[test]
    fn schema_version_is_the_last_entry_of_migrations() {
        let last = schema::MIGRATIONS.last().expect("at least one migration").0;
        assert_eq!(schema::SCHEMA_VERSION, last);
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
