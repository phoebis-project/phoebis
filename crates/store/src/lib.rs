//! SQLite registry and encrypted credentials.

/// A closed set of words, each variant beside its one word, with `WORDS` listing them in declaration order. The enum
/// reads and writes one text column, so the column's reader and its writer cannot drift apart, and a stored word
/// outside the set fails the read with the word named. Exported for the ledger's columns, whose expansion resolves the
/// `rusqlite` paths against the ledger's own dependency. A leading `plain` leaves the column conversions out, so its
/// expansion names no `rusqlite` path and serves a crate without that dependency, such as `phoebis-attention`. A
/// leading `plain deserialize` adds to that expansion a `Deserialize` through `parse` for a query's word parameter, so
/// a refusal of the query extractor names the words, with its `serde` paths resolved against the calling crate's own
/// dependency.
#[macro_export]
#[doc(hidden)]
macro_rules! text_enum {
    (
        plain deserialize
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $word:literal),* $(,)?
        }
    ) => {
        $crate::text_enum! {
            plain
            $(#[$meta])*
            pub enum $name {
                $($(#[$variant_meta])* $variant => $word),*
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let word = <String as serde::Deserialize>::deserialize(deserializer)?;
                $name::parse(&word)
                    .ok_or_else(|| serde::de::Error::unknown_variant(&word, $name::WORDS))
            }
        }
    };
    (
        plain
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $word:literal),* $(,)?
        }
    ) => {
        $(#[$meta])*
        pub enum $name {
            $($(#[$variant_meta])* $variant),*
        }

        impl $name {
            /// Every word of the set, in declaration order.
            pub const WORDS: &[&str] = &[$($word),*];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $word),*
                }
            }

            /// The variant `value` names, `None` for a word outside the set.
            pub fn parse(value: &str) -> Option<$name> {
                match value {
                    $($word => Some($name::$variant),)*
                    _ => None,
                }
            }
        }
    };
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $word:literal),* $(,)?
        }
    ) => {
        $crate::text_enum! {
            plain
            $(#[$meta])*
            pub enum $name {
                $($(#[$variant_meta])* $variant => $word),*
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(
                value: rusqlite::types::ValueRef<'_>,
            ) -> rusqlite::types::FromSqlResult<Self> {
                let word = value.as_str()?;
                $name::parse(word).ok_or_else(|| {
                    rusqlite::types::FromSqlError::Other($crate::unknown_word_error(word))
                })
            }
        }

        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(self.as_str().into())
            }
        }
    };
}

/// The error of a stored word outside a decoder's set, one text for every decoder. Public for the expansions of
/// `text_enum!` in the ledger's columns.
#[doc(hidden)]
pub fn unknown_word_error(word: &str) -> Box<dyn std::error::Error + Send + Sync> {
    format!("unknown word {word:?}").into()
}

/// The SET clause that writes `$new` to `accounts.state` and moves `state_changed_at` to `$stamp` only when the state
/// changes. SQLite reads `state` in each SET expression as the row's value before the write, so the CASE compares the
/// old state with the new one.
macro_rules! state_write {
    ($new:literal, $stamp:literal) => {
        concat!(
            "state = ",
            $new,
            ", state_changed_at = CASE WHEN state = ",
            $new,
            " THEN state_changed_at ELSE ",
            $stamp,
            " END"
        )
    };
}

mod accounts;
mod credentials;
mod lock;
mod memory;
mod registry;
mod routing;
mod schema;

pub mod backup;
pub mod crypto;
pub mod engine_state;
pub mod migrations;
pub mod permissions;

use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::Condvar;
use std::sync::{Arc, LockResult, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine;
use hmac::{Hmac, Mac};
use rand::Rng;
use rusqlite::{params, Connection, OpenFlags, TransactionBehavior};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::migrations::has_table;

pub use accounts::{
    answers_stamp_token, limit_resets_token, stamp_token, AccountExtras, AccountKind, AccountRow,
    AccountState, Identity, LimitResetsRow, StateCause, SubscriptionRow, INFERENCE_HOST,
};
pub use backup::{snapshot, BackupError, RETRY_PAUSE};
pub use credentials::{
    AttemptOutcome, CredentialBundle, CredentialMetadata, CredentialStanding, LastAttempt,
    NewBundle, OutcomeWord, PendingAttempt, RefreshHistory, ResolvedAttempt, Secret, Settled,
};
pub use engine_state::{
    keepalive_aad, CompanionAgentRow, CompanionSessionRow, EngineBatch, EngineState, FactsKey,
    FactsKind, FactsRow, IdleReturnsRow, KeepaliveAgentRow, StoredFacts,
};
pub use memory::{MemoryKey, MemoryRecord, MemoryStore};
pub use registry::{
    canonical_id, key_prefix, reports_no_plan, AccountUpdate, Affinity, AffinityMode, ApiKeyRow,
    AttachmentSubject, BudgetPeriod, Cancellation, CompanionPermission, CompanionPermissions,
    Failover, Feature, FeatureOverride, GuardrailAttachment, GuardrailIndex, GuardrailRow,
    KeyFeatureAccounts, MemberWrite, ModelRule, OveragePolicy, PoolFeatureAccounts, PoolMember,
    PoolRow, PresetStamp, RegistryChange, RegistryLoad, RetiredSecret, ReturnToPlan, RulesMode,
    SessionPlacement, Shedding, SlowDown, Strategy, SubagentPlacement, SubjectKind,
    SubscriptionTerm, WarmSessions, GUARDRAIL_BOUND, KEY_SECRET_PREFIX, LONG_CONTEXT_SUFFIX,
    ROTATION_GRACE_MS,
};
pub use routing::{
    AffinityChain, AffinityRow, CooldownRow, CooldownScope, CreditsRow, EvidenceSource,
    LapsedAgentRow, MarkSource, ModelWeekHourRow, OverageRow, PinBurn, PinKey, PinMark, PinRow,
    RekeyWrite, ResolutionRow, RoutingLoad, RoutingTx, ShedRoundRow, ShedStamp, ThinkingBindingRow,
    ThinkingSum, WindowRow, WindowSetRow,
};
pub use schema::SCHEMA_VERSION;

/// The wait on another writer of the registry file (a backup, an operator's `sqlite3`) before a write fails
/// `SQLITE_BUSY`: rusqlite's own default, named so the registry rides no library default.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The connection's cache of prepared statements. The routing writer's batch alone runs about 20 cached statements,
/// and the session memory, the save of the engine's state and the per-holder credential reads cache theirs beside
/// them, about 38 in all, so rusqlite's default of 16 would evict and recompile some on every batch. 64 holds them all
/// with room.
const STATEMENT_CACHE_CAPACITY: usize = 64;

// A poisoned store mutex means a prior panic happened while a write was in flight; the store's
// on-disk state may be half-written, so continuing past that point is not safe.
const LOCK_POISONED: &str = "store connection mutex poisoned by a prior panic";

// An attempt still open: unresolved, or resolved `uncertain`, since the upstream may already have rotated the
// refresh token. Shared by every reader that lists or settles open attempts, so they cannot drift apart: a
// commit's supersede and its own resolution, the disconnect and delete sweeps, the listing and the startup
// quarantine that reads it, and `resolve_refresh_attempt`. An `uncertain` row does not block a new refresh:
// `open_refresh_attempt`'s gate reads an attempt in flight alone, and the refresh manager's floor spaces the retry.
const PENDING_WHERE: &str = "resolved_at IS NULL OR outcome = 'uncertain'";

// One transaction of the history purge, so a writer of the registry waits behind a slice rather than the pass.
const HISTORY_SLICE: usize = 1_000;

// A resolved attempt under the cutoff, ordered along `refresh_attempts_resolved`. An `uncertain` row stays, since the
// startup quarantine still reads it as open, and so do each holder's newest two settled rows and its newest row by
// id, whatever their age: the attention reader's run of failures reads the first by settling, and the account
// view's last attempt the second, for a holder no refresh touched within the retention. The two orders part when
// older uncertain rows settle after the newest attempt.
const PURGE_ATTEMPTS: &str = "DELETE FROM refresh_attempts WHERE id IN ( \
     SELECT r.id FROM refresh_attempts r \
     WHERE r.outcome IS NOT NULL AND r.outcome <> 'uncertain' AND r.resolved_at < ?1 \
       AND r.id NOT IN (SELECT k.id FROM refresh_attempts k \
                        WHERE k.account_id = r.account_id AND k.incarnation = r.incarnation \
                          AND k.outcome IS NOT NULL \
                        ORDER BY k.resolved_at DESC, k.id DESC LIMIT 2) \
       AND r.id <> (SELECT MAX(m.id) FROM refresh_attempts m \
                    WHERE m.account_id = r.account_id AND m.incarnation = r.incarnation) \
     ORDER BY r.resolved_at, r.id LIMIT ?2)";

// A change under the cutoff, oldest id first: no index leads with `at`, and the ids follow it. Each key id keeps its
// newest `key_created`, whatever its age, since `insert_api_key` stamps a re-created key past the newest create.
const PURGE_CHANGES: &str = "DELETE FROM registry_changes WHERE id IN ( \
     SELECT c.id FROM registry_changes c \
     WHERE c.at < ?1 \
       AND NOT (c.kind = 'key_created' \
                AND c.id = (SELECT MAX(n.id) FROM registry_changes n \
                            WHERE n.target = c.target AND n.kind = c.kind)) \
     ORDER BY c.id LIMIT ?2)";

// A stored set whose session last reported before the cutoff. Each select leads with the kind, so the index
// `companion_facts_seen` serves both cuts.
const PURGE_AGENT_FACTS: &str = "DELETE FROM companion_facts WHERE rowid IN ( \
     SELECT rowid FROM companion_facts WHERE kind = ?3 AND seen_at < ?1 LIMIT ?2)";
const PURGE_SESSION_FACTS: &str = "DELETE FROM companion_facts WHERE rowid IN ( \
     SELECT rowid FROM companion_facts WHERE kind IN (?3, ?4) AND seen_at < ?1 LIMIT ?2)";

// A known plaintext under its own associated data: the row proves which key sealed the registry without opening a
// credential, and AES-GCM gives a reader of the plaintext nothing toward the key.
const CANARY_AAD: &str = "storage_key_canary";
const CANARY_PLAINTEXT: &[u8] = b"phoebis-storage-key-canary";

#[cfg(test)]
thread_local! {
    /// The `synchronous` value the last routing commit on this thread ran under, so a test reads the level
    /// inside the commit rather than the connection's resting one.
    static ROUTING_COMMIT_SAW: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("account {account_id} is a member of pool {pool_id}")]
    AccountInPool { account_id: String, pool_id: String },
    #[error("account {0} not found")]
    AccountNotFound(String),
    #[error("refresh attempt {0} not found")]
    AttemptNotFound(u64),
    #[error("a refresh attempt is already pending for this account")]
    AttemptPending,
    #[error("sealed credential could not be decrypted")]
    Crypto,
    #[error("the slot was disconnected or replaced while the pairing ran")]
    Disconnected,
    /// A feature account selection names an id no account holds.
    #[error("feature account {0} not found")]
    FeatureAccountNotFound(String),
    /// A feature account selection names an account that holds no claude.ai token.
    #[error("feature account {0} is not an OAuth account")]
    FeatureAccountNotOauth(String),
    #[error("guardrail {0} not found")]
    GuardrailNotFound(String),
    #[error("another account already holds this identity")]
    IdentityTaken,
    /// A create's insert met a row already standing under its id: another create won the race for it.
    #[error("{0} is already taken")]
    IdTaken(String),
    #[error("storage key file must hold exactly 32 raw bytes or 64 hex characters, with at most one trailing newline")]
    InvalidKey,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("key {0} would hold more than 64 guardrails")]
    KeyGuardrailBound(String),
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
    #[error("pool {0} would hold more than 64 guardrails")]
    PoolGuardrailBound(String),
    #[error("pool {0} still has api keys assigned to it")]
    PoolInUse(String),
    #[error("pool {0} not found")]
    PoolNotFound(String),
    #[error(
        "the registry's schema version {found} is newer than the {supported} this build opens: restore the backup \
         taken before the upgrade, or recreate a registry a pre-release build wrote with phoebis init"
    )]
    SchemaTooNew { found: u32, supported: u32 },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("lifecycle or credential generation is stale")]
    Stale,
}

