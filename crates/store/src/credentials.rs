//! Encrypted credential bundles, API-key secrets, and refresh-attempt bookkeeping.

use rusqlite::{params, OptionalExtension};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{crypto, StorageKey, Store, StoreError, LOCK_POISONED, PENDING_WHERE};

/// A secret string that zeroizes its buffer on drop and never prints its value.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: &str) -> Secret {
        Secret(value.to_owned())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(***)")
    }
}

// Wire shape sealed inside a credentials row's `sealed` blob; everything else about the bundle is a
// plain column.
#[derive(serde::Serialize, serde::Deserialize)]
struct SealedTokens {
    access_token: String,
    refresh_token: Option<String>,
}

#[derive(Debug)]
pub struct CredentialBundle {
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
    pub issued_at: i64,
    pub expires_at: i64,
    pub refresh_expires_at: Option<i64>,
    pub scopes: String,
    pub generation: u64,
    /// The account lifecycle this bundle was committed under. A holder that captured the bundle before a
    /// slow request compares it to tell a rotation of the same slot from a disconnect and a re-pair.
    pub lifecycle_generation: u64,
}

#[derive(Debug)]
pub struct NewBundle {
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
    pub issued_at: i64,
    pub expires_at: i64,
    pub refresh_expires_at: Option<i64>,
    pub scopes: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    Committed,
    Failed(String),
    /// The refresh may or may not have rotated the token upstream. The string says which way it went
    /// uncertain, because that is all an operator has to act on before re-pairing the slot.
    Uncertain(String),
}

#[derive(Debug, Clone)]
pub struct PendingAttempt {
    pub id: u64,
    pub account_id: String,
    pub credential_generation: u64,
    pub lifecycle_generation: u64,
    pub started_at: i64,
}

#[derive(Debug, Clone)]
pub struct LastAttempt {
    pub resolved_at: Option<i64>,
    pub outcome: Option<String>,
    pub detail: Option<String>,
}

// (generation, sealed, nonce, issued_at, expires_at, refresh_expires_at, scopes, lifecycle_generation)
type CredentialRow = (i64, Vec<u8>, Vec<u8>, i64, i64, Option<i64>, String, i64);

fn credential_aad(account_id: &str, generation: i64) -> String {
    format!("{account_id}:{generation}")
}

fn api_key_aad(account_id: &str) -> String {
    format!("{account_id}:api_key")
}

/// Whether the attempt row is there at all, which is what separates a verdict that arrived too late from
/// one that names an attempt nobody ever opened. `account_id` narrows it where the caller has one to check
/// against.
fn attempt_exists(
    conn: &rusqlite::Connection,
    attempt_id: u64,
    account_id: Option<&str>,
) -> Result<bool, StoreError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM refresh_attempts WHERE id = ?1 AND (?2 IS NULL OR account_id = ?2))",
        params![attempt_id as i64, account_id],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

/// Seals and stores an API-key secret under the caller's connection, so a creation or a verified edit can
/// carry it inside the same transaction as the row it belongs to. Bumps the generation on every write,
/// including the first, so a verdict formed against generation 1 is never confused with the row before it
/// existed.
pub(crate) fn store_api_key_secret(
    conn: &rusqlite::Connection,
    key: &StorageKey,
    account_id: &str,
    secret: &Secret,
) -> Result<(), StoreError> {
    let (nonce, sealed) = crypto::seal(key, &api_key_aad(account_id), secret.expose().as_bytes())?;
    conn.execute(
        "INSERT INTO api_key_secrets (account_id, sealed, nonce) VALUES (?1, ?2, ?3) \
         ON CONFLICT(account_id) DO UPDATE SET sealed = excluded.sealed, nonce = excluded.nonce, generation = api_key_secrets.generation + 1",
        params![account_id, sealed, nonce],
    )?;
    Ok(())
}

impl Store {
    pub fn credentials(&self, account_id: &str) -> Result<Option<CredentialBundle>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let row: Option<CredentialRow> = conn
            .query_row(
                "SELECT generation, sealed, nonce, issued_at, expires_at, refresh_expires_at, scopes, lifecycle_generation \
                 FROM credentials WHERE account_id = ?1",
                params![account_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
            )
            .optional()?;
        let Some((
            generation,
            sealed,
            nonce,
            issued_at,
            expires_at,
            refresh_expires_at,
            scopes,
            lifecycle_generation,
        )) = row
        else {
            return Ok(None);
        };

        let plaintext = crypto::open(
            &self.key,
            &credential_aad(account_id, generation),
            &nonce,
            &sealed,
        )?;
        let tokens: SealedTokens = serde_json::from_slice(&plaintext)?;

        Ok(Some(CredentialBundle {
            access_token: Secret::new(&tokens.access_token),
            refresh_token: tokens.refresh_token.map(|t| Secret::new(&t)),
            issued_at,
            expires_at,
            refresh_expires_at,
            scopes,
            generation: generation as u64,
            lifecycle_generation: lifecycle_generation as u64,
        }))
    }

