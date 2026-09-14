//! SQLite registry and encrypted credentials.

mod accounts;
mod credentials;
mod lock;
mod schema;

pub mod crypto;

use std::path::Path;
use std::sync::Mutex;

use rand::Rng;
use rusqlite::Connection;

pub use accounts::{AccountKind, AccountRow, AccountState, Identity};
pub use credentials::{
    AttemptOutcome, CredentialBundle, LastAttempt, NewBundle, PendingAttempt, Secret,
};

// A poisoned store mutex means a prior panic happened while a write was in flight; the store's
// on-disk state may be half-written, so continuing past that point is not safe.
const LOCK_POISONED: &str = "store connection mutex poisoned by a prior panic";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("account {0} not found")]
    AccountNotFound(String),
    #[error("a refresh attempt is already pending for this account")]
    AttemptPending,
    #[error("sealed credential could not be decrypted")]
    Crypto,
    #[error("storage key file must hold exactly 32 raw bytes or 64 hex characters")]
    InvalidKey,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("the data directory is locked by another process")]
    Locked,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("lifecycle or credential generation is stale")]
    Stale,
}

/// The AES-256 key sealing every credential and API-key secret in the store.
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
}

impl Store {
    /// Opens (creating if needed) the on-disk registry: directory mode 0700, the process lock, then
    /// `router.sqlite` at mode 0600.
    pub fn open(dir: &Path, key: &StorageKey) -> Result<Store, StoreError> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let process_lock = lock::acquire(dir)?;

        let db_path = dir.join("router.sqlite");
        if !db_path.exists() {
            // SQLite creates new database files honoring the process umask; create it ourselves first
            // so credentials never sit in a file readable by other local users, even briefly.
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(&db_path)?;
        }

        let conn = Connection::open(&db_path)?;
        conn.execute_batch(schema::SCHEMA)?;
        Ok(Store {
            conn: Mutex::new(conn),
            key: key.clone(),
            _lock: Some(process_lock),
        })
    }

    /// Opens an in-memory registry for tests: no directory, no lock, no on-disk file.
    pub fn open_in_memory(key: &StorageKey) -> Result<Store, StoreError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(schema::SCHEMA)?;
        Ok(Store {
            conn: Mutex::new(conn),
            key: key.clone(),
            _lock: None,
        })
    }
}