impl From<migrations::VersionError> for StoreError {
    fn from(error: migrations::VersionError) -> StoreError {
        match error {
            migrations::VersionError::Io(error) => StoreError::Io(error),
            migrations::VersionError::Sqlite(error) => StoreError::Sqlite(error),
            migrations::VersionError::TooNew { found, supported } => {
                StoreError::SchemaTooNew { found, supported }
            }
        }
    }
}

impl StoreError {
    /// Whether the failure is the database's rather than one write's, as `is_database_wide_failure` reads it.
    pub fn is_database_wide(&self) -> bool {
        matches!(self, StoreError::Sqlite(error) if is_database_wide_failure(error))
    }
}

/// Whether a SQLite failure is the database's rather than one write's: a lock held past the busy timeout, a full disk,
/// an I/O error, a closed handle, or a file that cannot be opened, is read-only (moved or deleted under the router
/// among them), corrupt or no database at all. Every write after it meets it too. The registry and the ledger both
/// read their failures through it, so the two files classify a failure alike.
pub fn is_database_wide_failure(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::ApiMisuse
                    | rusqlite::ErrorCode::CannotOpen
                    | rusqlite::ErrorCode::DatabaseBusy
                    | rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::DiskFull
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::ReadOnly
                    | rusqlite::ErrorCode::SystemIoFailure
            )
    )
}

/// What one `Store::purge_history` call deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurgedHistory {
    pub attempts: u64,
    pub changes: u64,
}

/// What one cut of the stored facts removed.
#[derive(Debug)]
pub struct PurgedFacts {
    pub agents: u64,
    pub sessions: u64,
}

