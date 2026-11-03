//! Encrypted credential bundles, API-key secrets, and refresh-attempt bookkeeping.

#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::Ordering;

use rusqlite::{params, OptionalExtension};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{crypto, StorageKey, Store, StoreError, LOCK_POISONED, PENDING_WHERE};

// The gate of `open_refresh_attempt` reads an attempt still in flight alone. An attempt resolved `uncertain` does
// not block here: the refresh manager spaces the retry by its floor, and the startup quarantine still reads the
// attempt through `PENDING_WHERE`.
const IN_FLIGHT_WHERE: &str = "resolved_at IS NULL";

// `PENDING_WHERE` as one term is the predicate of the partial index `refresh_attempts_pending`, so SQLite answers
// this read from that index rather than the table. Named, since the planner otherwise prefers
// `refresh_attempts_account`, which leads with both equality columns but holds every attempt the holder made.
fn holder_pending_query() -> String {
    format!(
        "SELECT EXISTS(SELECT 1 FROM refresh_attempts INDEXED BY refresh_attempts_pending \
         WHERE account_id = ?1 AND incarnation = ?2 AND ({PENDING_WHERE}))"
    )
}

// A holder's newest attempt, read through `refresh_attempts_account`: its two equality columns and then `id`, so the
// newest is the index's last entry under them rather than a sort of every attempt the holder made.
const LAST_REFRESH_ATTEMPT: &str = "SELECT resolved_at, outcome, detail FROM refresh_attempts \
     WHERE account_id = ?1 AND incarnation = ?2 ORDER BY id DESC LIMIT 1";

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
#[derive(serde::Serialize, serde::Deserialize, Zeroize, ZeroizeOnDrop)]
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