    /// Commits a new credential generation in one transaction: the lifecycle and credential generations
    /// are checked against the account's current state (`Stale` on mismatch), a missing refresh token is
    /// folded forward from the previous plaintext, and `attempt_id`, if given, is resolved as `Committed`
    /// here too, so a crash between the two writes cannot leave a committed bundle behind a pending attempt.
    pub fn commit_credentials(
        &self,
        account_id: &str,
        lifecycle_generation: u64,
        expected_generation: Option<u64>,
        attempt_id: Option<u64>,
        bundle: &NewBundle,
    ) -> Result<u64, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let account_lifecycle: i64 = tx
            .query_row(
                "SELECT lifecycle_generation FROM accounts WHERE id = ?1",
                params![account_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::AccountNotFound(account_id.to_string()))?;
        if account_lifecycle as u64 != lifecycle_generation {
            return Err(StoreError::Stale);
        }

        let existing: Option<(i64, Vec<u8>, Vec<u8>)> = tx
            .query_row(
                "SELECT generation, sealed, nonce FROM credentials WHERE account_id = ?1",
                params![account_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        let current_generation = existing
            .as_ref()
            .map(|(generation, _, _)| *generation as u64);
        if current_generation != expected_generation {
            return Err(StoreError::Stale);
        }

        let refresh_token = match &bundle.refresh_token {
            Some(secret) => Some(secret.expose().to_owned()),
            // A refresh response that omits the refresh token means "unchanged", not "cleared";
            // decrypt the row being replaced to carry its refresh token forward.
            None => match &existing {
                Some((generation, sealed, nonce)) => {
                    let plaintext = crypto::open(
                        &self.key,
                        &credential_aad(account_id, *generation),
                        nonce,
                        sealed,
                    )?;
                    let previous: SealedTokens = serde_json::from_slice(&plaintext)?;
                    previous.refresh_token
                }
                None => None,
            },
        };

        let new_generation = current_generation.map_or(1, |generation| generation + 1);
        let payload = SealedTokens {
            access_token: bundle.access_token.expose().to_owned(),
            refresh_token,
        };
        let plaintext = serde_json::to_vec(&payload)?;
        let (nonce, sealed) = crypto::seal(
            &self.key,
            &credential_aad(account_id, new_generation as i64),
            &plaintext,
        )?;
        let now = jiff::Timestamp::now().as_millisecond();

        tx.execute(
            "INSERT INTO credentials (account_id, generation, lifecycle_generation, sealed, nonce, issued_at, expires_at, refresh_expires_at, scopes, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(account_id) DO UPDATE SET generation = excluded.generation, lifecycle_generation = excluded.lifecycle_generation, \
             sealed = excluded.sealed, nonce = excluded.nonce, issued_at = excluded.issued_at, expires_at = excluded.expires_at, \
             refresh_expires_at = excluded.refresh_expires_at, scopes = excluded.scopes, updated_at = excluded.updated_at",
            params![
                account_id,
                new_generation as i64,
                lifecycle_generation as i64,
                sealed,
                nonce,
                bundle.issued_at,
                bundle.expires_at,
                bundle.refresh_expires_at,
                bundle.scopes,
                now,
            ],
        )?;

        // Whatever else was still open for this account was waiting on the bundle this commit replaces, so
        // it has its answer. Leaving one open would block every later refresh of the new credential and
        // quarantine the account at the next start.
        tx.execute(
            &format!(
                "UPDATE refresh_attempts SET resolved_at = ?2, outcome = 'failed', detail = 'superseded' \
                 WHERE account_id = ?1 AND id IS NOT ?3 AND ({PENDING_WHERE})"
            ),
            params![account_id, now, attempt_id.map(|id| id as i64)],
        )?;

        if let Some(id) = attempt_id {
            // Scoped by account_id, not just id: an attempt_id that belongs to another account must not
            // be resolved by this commit, even by accident.
            let updated = tx.execute(
                &format!(
                    "UPDATE refresh_attempts SET resolved_at = ?2, outcome = 'committed', detail = NULL \
                     WHERE id = ?1 AND account_id = ?3 AND ({PENDING_WHERE})"
                ),
                params![id as i64, now, account_id],
            )?;
            // A row that exists but was already settled elsewhere, by a disconnect for instance, leaves the
            // commit alone: the tokens in hand were rotated upstream and losing them to a rollback would
            // strand the account on a refresh token the endpoint has already replaced.
            if updated == 0 && !attempt_exists(&tx, id, Some(account_id))? {
                return Err(StoreError::AttemptNotFound(id));
            }
        }

        tx.commit()?;
        Ok(new_generation)
    }

    pub fn api_key_secret(&self, account_id: &str) -> Result<Option<Secret>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let row: Option<(Vec<u8>, Vec<u8>)> = conn
            .query_row(
                "SELECT sealed, nonce FROM api_key_secrets WHERE account_id = ?1",
                params![account_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((sealed, nonce)) = row else {
            return Ok(None);
        };
        let plaintext = crypto::open(&self.key, &api_key_aad(account_id), &nonce, &sealed)?;
        let value = String::from_utf8(plaintext).map_err(|_| StoreError::Crypto)?;
        Ok(Some(Secret::new(&value)))
    }

    /// The rotation moves the account's revision too: the secret is one half of what a dispatch pairs with
    /// an endpoint, so a holder that captured the row before the rotation has to see that it moved.
    pub fn set_api_key_secret(&self, account_id: &str, secret: &Secret) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        store_api_key_secret(&tx, &self.key, account_id, secret)?;
        crate::accounts::bump_revision(&tx, account_id)?;
        tx.commit()?;
        Ok(())
    }

    /// The secret with the generation that names it, so a verdict formed while it was in use can be told
    /// apart from one about its replacement.
    pub fn api_key_credential(
        &self,
        account_id: &str,
    ) -> Result<Option<(Secret, u64)>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let row: Option<(Vec<u8>, Vec<u8>, i64)> = conn
            .query_row(
                "SELECT sealed, nonce, generation FROM api_key_secrets WHERE account_id = ?1",
                params![account_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((sealed, nonce, generation)) = row else {
            return Ok(None);
        };
        let plain = crypto::open(&self.key, &api_key_aad(account_id), &nonce, &sealed)?;
        Ok(Some((
            Secret::new(std::str::from_utf8(&plain).map_err(|_| StoreError::Crypto)?),
            generation as u64,
        )))
    }

    /// The raw sealed blob of every credential row, for tests that assert plaintext is absent.
    #[doc(hidden)]
    pub fn debug_dump_credentials_table(&self) -> Result<Vec<Vec<u8>>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare("SELECT sealed FROM credentials")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<Vec<u8>>>>()
            .map_err(StoreError::from)
    }

    /// Replaces a credential row's sealed blob with bytes that cannot be opened, so a test can drive the
    /// paths that have to survive a row the store refuses to read. Compiled only under the `debug-hooks`
    /// feature, which a dependent's dev-dependency turns on.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_corrupt_credentials(&self, account_id: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "UPDATE credentials SET sealed = ?2 WHERE account_id = ?1",
            params![account_id, vec![0u8; 32]],
        )?;
        Ok(())
    }