/// What a key recovery discards, printed as counts before the operator confirms and after the transaction lands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SealedCounts {
    pub api_key_accounts: u64,
    pub api_key_secrets: u64,
    pub companion_facts: u64,
    pub credentials: u64,
    pub oauth_accounts: u64,
    pub open_refresh_attempts: u64,
    pub recipe_bodies: u64,
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

    /// The key as the 64 lowercase hex characters `from_file` reads, zeroed when dropped; `init` writes it and nothing
    /// else reads it.
    pub fn to_hex(&self) -> zeroize::Zeroizing<String> {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        // Sized up front, so pushing never reallocates and leaves an unzeroed copy of the key behind.
        let mut hex = zeroize::Zeroizing::new(String::with_capacity(64));
        for byte in self.0.iter() {
            hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
            hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        hex
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

/// The key `Store::companion_key` answers. A type of its own, so the API-key digest key, the same 32 bytes in shape,
/// cannot stand in for it.
#[derive(Clone, Copy)]
pub struct CompanionKey([u8; 32]);

impl CompanionKey {
    /// Eight lowercase hex characters: the first four bytes of the HMAC of the account id under this key.
    pub fn pseudonym(&self, account_id: &str) -> String {
        let digest = digest_with(&self.0, account_id.as_bytes());
        format!(
            "{:08x}",
            u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
        )
    }
}

/// The instant the store stamps a row with, in Unix milliseconds. The caller hands it to the open: the router passes
/// the clock that its other parts read, and a suite passes the wall clock or a cell that it steps.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// A `Clock` that reads the wall clock in Unix milliseconds.
pub fn wall_clock() -> Clock {
    Arc::new(|| jiff::Timestamp::now().as_millisecond())
}

pub struct Store {
    clock: Clock,
    conn: CountedMutex<Connection>,
    key: StorageKey,
    _lock: Option<lock::ProcessLock>,
    #[cfg(any(test, feature = "debug-hooks"))]
    hooks: DebugHooks,
}

/// The test seams' state: each field's default is the state both constructors start it in.
#[cfg(any(test, feature = "debug-hooks"))]
#[derive(Default)]
struct DebugHooks {
    credential_after_hold: Mutex<Option<Arc<HoldGate>>>,
    credential_hold: Mutex<Option<Arc<HoldGate>>>,
    credential_reads: AtomicU64,
    fail_next_credential_commit: AtomicBool,
    fail_next_load: AtomicBool,
    fail_next_refresh_read: AtomicBool,
    fail_next_schema_version: AtomicBool,
    key_write_hold: Mutex<Option<Arc<HoldGate>>>,
    load_hold: Mutex<Option<Arc<HoldGate>>>,
    loads: AtomicU64,
    pool_write_hold: Mutex<Option<Arc<HoldGate>>>,
    routing_commits: AtomicU64,
    routing_hold: Mutex<Option<Arc<HoldGate>>>,
}

/// The connection's mutex. Under `debug-hooks` it counts its takes, so a test can prove a path pays none; `lock`
/// answers what `Mutex::lock` answers, so every caller reads it as it reads a plain mutex.
struct CountedMutex<T> {
    mutex: Mutex<T>,
    #[cfg(any(test, feature = "debug-hooks"))]
    lock_count: AtomicU64,
}

impl<T> CountedMutex<T> {
    fn new(value: T) -> CountedMutex<T> {
        CountedMutex {
            mutex: Mutex::new(value),
            #[cfg(any(test, feature = "debug-hooks"))]
            lock_count: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.lock_count.fetch_add(1, Ordering::SeqCst);
        self.mutex.lock()
    }
}

impl Store {
    fn from_parts(
        conn: Connection,
        key: &StorageKey,
        lock: Option<lock::ProcessLock>,
        clock: Clock,
    ) -> Store {
        Store {
            clock,
            conn: CountedMutex::new(conn),
            key: key.clone(),
            _lock: lock,
            #[cfg(any(test, feature = "debug-hooks"))]
            hooks: DebugHooks::default(),
        }
    }

    /// Opens (creating if needed) the on-disk registry at `db_path`: the directory holding it at mode 0700,
    /// the process lock beside it, then the database file at mode 0600.
    pub fn open(db_path: &Path, key: &StorageKey, clock: Clock) -> Result<Store, StoreError> {
        let dir = permissions::data_dir_of(db_path);
        let dir_existed = dir.exists();
        permissions::create_private_dir(dir)?;
        if dir_existed {
            for warning in permissions::check_private_dir(dir) {
                tracing::warn!("{warning}");
            }
        }
        let process_lock = lock::acquire(dir)?;

        if !db_path.exists() {
            // The file holds credentials, so it is private from its creation.
            permissions::create_private_if_missing(db_path)?;
        } else {
            for warning in permissions::check_private_file(db_path, 0o600) {
                tracing::warn!("{warning}");
            }
        }

        let mut conn = Connection::open(db_path)?;
        prepare(&mut conn, schema::MIGRATIONS)?;
        check_or_seal_canary(&mut conn, key, db_path)?;
        Ok(Store::from_parts(conn, key, Some(process_lock), clock))
    }

    /// Opens an in-memory registry: no directory, no lock, no on-disk file.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory(key: &StorageKey, clock: Clock) -> Result<Store, StoreError> {
        let mut conn = Connection::open_in_memory()?;
        prepare(&mut conn, schema::MIGRATIONS)?;
        check_or_seal_canary(&mut conn, key, Path::new(":memory:"))?;
        Ok(Store::from_parts(conn, key, None, clock))
    }

    pub(crate) fn now_ms(&self) -> i64 {
        (self.clock)()
    }

    pub fn schema_version(&self) -> Result<u32, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        injected_failure(
            &self.hooks.fail_next_schema_version,
            "injected schema version read failure",
        )?;
        read_version(&self.conn.lock().expect(LOCK_POISONED))
    }

    /// The epoch the persistent admin tokens' key folds in, 0 before the first persistent logout.
    pub fn admin_session_epoch(&self) -> Result<u64, StoreError> {
        read_admin_session_epoch(&self.conn.lock().expect(LOCK_POISONED))
    }

    /// Moves the epoch on by one and answers the new value.
    pub fn bump_admin_session_epoch(&self) -> Result<u64, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction()?;
        let epoch = read_admin_session_epoch(&tx)? + 1;
        migrations::set_meta_value(&tx, "admin_session_epoch", &epoch.to_string())?;
        tx.commit()?;
        Ok(epoch)
    }

    /// The key an in-memory snapshot digests presented secrets with, so a comparison never needs the raw
    /// storage key outside the store.
    pub fn digest_key(&self) -> [u8; 32] {
        digest_with(&self.key.0, b"api-key-digest")
    }

    /// The key the companion's account pseudonyms are digested under. A label of its own keeps a pseudonym apart
    /// from every secret digest, and the storage key keeps it stable across restarts and distinct per installation.
    pub fn companion_key(&self) -> CompanionKey {
        CompanionKey(digest_with(&self.key.0, b"companion-pseudonym"))
    }

    /// A database copy without the key file cannot use this to test guessed secrets: the digest is keyed,
    /// not a plain hash.
    pub fn api_key_digest(&self, secret: &str) -> [u8; 32] {
        digest_with(&self.digest_key(), secret.as_bytes())
    }

    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_fail_next_load(&self) {
        self.hooks.fail_next_load.store(true, Ordering::SeqCst);
    }

    /// The next holder read errors, so the attention reader's failure path has a test.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_fail_next_refresh_read(&self) {
        self.hooks
            .fail_next_refresh_read
            .store(true, Ordering::SeqCst);
    }

    /// Fails the next `schema_version`, so a test reaches the readiness answer's dead store without a broken
    /// disk.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_fail_next_schema_version(&self) {
        self.hooks
            .fail_next_schema_version
            .store(true, Ordering::SeqCst);
    }

    /// Reads the canary through a read-only connection: no process lock, no `prepare`, no migration, no write,
    /// so it is safe beside a running router. With no canary it tries the key on the sealed rows, as the one-time
    /// path at the next start does, so a registry from before the canary fails the check under a key the start
    /// would refuse. `KeyMismatch` names `db_path`. Beside a stopped router SQLite leaves the `-wal` and `-shm`
    /// files a reader of a WAL database needs; the database file itself is never written.
    pub fn read_canary(db_path: &Path, key: &StorageKey) -> Result<CanaryRead, StoreError> {
        let Some(conn) = migrations::open_existing(db_path, BUSY_TIMEOUT)? else {
            return Ok(CanaryRead::NoRegistry);
        };
        prove_key(&conn, key, db_path)
    }

    /// The registry's `meta.schema_version` through a read-only connection, taking no lock, as `read_canary`
    /// reads; `None` for an absent file or one without the table, which the next start creates or migrates.
    pub fn read_schema_version(db_path: &Path) -> Result<Option<u32>, StoreError> {
        Ok(migrations::file_version(
            db_path,
            VERSION_WITHOUT_ROW,
            BUSY_TIMEOUT,
        )?)
    }

    /// Refuses a registry a newer build wrote with the start's own refusal, through a read-only connection taking no
    /// lock. An absent file passes, since the next start creates it.
    pub fn check_readable(db_path: &Path) -> Result<(), StoreError> {
        Ok(migrations::check_file(
            db_path,
            VERSION_WITHOUT_ROW,
            schema::SCHEMA_VERSION,
            BUSY_TIMEOUT,
        )?)
    }

    /// Resolved refresh attempts and registry changes older than `before_ms`, but for the rows their readers need
    /// whatever their age, in rowid slices of 1,000, one transaction per slice; the ledger's purge pass calls it
    /// once with the request cutoff.
    pub fn purge_history(&self, before_ms: i64) -> Result<PurgedHistory, StoreError> {
        Ok(PurgedHistory {
            attempts: self.purge_in_slices(PURGE_ATTEMPTS, before_ms, &[])?,
            changes: self.purge_in_slices(PURGE_CHANGES, before_ms, &[])?,
        })
    }

    /// The stored sets whose session last reported before a cutoff, in slices of 1,000, one transaction per slice: the
    /// kind `agents` before `agents_before_ms`, `None` keeping each, and the kinds `project` and `title` before
    /// `sessions_before_ms`. The ledger's purge pass calls it once.
    pub fn purge_companion_facts(
        &self,
        agents_before_ms: Option<i64>,
        sessions_before_ms: i64,
    ) -> Result<PurgedFacts, StoreError> {
        Ok(PurgedFacts {
            agents: match agents_before_ms {
                Some(before_ms) => {
                    self.purge_in_slices(PURGE_AGENT_FACTS, before_ms, &[&FactsKind::Agents])?
                }
                None => 0,
            },
            sessions: self.purge_in_slices(
                PURGE_SESSION_FACTS,
                sessions_before_ms,
                &[&FactsKind::Project, &FactsKind::Title],
            )?,
        })
    }

    /// `sql` binds the cutoff as `?1`, the slice's size as `?2` and `extra` from `?3` on.
    fn purge_in_slices(
        &self,
        sql: &str,
        before_ms: i64,
        extra: &[&dyn rusqlite::ToSql],
    ) -> Result<u64, StoreError> {
        let slice = HISTORY_SLICE as i64;
        let mut purged = 0;
        loop {
            let mut conn = self.conn.lock().expect(LOCK_POISONED);
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let bound: [&dyn rusqlite::ToSql; 2] = [&before_ms, &slice];
            let deleted = tx.execute(
                sql,
                rusqlite::params_from_iter(bound.into_iter().chain(extra.iter().copied())),
            )?;
            tx.commit()?;
            purged += deleted as u64;
            if deleted < HISTORY_SLICE {
                break;
            }
        }
        Ok(purged)
    }

    /// Transactions committed through `write_routing` and autocommits of the per-write methods since the
    /// store opened; the proof that a burst of reports lands in one commit.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_routing_commits(&self) -> u64 {
        self.hooks.routing_commits.load(Ordering::SeqCst)
    }

    /// Parks the next `write_routing` before its transaction until the hold is released, so a test can queue a
    /// burst of batches behind one wake.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_hold_next_routing_write(&self) -> Hold {
        arm(&self.hooks.routing_hold)
    }

    /// Before the connection's lock, so the store stays readable while the writer waits.
    #[cfg(any(test, feature = "debug-hooks"))]
    fn park_on_routing_hold(&self) {
        park_on(&self.hooks.routing_hold);
    }

    /// Sets the connection's busy timeout, so a test that holds the write lock from a second connection waits out a
    /// fraction of `BUSY_TIMEOUT` rather than the whole of it.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_set_busy_timeout(&self, timeout: Duration) -> Result<(), StoreError> {
        self.conn
            .lock()
            .expect(LOCK_POISONED)
            .busy_timeout(timeout)?;
        Ok(())
    }

    /// Takes of the connection mutex since start, so a test can prove a path pays none.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_lock_count(&self) -> u64 {
        self.conn.lock_count.load(Ordering::SeqCst)
    }

    /// Registry loads since start.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_load_count(&self) -> u64 {
        self.hooks.loads.load(Ordering::SeqCst)
    }

    /// Reads of an account row, its revision, an API key or an OAuth bundle since start: the reads a
    /// request's credential would pay, counted apart from the session memory and the routing writer, which take
    /// the same connection, so a test can prove a leg pays none.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_credential_reads(&self) -> u64 {
        self.hooks.credential_reads.load(Ordering::SeqCst)
    }

    /// Parks the next `api_key_credential` or `credentials` read before its lock until the hold drops, the shape of
    /// `debug_hold_next_routing_write`, so a test can rotate and reload under an acquisition in flight.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_hold_next_credential_read(&self) -> Hold {
        arm(&self.hooks.credential_hold)
    }

    /// Parks the next OAuth `credentials` read once it has read, after the connection's lock is released, until the
    /// hold drops: a commit and a reload the test lands under it postdate the credential the read answers.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_hold_after_next_credential_read(&self) -> Hold {
        arm(&self.hooks.credential_after_hold)
    }

    /// Parks the next fenced pool write before its lock until the hold drops, so a test can land another edit
    /// between a handler's read and its write.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_hold_next_pool_write(&self) -> Hold {
        arm(&self.hooks.pool_write_hold)
    }

    /// The key's counterpart of `debug_hold_next_pool_write`.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_hold_next_key_write(&self) -> Hold {
        arm(&self.hooks.key_write_hold)
    }

    /// Before the connection's lock, so the edit the test lands under the hold does not wait on the parked one.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub(crate) fn park_on_pool_write_hold(&self) {
        park_on(&self.hooks.pool_write_hold);
    }

    #[cfg(any(test, feature = "debug-hooks"))]
    pub(crate) fn park_on_key_write_hold(&self) {
        park_on(&self.hooks.key_write_hold);
    }

    /// Parks the next `load_registry` before its lock until the hold drops, so a test can ask the router for
    /// something while a reload's load runs.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_hold_next_load(&self) -> Hold {
        arm(&self.hooks.load_hold)
    }

    #[cfg(any(test, feature = "debug-hooks"))]
    fn count_credential_read(&self) {
        self.hooks.credential_reads.fetch_add(1, Ordering::SeqCst);
    }

    /// Before the connection's lock, so a write the test makes under the hold does not wait on the parked read.
    #[cfg(any(test, feature = "debug-hooks"))]
    fn park_on_credential_hold(&self) {
        park_on(&self.hooks.credential_hold);
    }

    #[cfg(any(test, feature = "debug-hooks"))]
    fn park_after_credential_read(&self) {
        park_on(&self.hooks.credential_after_hold);
    }

    /// Counted and parked before the connection's lock, so the store stays writable while the load waits.
    #[cfg(any(test, feature = "debug-hooks"))]
    fn enter_load(&self) {
        self.hooks.loads.fetch_add(1, Ordering::SeqCst);
        park_on(&self.hooks.load_hold);
    }

    /// Stamps `meta.schema_version`, so a test can stand for a registry a newer build wrote.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_set_schema_version(&self, version: u32) -> Result<(), StoreError> {
        self.conn.lock().expect(LOCK_POISONED).execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            params![version.to_string()],
        )?;
        Ok(())
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