/// A credential's bookkeeping fields without its sealed tokens, for a view that shows nothing the caller
/// could use to authenticate.
#[derive(Debug)]
pub struct CredentialMetadata {
    pub generation: u64,
    pub lifecycle_generation: u64,
    pub issued_at: i64,
    pub expires_at: i64,
    pub refresh_expires_at: Option<i64>,
    pub scopes: String,
    /// The reason of the last credential failure that opened no refresh attempt (a backoff or a quarantine);
    /// cleared by the next committed credential.
    pub last_error: Option<String>,
    /// When `last_error` was recorded. Meaningful only alongside `last_error`: it rides on the row's
    /// `updated_at`, which a commit that clears the error also advances, so the pairing never goes stale.
    pub last_error_at: i64,
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
    /// The holder of the id the attempt began under, which a recovery compares before acting on it.
    pub incarnation: u64,
    pub lifecycle_generation: u64,
    pub started_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedAttempt {
    pub id: u64,
    pub account_id: String,
    pub started_at: i64,
    pub resolved_at: i64,
    pub outcome: String,
    pub detail: Option<String>,
    pub lifecycle_generation: u64,
    pub credential_generation: u64,
}

impl ResolvedAttempt {
    /// The credential generation a committed attempt's commit wrote: a pairing's row records the generation it
    /// wrote, a refresh's the one it was opened against, which its commit moved one past. `None` for an attempt that
    /// committed nothing.
    pub fn committed_generation(&self) -> Option<u64> {
        (self.outcome == "committed").then(|| {
            if self.detail.as_deref() == Some("pairing") {
                self.credential_generation
            } else {
                self.credential_generation + 1
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct LastAttempt {
    pub resolved_at: Option<i64>,
    pub outcome: Option<String>,
    pub detail: Option<String>,
}

// (generation, sealed, nonce, issued_at, expires_at, refresh_expires_at, scopes, lifecycle_generation)
type CredentialRow = (i64, Vec<u8>, Vec<u8>, i64, i64, Option<i64>, String, i64);

pub(crate) fn credential_aad(account_id: &str, generation: i64) -> String {
    format!("{account_id}:{generation}")
}

pub(crate) fn api_key_aad(account_id: &str) -> String {
    format!("{account_id}:api_key")
}

/// Whether the attempt row is there at all, which is what separates a verdict that arrived too late from
/// one that names an attempt nobody ever opened.
fn attempt_exists(
    conn: &rusqlite::Connection,
    attempt_id: u64,
    account_id: &str,
) -> Result<bool, StoreError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM refresh_attempts WHERE id = ?1 AND account_id = ?2)",
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
        #[cfg(any(test, feature = "debug-hooks"))]
        {
            self.count_credential_read();
            self.park_on_credential_hold();
        }
        let read = self.read_credentials(account_id);
        #[cfg(any(test, feature = "debug-hooks"))]
        self.park_after_credential_read();
        read
    }

    fn read_credentials(&self, account_id: &str) -> Result<Option<CredentialBundle>, StoreError> {
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

        let plaintext = Zeroizing::new(crypto::open(
            &self.key,
            &credential_aad(account_id, generation),
            &nonce,
            &sealed,
        )?);
        let tokens: SealedTokens = serde_json::from_slice(&plaintext)?;

        Ok(Some(CredentialBundle {
            access_token: Secret::new(&tokens.access_token),
            refresh_token: tokens.refresh_token.as_deref().map(Secret::new),
            issued_at,
            expires_at,
            refresh_expires_at,
            scopes,
            generation: generation as u64,
            lifecycle_generation: lifecycle_generation as u64,
        }))
    }

    /// The bookkeeping fields of an account's credential without opening its sealed blob, for a view that
    /// shows nothing the caller could use to authenticate.
    pub fn credential_metadata(
        &self,
        account_id: &str,
    ) -> Result<Option<CredentialMetadata>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT generation, issued_at, expires_at, refresh_expires_at, scopes, lifecycle_generation, last_error, updated_at \
             FROM credentials WHERE account_id = ?1",
            params![account_id],
            |row| {
                Ok(CredentialMetadata {
                    generation: row.get::<_, i64>(0)? as u64,
                    issued_at: row.get(1)?,
                    expires_at: row.get(2)?,
                    refresh_expires_at: row.get(3)?,
                    scopes: row.get(4)?,
                    lifecycle_generation: row.get::<_, i64>(5)? as u64,
                    last_error: row.get(6)?,
                    last_error_at: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// Records a credential failure that opened no refresh attempt (a backoff or a quarantine), so
    /// `credential.last_error` has something to show; a no-op once the account has moved past the
    /// incarnation the caller observed, since the failure is no longer about the slot standing there now.
    pub fn record_credential_error(
        &self,
        account_id: &str,
        incarnation: u64,
        message: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let now = self.now_ms();
        let updated = conn.execute(
            "UPDATE credentials SET last_error = ?3, updated_at = ?4 WHERE account_id = ?1 \
             AND EXISTS (SELECT 1 FROM accounts WHERE accounts.id = ?1 AND accounts.incarnation = ?2)",
            params![account_id, incarnation as i64, message, now],
        )?;
        Ok(updated > 0)
    }

    /// Commits a new credential generation in one transaction: the incarnation and the lifecycle generation
    /// are checked against the account's current state (`Disconnected` on mismatch, since either one moving
    /// means the slot was disconnected or replaced), the credential generation likewise (`Stale` on mismatch,
    /// a race with another write to the same bundle), a missing refresh token is folded forward from the
    /// previous plaintext, and `attempt_id`, if given, is resolved as `Committed` here too, so a crash between
    /// the two writes cannot leave a committed bundle behind a pending attempt.
    ///
    /// The incarnation is what the lifecycle generation cannot answer for: deleting a slot and recreating it
    /// under the same id restarts every generation at 1, so only the incarnation tells a refresh that began
    /// before the deletion from one that belongs to the row standing there now.
    pub fn commit_credentials(
        &self,
        account_id: &str,
        incarnation: u64,
        lifecycle_generation: u64,
        expected_generation: Option<u64>,
        attempt_id: Option<u64>,
        bundle: &NewBundle,
    ) -> Result<u64, StoreError> {
        self.fail_injected_commit()?;
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let generation = self.commit_credentials_in(
            &tx,
            account_id,
            incarnation,
            lifecycle_generation,
            expected_generation,
            attempt_id,
            bundle,
            self.now_ms(),
        )?;
        tx.commit()?;
        Ok(generation)
    }

    /// `commit_credentials` for a pairing: the credential write, the supersession of the pending and uncertain
    /// rows and the pairing's own resolved `committed` attempt (`detail = 'pairing'`) in one transaction, so a
    /// commit refused by its fences or its write records no success. The attempt is what ends a run of failed
    /// refreshes for the attention reader when nothing was pending to supersede.
    pub fn commit_pairing(
        &self,
        account_id: &str,
        incarnation: u64,
        lifecycle_generation: u64,
        expected_generation: Option<u64>,
        bundle: &NewBundle,
    ) -> Result<u64, StoreError> {
        self.fail_injected_commit()?;
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // One instant for the supersession and the pairing's own row, so the pairing reads as the newest settled
        // attempt whatever the clock does between the two writes.
        let now = self.now_ms();
        let generation = self.commit_credentials_in(
            &tx,
            account_id,
            incarnation,
            lifecycle_generation,
            expected_generation,
            None,
            bundle,
            now,
        )?;
        tx.execute(
            "INSERT INTO refresh_attempts (account_id, incarnation, lifecycle_generation, credential_generation, \
             started_at, resolved_at, outcome, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?5, 'committed', 'pairing')",
            params![
                account_id,
                incarnation as i64,
                lifecycle_generation as i64,
                generation as i64,
                now
            ],
        )?;
        tx.commit()?;
        Ok(generation)
    }

    /// The seam `debug_fail_next_credential_commit` arms, checked before each commit's transaction.
    pub(crate) fn fail_injected_commit(&self) -> Result<(), StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        if self
            .fail_next_credential_commit
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Io(std::io::Error::other(
                "injected credential commit failure",
            )));
        }
        Ok(())
    }

    /// The body both commits share, inside the caller's transaction, which the caller commits; `now` stamps the
    /// credential row and every attempt it settles.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_credentials_in(
        &self,
        tx: &rusqlite::Transaction<'_>,
        account_id: &str,
        incarnation: u64,
        lifecycle_generation: u64,
        expected_generation: Option<u64>,
        attempt_id: Option<u64>,
        bundle: &NewBundle,
        now: i64,
    ) -> Result<u64, StoreError> {
        let account: Option<(i64, i64)> = tx
            .query_row(
                "SELECT incarnation, lifecycle_generation FROM accounts WHERE id = ?1",
                params![account_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (account_incarnation, account_lifecycle) =
            account.ok_or_else(|| StoreError::AccountNotFound(account_id.to_string()))?;
        if account_incarnation as u64 != incarnation
            || account_lifecycle as u64 != lifecycle_generation
        {
            return Err(StoreError::Disconnected);
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
                    let plaintext = Zeroizing::new(crypto::open(
                        &self.key,
                        &credential_aad(account_id, *generation),
                        nonce,
                        sealed,
                    )?);
                    let mut previous: SealedTokens = serde_json::from_slice(&plaintext)?;
                    previous.refresh_token.take()
                }
                None => None,
            },
        };

        let new_generation = current_generation.map_or(1, |generation| generation + 1);
        let payload = SealedTokens {
            access_token: bundle.access_token.expose().to_owned(),
            refresh_token,
        };
        let plaintext = Zeroizing::new(serde_json::to_vec(&payload)?);
        let (nonce, sealed) = crypto::seal(
            &self.key,
            &credential_aad(account_id, new_generation as i64),
            &plaintext,
        )?;

        // `last_error` is cleared here rather than through a separate call: the connection mutex is not
        // reentrant, and a credential that just committed has nothing stale left to explain.
        tx.execute(
            "INSERT INTO credentials (account_id, generation, lifecycle_generation, sealed, nonce, issued_at, expires_at, refresh_expires_at, scopes, updated_at, last_error) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL) \
             ON CONFLICT(account_id) DO UPDATE SET generation = excluded.generation, lifecycle_generation = excluded.lifecycle_generation, \
             sealed = excluded.sealed, nonce = excluded.nonce, issued_at = excluded.issued_at, expires_at = excluded.expires_at, \
             refresh_expires_at = excluded.refresh_expires_at, scopes = excluded.scopes, updated_at = excluded.updated_at, last_error = NULL",
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
            if updated == 0 && !attempt_exists(tx, id, account_id)? {
                return Err(StoreError::AttemptNotFound(id));
            }
        }
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
        crate::accounts::bump_revision(&tx, self.now_ms(), account_id)?;
        tx.commit()?;
        Ok(())
    }

    /// The generation of whatever credential the account holds, whichever of the two tables holds it. An
    /// account holds one kind at a time, so at most one row answers.
    pub fn credential_generation(&self, account_id: &str) -> Result<Option<u64>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT generation FROM credentials WHERE account_id = ?1 \
             UNION ALL SELECT generation FROM api_key_secrets WHERE account_id = ?1",
            params![account_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map(|generation| generation.map(|g| g as u64))
        .map_err(StoreError::from)
    }

    /// The secret with the generation that names it, so a verdict formed while it was in use can be told
    /// apart from one about its replacement.
    pub fn api_key_credential(
        &self,
        account_id: &str,
    ) -> Result<Option<(Secret, u64)>, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        {
            self.count_credential_read();
            self.park_on_credential_hold();
        }
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

    /// Attempts settled after a watermark, oldest first. Keyed on the resolution time rather than the id
    /// because an attempt opened early can settle late, and a mirror that walked ids would skip it; the id is
    /// the tie-breaker, so rows settled in the same millisecond page through without a gap. An attempt
    /// resolved `uncertain` that a later commit closes `superseded` settles again under the commit's time, so
    /// a reader meets it twice; the second reading records the attempt the commit replaced, not a failure, and
    /// a failure count skips it.
    pub fn refresh_attempts_resolved_after(
        &self,
        resolved_at: i64,
        id: u64,
        limit: usize,
    ) -> Result<Vec<ResolvedAttempt>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(
            "SELECT id, account_id, started_at, resolved_at, outcome, detail, lifecycle_generation, \
             credential_generation FROM refresh_attempts \
             WHERE (resolved_at > ?1 OR (resolved_at = ?1 AND id > ?2)) AND outcome IS NOT NULL \
             ORDER BY resolved_at, id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![resolved_at, id as i64, limit as i64], |r| {
            Ok(ResolvedAttempt {
                id: r.get::<_, i64>(0)? as u64,
                account_id: r.get(1)?,
                started_at: r.get(2)?,
                resolved_at: r.get(3)?,
                outcome: r.get(4)?,
                detail: r.get(5)?,
                lifecycle_generation: r.get::<_, i64>(6)? as u64,
                credential_generation: r.get::<_, i64>(7)? as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    /// Where a mirror starts: the newest settled attempt, read through the resolved index rather than by
    /// loading a table the history purge cuts only at the retention.
    pub fn newest_resolved_refresh_attempt(&self) -> Result<Option<(i64, u64)>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT resolved_at, id FROM refresh_attempts WHERE outcome IS NOT NULL \
             ORDER BY resolved_at DESC, id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)),
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// Test seam for the paging test above, which needs several rows settled in one millisecond; compiled
    /// under `debug-hooks` like the other seams, so a release build cannot rewrite history.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_set_resolved_at(&self, ids: &[u64], resolved_at: i64) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        for id in ids {
            conn.execute(
                "UPDATE refresh_attempts SET resolved_at = ?2 WHERE id = ?1",
                params![*id as i64, resolved_at],
            )?;
        }
        Ok(())
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

    /// Fails the next `commit_credentials` before its transaction, so a test reaches the manager's failed-commit
    /// arm without a broken disk.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_fail_next_credential_commit(&self) {
        self.fail_next_credential_commit
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn open_refresh_attempt(
        &self,
        account_id: &str,
        incarnation: u64,
        lifecycle_generation: u64,
        credential_generation: u64,
    ) -> Result<u64, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let pending: bool = conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM refresh_attempts WHERE account_id = ?1 AND ({IN_FLIGHT_WHERE}))"),
            params![account_id],
            |row| row.get(0),
        )?;
        if pending {
            return Err(StoreError::AttemptPending);
        }
        let now = self.now_ms();
        conn.execute(
            "INSERT INTO refresh_attempts (account_id, incarnation, lifecycle_generation, credential_generation, started_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![account_id, incarnation as i64, lifecycle_generation as i64, credential_generation as i64, now],
        )?;
        Ok(conn.last_insert_rowid() as u64)
    }

    /// Settles an attempt, but only one that is still open: unresolved, or resolved uncertain, as `PENDING_WHERE`
    /// reads it for the listing and the sweeps too. A verdict that arrives after something else settled the row
    /// for good, a disconnect racing a slow token request for instance, is dropped rather than applied, and says
    /// so by returning `false`.
    ///
    /// `incarnation` is the one the caller's work began under: a verdict from an incarnation the id has since
    /// moved past answers `Stale` rather than settling, or reopening as uncertain, a row under the id's new
    /// holder. An id with no account row left has nothing to be stale against, so its orphaned attempts still
    /// settle.
    pub fn resolve_refresh_attempt(
        &self,
        attempt_id: u64,
        incarnation: u64,
        outcome: AttemptOutcome,
    ) -> Result<bool, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = self.now_ms();
        let (label, detail) = match outcome {
            AttemptOutcome::Committed => ("committed", None),
            AttemptOutcome::Failed(detail) => ("failed", Some(detail)),
            AttemptOutcome::Uncertain(detail) => ("uncertain", Some(detail)),
        };
        let account_id: Option<String> = tx
            .query_row(
                "SELECT account_id FROM refresh_attempts WHERE id = ?1",
                params![attempt_id as i64],
                |row| row.get(0),
            )
            .optional()?;
        let Some(account_id) = account_id else {
            return Err(StoreError::AttemptNotFound(attempt_id));
        };
        let current: Option<i64> = tx
            .query_row(
                "SELECT incarnation FROM accounts WHERE id = ?1",
                params![account_id],
                |row| row.get(0),
            )
            .optional()?;
        if current.is_some_and(|current| current as u64 != incarnation) {
            return Err(StoreError::Stale);
        }
        let updated = tx.execute(
            &format!(
                "UPDATE refresh_attempts SET resolved_at = ?2, outcome = ?3, detail = ?4 \
                 WHERE id = ?1 AND ({PENDING_WHERE})"
            ),
            params![attempt_id as i64, now, label, detail],
        )?;
        tx.commit()?;
        Ok(updated > 0)
    }

    // The partial index in the schema exists for this query: an attempt still in flight, or one that
    // resolved uncertain and needs recovery, both stay visible until something resolves them further.
    pub fn pending_refresh_attempts(&self) -> Result<Vec<PendingAttempt>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(&format!(
            "SELECT id, account_id, credential_generation, lifecycle_generation, started_at, incarnation \
             FROM refresh_attempts WHERE {PENDING_WHERE} ORDER BY id"
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok(PendingAttempt {
                id: row.get::<_, i64>(0)? as u64,
                account_id: row.get(1)?,
                credential_generation: row.get::<_, i64>(2)? as u64,
                incarnation: row.get::<_, i64>(5)? as u64,
                lifecycle_generation: row.get::<_, i64>(3)? as u64,
                started_at: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    /// Whether the holder `incarnation` names has an attempt still open, which a restart quarantines the account
    /// for: one in flight, or one resolved uncertain that no later commit has superseded; a later retry that
    /// failed leaves it open.
    pub fn has_pending_refresh_attempt(
        &self,
        account_id: &str,
        incarnation: u64,
    ) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            &holder_pending_query(),
            params![account_id, incarnation as i64],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
    }

    /// The newest attempt of the holder `incarnation` names: a previous holder's history under the same id is
    /// not this one's.
    pub fn last_refresh_attempt(
        &self,
        account_id: &str,
        incarnation: u64,
    ) -> Result<Option<LastAttempt>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            LAST_REFRESH_ATTEMPT,
            params![account_id, incarnation as i64],
            |row| {
                Ok(LastAttempt {
                    resolved_at: row.get(0)?,
                    outcome: row.get(1)?,
                    detail: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// The newest settled attempts of the holder `incarnation` names, newest first, bounded by `limit`: the
    /// attention reader walks them for a run of failures, and a previous holder's history is not this one's.
    pub fn newest_resolved_refresh_attempts(
        &self,
        account_id: &str,
        incarnation: u64,
        limit: usize,
    ) -> Result<Vec<ResolvedAttempt>, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        if self.fail_next_refresh_read.swap(false, Ordering::SeqCst) {
            return Err(StoreError::Io(std::io::Error::other(
                "injected refresh read failure",
            )));
        }
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(
            "SELECT id, account_id, started_at, resolved_at, outcome, detail, lifecycle_generation, \
             credential_generation FROM refresh_attempts \
             WHERE account_id = ?1 AND incarnation = ?2 AND outcome IS NOT NULL \
             ORDER BY resolved_at DESC, id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, incarnation as i64, limit as i64], |r| {
            Ok(ResolvedAttempt {
                id: r.get::<_, i64>(0)? as u64,
                account_id: r.get(1)?,
                started_at: r.get(2)?,
                resolved_at: r.get(3)?,
                outcome: r.get(4)?,
                detail: r.get(5)?,
                lifecycle_generation: r.get::<_, i64>(6)? as u64,
                credential_generation: r.get::<_, i64>(7)? as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_holders_pending_read_is_answered_from_the_partial_index() {
        let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
        let conn = store.conn.lock().unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", holder_pending_query()))
            .unwrap()
            .query_map(params!["a", 1], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|detail| detail.contains("USING INDEX refresh_attempts_pending")),
            "{plan:?}"
        );
    }

    #[test]
    fn a_holders_newest_attempt_is_read_through_the_account_index() {
        let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
        let conn = store.conn.lock().unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {LAST_REFRESH_ATTEMPT}"))
            .unwrap()
            .query_map(params!["a", 1], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|detail| detail.contains("USING INDEX refresh_attempts_account")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|detail| detail.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }
}