    pub fn open_refresh_attempt(
        &self,
        account_id: &str,
        lifecycle_generation: u64,
        credential_generation: u64,
    ) -> Result<u64, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let pending: bool = conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM refresh_attempts WHERE account_id = ?1 AND ({PENDING_WHERE}))"),
            params![account_id],
            |row| row.get(0),
        )?;
        if pending {
            return Err(StoreError::AttemptPending);
        }
        let now = jiff::Timestamp::now().as_millisecond();
        conn.execute(
            "INSERT INTO refresh_attempts (account_id, lifecycle_generation, credential_generation, started_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![account_id, lifecycle_generation as i64, credential_generation as i64, now],
        )?;
        Ok(conn.last_insert_rowid() as u64)
    }

    /// Settles an attempt, but only one that is still open: unresolved, or resolved uncertain, which is
    /// what this crate treats as open everywhere else. A verdict that arrives after something else settled
    /// the row for good, a disconnect racing a slow token request for instance, is dropped rather than
    /// applied, and says so by returning `false`.
    pub fn resolve_refresh_attempt(
        &self,
        attempt_id: u64,
        outcome: AttemptOutcome,
    ) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let now = jiff::Timestamp::now().as_millisecond();
        let (label, detail) = match outcome {
            AttemptOutcome::Committed => ("committed", None),
            AttemptOutcome::Failed(detail) => ("failed", Some(detail)),
            AttemptOutcome::Uncertain(detail) => ("uncertain", Some(detail)),
        };
        let updated = conn.execute(
            &format!(
                "UPDATE refresh_attempts SET resolved_at = ?2, outcome = ?3, detail = ?4 \
                 WHERE id = ?1 AND ({PENDING_WHERE})"
            ),
            params![attempt_id as i64, now, label, detail],
        )?;
        if updated == 0 && !attempt_exists(&conn, attempt_id, None)? {
            return Err(StoreError::AttemptNotFound(attempt_id));
        }
        Ok(updated > 0)
    }

    // The partial index in the schema exists for this query: an attempt still in flight, or one that
    // resolved uncertain and needs recovery, both stay visible until something resolves them further.
    pub fn pending_refresh_attempts(&self) -> Result<Vec<PendingAttempt>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(&format!(
            "SELECT id, account_id, credential_generation, lifecycle_generation, started_at \
             FROM refresh_attempts WHERE {PENDING_WHERE} ORDER BY id"
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok(PendingAttempt {
                id: row.get::<_, i64>(0)? as u64,
                account_id: row.get(1)?,
                credential_generation: row.get::<_, i64>(2)? as u64,
                lifecycle_generation: row.get::<_, i64>(3)? as u64,
                started_at: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    pub fn last_refresh_attempt(
        &self,
        account_id: &str,
    ) -> Result<Option<LastAttempt>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT resolved_at, outcome, detail FROM refresh_attempts WHERE account_id = ?1 ORDER BY id DESC LIMIT 1",
            params![account_id],
            |row| Ok(LastAttempt { resolved_at: row.get(0)?, outcome: row.get(1)?, detail: row.get(2)? }),
        )
        .optional()
        .map_err(StoreError::from)
    }
}