/// Fails the call once a test armed `flag`, disarming it.
#[cfg(any(test, feature = "debug-hooks"))]
pub(crate) fn injected_failure(flag: &AtomicBool, message: &'static str) -> Result<(), StoreError> {
    if flag.swap(false, Ordering::SeqCst) {
        return Err(StoreError::Io(std::io::Error::other(message)));
    }
    Ok(())
}

#[cfg(any(test, feature = "debug-hooks"))]
const HOLD_POISONED: &str = "a test hold's mutex poisoned by a prior panic";

#[cfg(any(test, feature = "debug-hooks"))]
fn arm(slot: &Mutex<Option<Arc<HoldGate>>>) -> Hold {
    let gate = Arc::new(HoldGate::default());
    *slot.lock().expect(HOLD_POISONED) = Some(gate.clone());
    Hold::new(gate)
}

/// Waits on the hold armed in `slot`, if any, until the test releases it; the hold is taken, so it parks one call.
#[cfg(any(test, feature = "debug-hooks"))]
fn park_on(slot: &Mutex<Option<Arc<HoldGate>>>) {
    let armed = slot.lock().expect(HOLD_POISONED).take();
    if let Some(gate) = armed {
        gate.park();
    }
}

/// The park-until-released seam under every test hold: the held call parks on it until the test opens it. Public
/// under the feature so another crate's test seam parks on the same gate.
#[cfg(any(test, feature = "debug-hooks"))]
#[doc(hidden)]
#[derive(Default)]
pub struct HoldGate {
    parked: AtomicBool,
    released: Mutex<bool>,
    opened: Condvar,
}

/// A test's hold on the next call its arming names.
#[cfg(any(test, feature = "debug-hooks"))]
#[doc(hidden)]
pub struct Hold(Arc<HoldGate>);

#[cfg(any(test, feature = "debug-hooks"))]
impl Hold {
    /// The hold over `gate`, for a seam outside the store that parks on it, such as the ledger's writer-read hold.
    #[doc(hidden)]
    pub fn new(gate: Arc<HoldGate>) -> Hold {
        Hold(gate)
    }

    /// Whether the held call waits on the hold, which a test polls before it queues what should land behind.
    pub fn is_parked(&self) -> bool {
        self.0.is_parked()
    }

    pub fn release(self) {
        drop(self);
    }
}

/// Dropping the hold releases it too, so a test that fails with the call parked does not leave a blocking thread its
/// runtime waits on at shutdown.
#[cfg(any(test, feature = "debug-hooks"))]
impl Drop for Hold {
    fn drop(&mut self) {
        self.0.open();
    }
}

#[cfg(any(test, feature = "debug-hooks"))]
impl HoldGate {
    /// Marks the gate parked and blocks the calling thread until `open`.
    pub fn park(&self) {
        self.parked.store(true, Ordering::SeqCst);
        let mut released = self.released.lock().expect(HOLD_POISONED);
        while !*released {
            released = self.opened.wait(released).expect(HOLD_POISONED);
        }
    }

    /// Whether a call waits on the gate, which a test polls before it queues what should land behind.
    pub fn is_parked(&self) -> bool {
        self.parked.load(Ordering::SeqCst)
    }

    pub fn open(&self) {
        *self.released.lock().expect(HOLD_POISONED) = true;
        self.opened.notify_all();
    }
}

/// The registry opened for a key recovery: the process lock taken (so a running router refuses the open with
/// `Locked`), the schema checked but not migrated until `prepare_recovery` runs before the discard, and no canary
/// proof, since the key is the one thing being replaced.
pub struct RecoveryStore {
    conn: Connection,
    _lock: lock::ProcessLock,
}

/// How the configured key meets the registry a recovery opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMatch {
    /// The canary opens under the key.
    Matches,
    /// The registry has no canary and `readable_rows` sealed rows open under the key: the recovery seals the canary
    /// and discards nothing.
    OpensWithoutCanary { readable_rows: u64 },
    /// The canary does not open, but `readable_rows` sealed rows do: the canary is the corrupt part, and a backup
    /// restores it.
    CorruptCanary { readable_rows: u64 },
    /// Neither the canary nor any sealed row opens under the key.
    Differs,
}

impl Store {
    /// Takes the data directory's lock, then opens the registry file that is already there. A missing file is refused
    /// rather than created, since a recovery has nothing to discard in it and SQLite would create it at the umask's
    /// mode. A recovery run with `--owner` creates the lock file and hands it to that user before it calls this, and
    /// without `--owner` this open creates the file where it is missing, as every open does.
    pub fn open_for_recovery(db_path: &Path) -> Result<RecoveryStore, StoreError> {
        let data_dir = permissions::data_dir_of(db_path);
        let process_lock = lock::acquire(data_dir)?;
        let open = || -> Result<Connection, StoreError> {
            let conn = Connection::open_with_flags(
                db_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            // The key check, the counts and the prompt read the file as it is: a run that stops before the
            // preparation leaves the registry at the version it found, which the build that wrote it still opens.
            conn.busy_timeout(BUSY_TIMEOUT)?;
            check_schema(&conn)?;
            Ok(conn)
        };
        // A refused open keeps the lock file it created: a start that opened it meanwhile locks that file, and an
        // unlinked one would let a later start create and lock a second file at the path.
        let conn = open()?;
        Ok(RecoveryStore {
            conn,
            _lock: process_lock,
        })
    }
}

impl RecoveryStore {
    /// How `key` meets the registry. A canary that does not open proves nothing about the rows a discard would
    /// erase, so every sealed row is tried before the answer licenses one, never a sample: a canary that fails while
    /// rows open is corrupt. Without a canary the same scan proves the key, and the recovery seals the canary rather
    /// than discarding the rows it opens. A registry holding nothing sealed differs, so its recovery discards nothing
    /// and seals its canary.
    pub fn key_matches(&self, key: &StorageKey) -> Result<KeyMatch, StoreError> {
        let canary = stored_canary(&self.conn)?;
        if canary
            .as_deref()
            .is_some_and(|value| canary_matches(key, value))
        {
            return Ok(KeyMatch::Matches);
        }
        Ok(match (canary, readable_sealed_rows(&self.conn, key)?) {
            (_, 0) => KeyMatch::Differs,
            (Some(_), readable_rows) => KeyMatch::CorruptCanary { readable_rows },
            (None, readable_rows) => KeyMatch::OpensWithoutCanary { readable_rows },
        })
    }

    /// Seals the canary under `key` in one transaction, for a registry `key_matches` read as opening without one.
    pub fn seal_canary(&mut self, key: &StorageKey) -> Result<(), StoreError> {
        insert_canary(&mut self.conn, key)
    }

    pub fn sealed_counts(&self) -> Result<SealedCounts, StoreError> {
        sealed_counts_on(&self.conn)
    }

    /// Runs the schema and every migration above the stored version, the step `discard_sealed` needs before its
    /// statements meet the columns they name. A failure here moved nothing the discard would, so the caller says
    /// nothing was discarded.
    pub fn prepare_recovery(&mut self) -> Result<(), StoreError> {
        prepare(&mut self.conn, schema::MIGRATIONS)
    }

    /// `prepare_recovery` over a chain the test names, so a failing step can be driven whatever the build's own chain
    /// holds.
    #[cfg(test)]
    fn prepare_recovery_with(&mut self, migrations: &[(u32, &str)]) -> Result<(), StoreError> {
        prepare(&mut self.conn, migrations)
    }

    /// One transaction over a schema `prepare_recovery` moved: the canary, the credentials, the API-key secrets, the
    /// recipe bodies and the facts that a companion reported go, every oauth account reads `disconnected` and every
    /// api_key account `unpaired` under `cause` with its lifecycle generation and revision stepped, open and uncertain
    /// refresh attempts settle `failed` with the detail `disconnected`, and a canary is sealed under `key`.
    ///
    /// The operations page prints these statements as the sequence an operator runs by hand, so the two change
    /// together.
    pub fn discard_sealed(
        &mut self,
        key: &StorageKey,
        cause: StateCause,
    ) -> Result<SealedCounts, StoreError> {
        let canary = sealed_canary(key)?;
        // No `Store` clock exists here: the wall clock stamps the rows, as it does every write outside a test rig.
        let now = jiff::Timestamp::now().as_millisecond();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let counts = sealed_counts_on(&tx)?;
        tx.execute("DELETE FROM meta WHERE key = 'storage_key_canary'", [])?;
        tx.execute("DELETE FROM credentials", [])?;
        tx.execute("DELETE FROM api_key_secrets", [])?;
        tx.execute(
            "UPDATE keepalive_agents SET body_sealed = NULL, body_nonce = NULL WHERE body_sealed IS NOT NULL",
            [],
        )?;
        tx.execute("DELETE FROM companion_facts", [])?;
        tx.execute(
            concat!(
                "UPDATE accounts SET ",
                state_write!("CASE kind WHEN 'oauth' THEN 'disconnected' ELSE 'unpaired' END", "?2"),
                ", state_cause = ?1, lifecycle_generation = lifecycle_generation + 1, revision = revision + 1, \
                 updated_at = ?2"
            ),
            params![cause, now],
        )?;
        tx.execute(
            &format!(
                "UPDATE refresh_attempts SET resolved_at = ?1, outcome = 'failed', detail = 'disconnected', \
                 cause = 'disconnected' WHERE {PENDING_WHERE}"
            ),
            params![now],
        )?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('storage_key_canary', ?1)",
            params![canary],
        )?;
        tx.commit()?;
        Ok(counts)
    }
}

/// A recovery counts before it migrates, so a table the registry does not have yet counts nothing.
fn sealed_counts_on(conn: &Connection) -> Result<SealedCounts, StoreError> {
    let count = |table: &str, filter: &str| -> Result<u64, StoreError> {
        if !has_table(conn, table)? {
            return Ok(0);
        }
        let n: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table}{filter}"),
            [],
            |row| row.get(0),
        )?;
        Ok(n.max(0) as u64)
    };
    Ok(SealedCounts {
        api_key_accounts: count("accounts", " WHERE kind = 'api_key'")?,
        api_key_secrets: count("api_key_secrets", "")?,
        companion_facts: count("companion_facts", "")?,
        credentials: count("credentials", "")?,
        oauth_accounts: count("accounts", " WHERE kind = 'oauth'")?,
        open_refresh_attempts: count("refresh_attempts", &format!(" WHERE {PENDING_WHERE}"))?,
        recipe_bodies: count("keepalive_agents", " WHERE body_sealed IS NOT NULL")?,
    })
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
    if prove_key(conn, key, path)? == CanaryRead::Matches {
        return Ok(());
    }
    insert_canary(conn, key)
}

/// Proves `key` against the open registry at `path`: by its canary, else by its sealed rows, a registry sealed
/// before the canary existed. `KeyMismatch` names `path`.
fn prove_key(conn: &Connection, key: &StorageKey, path: &Path) -> Result<CanaryRead, StoreError> {
    let mismatch = || StoreError::KeyMismatch {
        path: path.to_path_buf(),
    };
    if let Some(value) = stored_canary(conn)? {
        return if canary_matches(key, &value) {
            Ok(CanaryRead::Matches)
        } else {
            Err(mismatch())
        };
    }
    match sealed_rows_open_under(conn, key)? {
        None => Ok(CanaryRead::Absent),
        Some(true) => Ok(CanaryRead::SealedRowMatches),
        Some(false) => Err(mismatch()),
    }
}

/// Seals the canary under `key` in its own transaction, keeping a canary another writer sealed first.
fn insert_canary(conn: &mut Connection, key: &StorageKey) -> Result<(), StoreError> {
    let canary = sealed_canary(key)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('storage_key_canary', ?1) ON CONFLICT(key) DO NOTHING",
        params![canary],
    )?;
    tx.commit()?;
    Ok(())
}

/// `None` for a registry without the row, or without `meta` at all.
fn stored_canary(conn: &Connection) -> Result<Option<String>, StoreError> {
    if !has_table(conn, "meta")? {
        return Ok(None);
    }
    Ok(migrations::meta_value(conn, "storage_key_canary")?)
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
    /// The plaintext is a credential, a secret, a recipe body or a set of reported facts, so it is wiped as soon as the
    /// open has proved the key.
    fn opens_under(&self, key: &StorageKey) -> bool {
        crypto::open(key, &self.aad, &self.nonce, &self.sealed)
            .map(Zeroizing::new)
            .is_ok()
    }
}

/// How many sealed rows of each kind the proof against a registry without a canary reads. A corrupt row cannot
/// stop a start whose key the rows behind it inside the bound prove, and a wrong key, which opens none, costs a
/// bounded read rather than every row of a large registry at each start and each `check-config`.
///
/// The bound leaves one shape of a registry without a canary refusing the right key at the start: one whose readable
/// credential bundles, API-key secrets, recipe bodies and sets of reported facts all sit past the 32nd of their kind.
/// `recover-key` scans every sealed row, so on such a registry it seals the canary under the key it opens and discards
/// nothing, the way out.
const PROOF_ROWS_PER_KIND: i64 = 32;

/// Whether the key opens any of the first `PROOF_ROWS_PER_KIND` sealed rows of each kind in row order (credential
/// bundles, API-key secrets, recipe bodies and sets of reported facts), or `None` when the registry holds nothing
/// sealed. A row that does not open is no proof of a wrong key, since the runtime survives a corrupt row too, and a
/// wrong key opens none of them. `read_canary` meets the file as it is, without `prepare`, so a table the registry
/// does not have yet holds nothing sealed rather than failing the read.
fn sealed_rows_open_under(conn: &Connection, key: &StorageKey) -> Result<Option<bool>, StoreError> {
    let mut seen = false;
    let mut opened = false;
    for kind in &SEALED_KINDS {
        if !has_table(conn, kind.table)? {
            continue;
        }
        let mut statement = conn.prepare(&format!("{} ORDER BY rowid LIMIT ?1", kind.sql))?;
        let mut rows = statement.query(params![PROOF_ROWS_PER_KIND])?;
        while let Some(row) = rows.next()? {
            let sealed = (kind.row_of)(row)?;
            seen = true;
            opened = opened || sealed.opens_under(key);
        }
    }
    Ok(seen.then_some(opened))
}

type RowOf = fn(&rusqlite::Row<'_>) -> rusqlite::Result<SealedRow>;

/// One kind of sealed row, a credential bundle, an API-key secret, a recipe body or a set of reported facts: its
/// table, the statement over it and the mapper to the row with its associated data.
struct SealedKind {
    table: &'static str,
    sql: &'static str,
    row_of: RowOf,
}

/// Each kind of sealed row: credential bundles, API-key secrets, recipe bodies and sets of reported facts. A recovery
/// reads every row of each kind, and the start's sample the first `PROOF_ROWS_PER_KIND` of each.
const SEALED_KINDS: [SealedKind; 4] = [
    SealedKind {
        table: "credentials",
        sql: "SELECT account_id, generation, nonce, sealed FROM credentials",
        row_of: credential_row,
    },
    SealedKind {
        table: "api_key_secrets",
        sql: "SELECT account_id, nonce, sealed FROM api_key_secrets",
        row_of: api_key_row,
    },
    SealedKind {
        table: "keepalive_agents",
        sql: "SELECT key_id, key_generation, session_id, agent_id, body_nonce, body_sealed FROM keepalive_agents \
              WHERE body_sealed IS NOT NULL AND body_nonce IS NOT NULL",
        row_of: recipe_row,
    },
    SealedKind {
        table: "companion_facts",
        sql: "SELECT key_id, key_generation, session_id, kind, agent_id, facts_nonce, facts_sealed \
              FROM companion_facts",
        row_of: facts_row,
    },
];

fn credential_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SealedRow> {
    Ok(SealedRow {
        aad: credentials::credential_aad(&row.get::<_, String>(0)?, row.get(1)?),
        nonce: row.get(2)?,
        sealed: row.get(3)?,
    })
}

fn api_key_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SealedRow> {
    Ok(SealedRow {
        aad: credentials::api_key_aad(&row.get::<_, String>(0)?),
        nonce: row.get(1)?,
        sealed: row.get(2)?,
    })
}

fn recipe_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SealedRow> {
    Ok(SealedRow {
        aad: engine_state::keepalive_aad(
            &row.get::<_, String>(0)?,
            row.get(1)?,
            &row.get::<_, String>(2)?,
            &row.get::<_, String>(3)?,
        ),
        nonce: row.get(4)?,
        sealed: row.get(5)?,
    })
}

/// A stored kind word outside `FactsKind` fails the row's conversion, so the proof and the scan fail rather than
/// judge a row under a key they cannot name.
fn facts_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SealedRow> {
    let key = engine_state::FactsKey {
        key_id: row.get(0)?,
        key_generation: row.get(1)?,
        session_id: row.get(2)?,
        kind: row.get(3)?,
        agent_id: row.get(4)?,
    };
    Ok(SealedRow {
        aad: engine_state::facts_aad(&key),
        nonce: row.get(5)?,
        sealed: row.get(6)?,
    })
}

/// How many sealed rows open under `key`, over every row of each kind: credential bundles, API-key secrets, recipe
/// bodies and sets of reported facts. A recovery discards what this finds unreadable, so it reads the whole registry
/// once rather than the start's sample. A table the registry does not have yet holds nothing sealed.
fn readable_sealed_rows(conn: &Connection, key: &StorageKey) -> Result<u64, StoreError> {
    let mut readable = 0;
    for kind in &SEALED_KINDS {
        if !has_table(conn, kind.table)? {
            continue;
        }
        let mut statement = conn.prepare(kind.sql)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if (kind.row_of)(row)?.opens_under(key) {
                readable += 1;
            }
        }
    }
    Ok(readable)
}

/// A read-only connection to the file at `path` that takes no lock and writes nothing, waiting up to `busy_timeout`
/// on another writer. The ledger opens its readers through it too, so the two files' read-only opens cannot drift.
pub fn open_read_only(path: &Path, busy_timeout: Duration) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(busy_timeout)?;
    Ok(conn)
}

/// The canary sealed under `key`, the nonce `crypto::seal` drew stored as the value's first bytes.
fn sealed_canary(key: &StorageKey) -> Result<String, StoreError> {
    let (nonce, sealed) = crypto::seal(key, CANARY_AAD, CANARY_PLAINTEXT)?;
    Ok(base64::engine::general_purpose::STANDARD.encode([nonce, sealed].concat()))
}

/// Answers the canary's plaintext. A value that is not base64, or too short to hold a nonce, is refused like a
/// wrong key: no key this build holds sealed it.
fn open_canary(key: &StorageKey, value: &str) -> Result<Vec<u8>, StoreError> {
    let joined = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| StoreError::Crypto)?;
    let (nonce, sealed) = joined
        .split_at_checked(crypto::NONCE_LEN)
        .ok_or(StoreError::Crypto)?;
    crypto::open(key, CANARY_AAD, nonce, sealed)
}

/// Runs `SCHEMA`, the baseline at version 1, then the steps above the stored `schema_version`.
fn prepare(conn: &mut Connection, steps: &[(u32, &str)]) -> Result<(), StoreError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
    // A per-connection setting, never written to the database file, so it costs nothing to set before the
    // version is even known.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let current = check_schema(conn)?;
    // WAL lets the router task's reads run beside an admin write. Unlike `foreign_keys` this rewrites the database
    // header, so it waits behind the version check too.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // Every change to the registry's own rows (accounts, credentials, keys, members, pools, refresh attempts, rules,
    // secrets) is on disk before its commit returns. `routing_commit` alone relaxes, for routing state whose last
    // moments a power loss may take: the next requests rebuild it, and an operator makes a lost cooldown clear or
    // session release again.
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(migrations::apply(conn, schema::SCHEMA, current, steps)?)
}

/// What a registry's `meta` without its `schema_version` row reads as: version 1, the baseline `schema::SCHEMA`
/// creates.
const VERSION_WITHOUT_ROW: u32 = 1;

/// The refusal `prepare` makes before it writes anything, answering the stored version.
fn check_schema(conn: &Connection) -> Result<u32, StoreError> {
    Ok(migrations::check_version(
        conn,
        VERSION_WITHOUT_ROW,
        schema::SCHEMA_VERSION,
    )?)
}

/// A connection that has never run `prepare` has no `meta` table yet, which reads as version 0 so the check
/// above runs before `schema::SCHEMA` creates it.
fn read_version(conn: &Connection) -> Result<u32, StoreError> {
    Ok(migrations::stored_version(conn, VERSION_WITHOUT_ROW)?)
}

/// A value that does not parse is refused rather than read as 0, which would let every token signed before the first
/// persistent logout verify again.
fn read_admin_session_epoch(conn: &Connection) -> Result<u64, StoreError> {
    let Some(stored) = migrations::meta_value(conn, "admin_session_epoch")? else {
        return Ok(0);
    };
    stored.parse().map_err(|error| {
        StoreError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(error),
        ))
    })
}

/// Shared by `Store::api_key_digest` and the router task's in-memory snapshot, so both digest a presented
/// secret with the exact same function.
pub fn digest_with(key: &[u8; 32], secret: &[u8]) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC-SHA256 accepts a 32 byte key");
    mac.update(secret);
    mac.finalize().into_bytes().into()
}

/// `ms` as `YYYY-MM-DD HH:MM:SS UTC`, the one spelling the ledger's, the keepalive's and the usage poll's log lines
/// print, or the raw milliseconds for one out of range.
pub fn utc_stamp(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|at| at.strftime("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|_| ms.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The wall clock, for a case that reads no stamp.
    fn wall() -> Clock {
        Arc::new(|| jiff::Timestamp::now().as_millisecond())
    }

    fn canary_of(store: &Store) -> Option<String> {
        stored_canary(&store.conn.lock().unwrap()).unwrap()
    }

    /// Account `a` holding one committed credential bundle, the first sealed row the one-time path proves.
    fn with_a_credential(store: &Store) {
        with_a_credential_on(store, "a");
    }

    fn with_a_credential_on(store: &Store, id: &str) {
        store
            .insert_account(&account(id, AccountKind::Oauth))
            .unwrap();
        store
            .commit_credentials(
                id,
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
    fn the_admin_session_epoch_reads_zero_on_a_new_registry_and_a_bump_holds_across_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        let store = Store::open(&db, &key, wall()).unwrap();
        assert_eq!(store.admin_session_epoch().unwrap(), 0);
        assert_eq!(store.bump_admin_session_epoch().unwrap(), 1);
        assert_eq!(store.admin_session_epoch().unwrap(), 1);
        drop(store);
        let store = Store::open(&db, &key, wall()).unwrap();
        assert_eq!(store.admin_session_epoch().unwrap(), 1);
        assert_eq!(store.bump_admin_session_epoch().unwrap(), 2);
    }

    #[test]
    fn a_registry_sealed_under_one_key_refuses_another_key_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        drop(Store::open(&db, &key, wall()).unwrap());
        match Store::open(&db, &StorageKey::generate(), wall()) {
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
            Store::open(&db, &key, wall()).is_ok(),
            "the refusal left the canary as it was"
        );
    }

    #[test]
    fn a_registry_sealed_before_the_canary_proves_the_key_against_its_first_sealed_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        let store = Store::open(&db, &key, wall()).unwrap();
        with_a_credential(&store);
        store.debug_drop_canary().unwrap();
        assert_eq!(canary_of(&store), None);
        drop(store);
        match Store::open(&db, &StorageKey::generate(), wall()) {
            Err(StoreError::KeyMismatch { path }) => assert_eq!(path, db),
            Err(other) => panic!("refused for another reason: {other}"),
            Ok(_) => panic!("opened under another key"),
        }
        let store = Store::open(&db, &key, wall()).unwrap();
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
            state: AccountState::Unpaired,
            ..AccountRow::for_tests(id, kind)
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
        match Store::open(db, &other, wall()) {
            Err(StoreError::KeyMismatch { path }) => assert_eq!(path, db),
            Err(other) => panic!("refused for another reason: {other}"),
            Ok(_) => panic!("opened under another key"),
        }
        assert_eq!(
            Store::read_canary(db, key).unwrap(),
            CanaryRead::SealedRowMatches
        );
        let store = Store::open(db, key, wall()).unwrap();
        let value = canary_of(&store).expect("the start under the right key sealed the canary");
        assert_eq!(open_canary(key, &value).unwrap(), CANARY_PLAINTEXT);
    }

    #[test]
    fn a_registry_whose_only_sealed_row_is_an_api_key_secret_proves_the_key_against_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        let store = Store::open(&db, &key, wall()).unwrap();
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
    fn a_registry_whose_only_sealed_rows_are_recipe_bodies_proves_the_key_against_them() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        let store = Store::open(&db, &key, wall()).unwrap();
        store
            .save_engine_state(&EngineBatch {
                warming_agents: vec![KeepaliveAgentRow {
                    key_id: "desktop".into(),
                    key_generation: 1,
                    session_id: "s-1".into(),
                    agent_id: "main".into(),
                    record_version: 1,
                    payload: "{}".into(),
                    body: None,
                    body_write: engine_state::BodyWrite::Seal(b"{\"messages\":[]}".to_vec()),
                    headers: "[]".into(),
                    body_size: 15,
                    dispatched_ms: None,
                    fast: false,
                    updated_at: 1_800_000_000_000,
                }],
                ..EngineBatch::default()
            })
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
        let store = Store::open(&db, &key, wall()).unwrap();
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
        let store = Store::open(&only_corrupt, &key, wall()).unwrap();
        with_a_credential(&store);
        store.debug_corrupt_credentials("a").unwrap();
        store.debug_drop_canary().unwrap();
        drop(store);
        match Store::read_canary(&only_corrupt, &key) {
            Err(StoreError::KeyMismatch { path }) => assert_eq!(path, only_corrupt),
            answer => panic!("{answer:?}"),
        }
        assert!(matches!(
            Store::open(&only_corrupt, &key, wall()),
            Err(StoreError::KeyMismatch { .. })
        ));
    }

    #[test]
    fn a_corrupt_first_credential_leaves_the_proof_to_an_intact_credential_behind_it() {
        let dir = tempfile::tempdir().unwrap();
        let key = StorageKey::generate();

        let db = dir.path().join("router.sqlite");
        let store = Store::open(&db, &key, wall()).unwrap();
        with_a_credential_on(&store, "a");
        with_a_credential_on(&store, "b");
        store.debug_corrupt_credentials("a").unwrap();
        store.debug_drop_canary().unwrap();
        drop(store);
        assert_the_sealed_rows_prove_the_key(&db, &key);

        let all_corrupt = dir.path().join("all-corrupt.sqlite");
        let store = Store::open(&all_corrupt, &key, wall()).unwrap();
        with_a_credential_on(&store, "a");
        with_a_credential_on(&store, "b");
        store.debug_corrupt_credentials("a").unwrap();
        store.debug_corrupt_credentials("b").unwrap();
        store.debug_drop_canary().unwrap();
        drop(store);
        match Store::read_canary(&all_corrupt, &key) {
            Err(StoreError::KeyMismatch { path }) => assert_eq!(path, all_corrupt),
            answer => panic!("{answer:?}"),
        }
        match Store::open(&all_corrupt, &key, wall()) {
            Err(StoreError::KeyMismatch { path }) => assert_eq!(path, all_corrupt),
            Err(other) => panic!("refused for another reason: {other}"),
            Ok(_) => panic!("opened with no sealed row to prove the key by"),
        }
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
            drop(Store::open(&db, &key, wall()).unwrap());
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
            match Store::open(&db, &key, wall()) {
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

        // An empty file is what a first open leaves when it dies before `prepare`, with none of the tables the
        // sealed-row proof reads; a v1 registry is the oldest shape the proof meets as it is.
        let empty_file = dir.path().join("empty-file.sqlite");
        std::fs::write(&empty_file, b"").unwrap();
        let v1 = dir.path().join("v1.sqlite");
        Connection::open(&v1)
            .unwrap()
            .execute_batch(schema::SCHEMA)
            .unwrap();
        let unsealed = dir.path().join("unsealed.sqlite");
        let store = Store::open(&unsealed, &key, wall()).unwrap();
        store.debug_drop_canary().unwrap();
        drop(store);
        let legacy = dir.path().join("legacy.sqlite");
        let store = Store::open(&legacy, &key, wall()).unwrap();
        with_a_credential(&store);
        store.debug_drop_canary().unwrap();
        drop(store);
        let sealed = dir.path().join("sealed.sqlite");
        drop(Store::open(&sealed, &key, wall()).unwrap());

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
        let store = Store::open(&db, &key, wall()).unwrap();
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

    #[test]
    fn a_registry_without_a_canary_reads_the_count_of_the_rows_the_key_opens_until_its_canary_is_sealed(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        let store = Store::open(&db, &key, wall()).unwrap();
        with_a_credential_on(&store, "a");
        with_a_credential_on(&store, "b");
        store.debug_corrupt_credentials("a").unwrap();
        store
            .insert_account(&account("vendor", AccountKind::ApiKey))
            .unwrap();
        store
            .set_api_key_secret("vendor", &Secret::new("sk-ant-test-not-a-key"))
            .unwrap();
        store.debug_drop_canary().unwrap();
        // Dropped first, since the recovery's open takes the process lock the store holds.
        drop(store);
        let mut recovery = Store::open_for_recovery(&db).unwrap();
        assert_eq!(
            recovery.key_matches(&key).unwrap(),
            KeyMatch::OpensWithoutCanary { readable_rows: 2 },
            "the bundle of b and the secret open, the corrupt bundle of a does not"
        );
        recovery.seal_canary(&key).unwrap();
        assert_eq!(recovery.key_matches(&key).unwrap(), KeyMatch::Matches);
        drop(recovery);
        assert_eq!(Store::read_canary(&db, &key).unwrap(), CanaryRead::Matches);
    }

    #[test]
    fn a_key_that_opens_the_canary_matches_and_a_key_that_opens_nothing_differs_with_or_without_a_canary(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        let other = StorageKey::generate();
        let store = Store::open(&db, &key, wall()).unwrap();
        with_a_credential(&store);
        drop(store);
        let recovery = Store::open_for_recovery(&db).unwrap();
        assert_eq!(recovery.key_matches(&key).unwrap(), KeyMatch::Matches);
        assert_eq!(recovery.key_matches(&other).unwrap(), KeyMatch::Differs);
        drop(recovery);
        Store::open(&db, &key, wall())
            .unwrap()
            .debug_drop_canary()
            .unwrap();
        let recovery = Store::open_for_recovery(&db).unwrap();
        assert_eq!(recovery.key_matches(&other).unwrap(), KeyMatch::Differs);
    }

    /// On disk: `synchronous` on an in-memory registry syncs nothing, so only a file proves the setting.
    fn on_disk(dir: &tempfile::TempDir) -> Store {
        Store::open(
            &dir.path().join("router.sqlite"),
            &StorageKey::generate(),
            wall(),
        )
        .unwrap()
    }

    fn pragma(store: &Store, name: &str) -> i64 {
        store
            .conn
            .lock()
            .unwrap()
            .pragma_query_value(None, name, |row| row.get(0))
            .unwrap()
    }

    fn cooldown(account_id: &str, scope: &str) -> CooldownRow {
        CooldownRow {
            account_id: account_id.into(),
            scope: CooldownScope::parse(scope).unwrap(),
            until: 10_000,
            set_at: 0,
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: None,
            incarnation: 1,
            spend_share: None,
        }
    }

    #[test]
    fn a_routing_write_commits_at_normal_and_leaves_full_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = on_disk(&dir);
        store
            .insert_account(&account("a", AccountKind::ApiKey))
            .unwrap();
        ROUTING_COMMIT_SAW.take();

        store.upsert_cooldown(&cooldown("a", "account")).unwrap();
        assert_eq!(
            ROUTING_COMMIT_SAW.take(),
            Some(1),
            "NORMAL under a per-write method"
        );
        assert_eq!(pragma(&store, "synchronous"), 2, "FULL after it");
        store
            .write_routing(|tx| tx.upsert_cooldown(&cooldown("a", "model:claude-opus-5")))
            .unwrap();
        assert_eq!(
            ROUTING_COMMIT_SAW.take(),
            Some(1),
            "NORMAL under a wake's transaction"
        );
        assert_eq!(pragma(&store, "synchronous"), 2, "FULL after it");
        assert!(store
            .upsert_cooldown(&cooldown("ghost", "account"))
            .is_err());
        assert_eq!(
            ROUTING_COMMIT_SAW.take(),
            Some(1),
            "NORMAL under a refused write"
        );
        assert_eq!(
            pragma(&store, "synchronous"),
            2,
            "FULL after the refusal too"
        );
    }

    fn pool(id: &str) -> PoolRow {
        PoolRow {
            strategy: Strategy::Priority,
            prefer_subscription: false,
            ..PoolRow::for_tests(id)
        }
    }

    #[test]
    fn a_registry_write_commits_at_full_throughout() {
        let dir = tempfile::tempdir().unwrap();
        let store = on_disk(&dir);
        store.insert_pool(&pool("p"), "test").unwrap();
        ROUTING_COMMIT_SAW.take();

        with_a_credential(&store);
        assert_eq!(
            ROUTING_COMMIT_SAW.take(),
            None,
            "no credential write relaxes"
        );
        assert_eq!(
            pragma(&store, "synchronous"),
            2,
            "FULL after the credential commit"
        );
        let read = store.pool("p").unwrap().unwrap();
        store
            .update_pool(
                &PoolRow {
                    enabled: false,
                    ..read.clone()
                },
                read.updated_at,
                "test",
            )
            .unwrap();
        assert_eq!(ROUTING_COMMIT_SAW.take(), None, "no pool edit relaxes");
        assert_eq!(
            pragma(&store, "synchronous"),
            2,
            "FULL after the pool's disable"
        );
    }

    fn failure(code: std::ffi::c_int) -> StoreError {
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        ))
    }

    #[test]
    fn the_accounts_and_their_extras_come_from_one_take_of_the_store() {
        let store = Store::open_in_memory(&StorageKey::generate(), wall()).unwrap();
        for id in ["a", "b"] {
            store
                .insert_account(&account(id, AccountKind::Oauth))
                .unwrap();
        }
        let before = store.debug_lock_count();
        let pairs = store.accounts_with_extras().unwrap();
        assert_eq!(store.debug_lock_count() - before, 1);
        assert_eq!(
            pairs
                .iter()
                .map(|(row, extras)| (row.id.as_str(), extras.term.account_id.as_str()))
                .collect::<Vec<_>>(),
            [("a", "a"), ("b", "b")]
        );
    }

    #[test]
    fn a_file_the_database_cannot_read_or_write_is_database_wide_and_a_refused_row_is_not() {
        let store = Store::open_in_memory(&StorageKey::generate(), wall()).unwrap();
        let refused = store
            .upsert_cooldown(&cooldown("ghost", "account"))
            .unwrap_err();
        assert!(!refused.is_database_wide(), "{refused}");
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_CANTOPEN,
            rusqlite::ffi::SQLITE_CORRUPT,
            rusqlite::ffi::SQLITE_FULL,
            rusqlite::ffi::SQLITE_IOERR_WRITE,
            rusqlite::ffi::SQLITE_MISUSE,
            rusqlite::ffi::SQLITE_NOTADB,
            rusqlite::ffi::SQLITE_READONLY,
            rusqlite::ffi::SQLITE_READONLY_DBMOVED,
        ] {
            let error = failure(code);
            assert!(error.is_database_wide(), "{code}: {error}");
        }
        for code in [
            rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER,
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
            rusqlite::ffi::SQLITE_TOOBIG,
        ] {
            let error = failure(code);
            assert!(!error.is_database_wide(), "{code}: {error}");
        }
    }

    #[test]
    fn a_failing_recovery_preparation_leaves_the_sealed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("router.sqlite");
        let key = StorageKey::generate();
        let store = Store::open(&db, &key, wall()).unwrap();
        with_a_credential(&store);
        // Dropped first, since the recovery's open takes the process lock the store holds.
        drop(store);
        let mut recovery = Store::open_for_recovery(&db).unwrap();
        let error = recovery
            .prepare_recovery_with(&[(schema::SCHEMA_VERSION + 1, "THIS IS NOT SQL;")])
            .unwrap_err();
        assert!(matches!(error, StoreError::Sqlite(_)), "{error:?}");
        assert_eq!(recovery.sealed_counts().unwrap().credentials, 1);
        drop(recovery);
        let store = Store::open(&db, &key, wall()).unwrap();
        assert_eq!(store.schema_version().unwrap(), schema::SCHEMA_VERSION);
        assert!(
            store.credentials("a").unwrap().is_some(),
            "the bundle still opens under the key"
        );
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
    fn a_key_written_as_its_hex_and_a_newline_reads_back_as_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("storage.key");
        let key = StorageKey::generate();
        let hex = key.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "lowercase hex"
        );
        std::fs::write(&path, format!("{}\n", hex.as_str())).unwrap();
        let read = StorageKey::from_file(&path).unwrap();
        assert_eq!(read.to_hex(), hex);
        assert_eq!(read.0, key.0);
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
        let error = prepare(&mut conn, schema::MIGRATIONS).unwrap_err();
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

    #[test]
    fn an_open_on_a_fresh_path_creates_the_registry_file_at_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        on_disk(&dir);
        let mode = std::fs::metadata(dir.path().join("router.sqlite"))
            .unwrap()
            .permissions();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.mode() & 0o777, 0o600);
    }

    #[test]
    fn a_log_lines_instant_prints_in_utc_with_the_zone_named_or_raw_when_out_of_range() {
        assert_eq!(utc_stamp(1_760_961_787_123), "2025-10-20 12:03:07 UTC");
        assert_eq!(utc_stamp(i64::MAX), i64::MAX.to_string());
    }
}
