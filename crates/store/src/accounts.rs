//! Account registry: rows, lifecycle state, and the lifecycle-generation bump that disconnects a slot.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use rusqlite::{params, OptionalExtension, Row};

use crate::registry::{
    optional_json_column, organization_type_of, plan_of, usage_based_of, Cancellation,
    SubscriptionTerm,
};
use crate::{AttemptOutcome, Store, StoreError, LOCK_POISONED, PENDING_WHERE};

text_enum! {
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AccountKind {
        #[default]
        Oauth => "oauth",
        ApiKey => "api_key",
    }
}

text_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AccountState {
        Unpaired => "unpaired",
        Ready => "ready",
        ReauthRequired => "reauth_required",
        FundingRequired => "funding_required",
        Disconnected => "disconnected",
    }
}

text_enum! {
    /// Who moved an account's state, stored beside it in the same statement and read by the routing log's transition
    /// row. The list is closed: a stored word outside it fails the read, as an unknown state does.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum StateCause {
        /// The upstream refused the credential: a 401, or a 403 `authentication_error`, on an `api_key` account, or
        /// a 401 on the replay with a freshly refreshed token.
        AuthFailed => "auth_failed",
        /// The account's deletion. The row goes with it, so the word reaches the log alone.
        Deleted => "deleted",
        /// The upstream named an `api_key` account's credit balance.
        FundingRefused => "funding_refused",
        /// The router's own retry of a `funding_required` account, whose answer came back a success.
        FundingRetry => "funding_retry",
        /// The admin routes, and whatever reaches them.
        Operator => "operator",
        /// A pairing's confirm.
        Paired => "paired",
        /// The token refresh's definitive failure, the oauth manager's own write.
        RefreshFailed => "refresh_failed",
        /// The migration's default on a row from before the column. No writer stores it.
        Unrecorded => "unrecorded",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub account_uuid: String,
    pub email: String,
    pub organization_uuid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub id: String,
    pub kind: AccountKind,
    pub label: String,
    pub enabled: bool,
    pub identity: Option<Identity>,
    pub lifecycle_generation: u64,
    /// How many times this account id has been used. A deletion leaves the count behind in a tombstone and
    /// its replacement starts one above it, so work still in flight for the deleted slot cannot land on the
    /// row that took its id.
    pub incarnation: u64,
    /// Bumped by every write that changes what a request dispatches with, or what an operator verified an
    /// edit against. A holder that captured the row before a slow step compares it to tell whether the
    /// endpoint and the credential it is about to pair are still the ones it looked at.
    pub revision: u64,
    pub state: AccountState,
    /// Who moved `state` to what it reads, written by the statement that wrote it.
    pub state_cause: StateCause,
}

/// The low bits of an edit token, which carry the revision; the incarnation takes the bits above them.
const EDIT_TOKEN_REVISION_BITS: u32 = 32;

/// `high` above the low `low_bits` and `low` in them, each part saturating at its share. The token stays below 2^53,
/// the largest integer a JavaScript number holds exactly, so the console sends back the very value it was given.
fn pack_token(high: u64, low: u64, low_bits: u32) -> u64 {
    let low_max = (1 << low_bits) - 1;
    let high_max = (1 << (53 - low_bits)) - 1;
    (high.min(high_max) << low_bits) | low.min(low_max)
}

fn unpack_token(token: u64, low_bits: u32) -> (u64, u64) {
    (token >> low_bits, token & ((1 << low_bits) - 1))
}

impl AccountRow {
    /// A ready account labeled by its id, at its first incarnation, generation and revision, with no identity: a
    /// test overrides the fields its case is about, so a field added to or removed from the row touches this one site.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(id: &str, kind: AccountKind) -> AccountRow {
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

    /// What an operator's edit sends back to be fenced on: the incarnation and the revision in one integer. An
    /// account deleted and created again under the same id restarts its revision at 1, so the revision alone would
    /// let an editor opened before the deletion land on the replacement. A part past its share saturates: the token
    /// then names a row this one is not, so a save sending it back is refused rather than fenced onto another holder.
    pub fn edit_token(&self) -> u64 {
        pack_token(self.incarnation, self.revision, EDIT_TOKEN_REVISION_BITS)
    }

    /// Whether `token` names this row's incarnation and revision both.
    pub fn answers_edit_token(&self, token: u64) -> bool {
        unpack_token(token, EDIT_TOKEN_REVISION_BITS) == (self.incarnation, self.revision)
    }
}

/// The low bits of a stamp edit token, which carry the `updated_at` millisecond; 42 bits hold every stamp to the
/// year 2109, and the incarnation takes the bits above them, below 2^53.
const STAMP_TOKEN_BITS: u32 = 42;

/// What an operator's edit of a pool or a key sends back to be fenced on: the row's incarnation and its `updated_at`
/// in one integer, so an editor opened before the row was deleted and created again cannot land on the replacement
/// even when both stamps fall in one millisecond. A part past its share saturates, as `edit_token`'s does, and the
/// token then names a row this one is not.
pub fn stamp_token(incarnation: u64, updated_at: i64) -> u64 {
    let stamp = u64::try_from(updated_at).unwrap_or(0);
    pack_token(incarnation, stamp, STAMP_TOKEN_BITS)
}

/// Whether `token` names a row of this incarnation and this `updated_at` both.
pub fn answers_stamp_token(incarnation: u64, updated_at: i64, token: u64) -> bool {
    let (high, stamp) = unpack_token(token, STAMP_TOKEN_BITS);
    (high, stamp as i64) == (incarnation, updated_at)
}

/// The low bits of a limit resets token, which carry the block's epoch. The incarnation takes the bits above them.
const LIMIT_RESETS_EPOCH_BITS: u32 = 32;

/// What a use of a limit reset sends back to be bound to the block it was confirmed on: the row's incarnation and the
/// block's epoch in one integer. A grant id names a promotion and repeats across accounts, so a claim must name the
/// account a dialog showed. The epoch moves at a pairing, which can name another organization of the same upstream
/// account, and at a claim's fold, which changes the counts, and an account created again under the id starts its
/// epoch over, so neither part alone names one account under one login with the counts one dialog showed. A part past
/// its share saturates, as `edit_token`'s does.
pub fn limit_resets_token(incarnation: u64, epoch: u64) -> u64 {
    pack_token(incarnation, epoch, LIMIT_RESETS_EPOCH_BITS)
}

/// The one host the relay dials.
pub const INFERENCE_HOST: &str = "api.anthropic.com";

fn row_to_account(row: &Row<'_>) -> rusqlite::Result<AccountRow> {
    Ok(AccountRow {
        id: row.get("id")?,
        kind: row.get("kind")?,
        label: row.get("label")?,
        enabled: row.get("enabled")?,
        identity: optional_json_column(row, 4)?,
        incarnation: row.get::<_, i64>("incarnation")? as u64,
        lifecycle_generation: row.get::<_, i64>("lifecycle_generation")? as u64,
        revision: row.get::<_, i64>("revision")? as u64,
        state: row.get("state")?,
        state_cause: row.get("state_cause")?,
    })
}

fn identity_json(row: &AccountRow) -> Result<Option<String>, StoreError> {
    Ok(row
        .identity
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?)
}

/// The account row under the incarnation one past its id's tombstone, stamped `at`. Answers the incarnation it took.
pub(crate) fn insert_account_row(
    tx: &rusqlite::Transaction<'_>,
    row: &AccountRow,
    at: i64,
) -> Result<u64, StoreError> {
    let identity_json = identity_json(row)?;
    let incarnation = next_incarnation_in(tx, Tombstones::Account, &row.id)?;
    tx.execute(
        "INSERT INTO accounts (id, kind, label, enabled, identity_json, incarnation, lifecycle_generation, revision, state, state_cause, created_at, updated_at, state_changed_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?10, ?9, ?9, ?9)",
        params![
            row.id,
            row.kind,
            row.label,
            row.enabled,
            identity_json,
            incarnation as i64,
            row.lifecycle_generation as i64,
            row.state,
            at,
            row.state_cause,
        ],
    )?;
    Ok(incarnation)
}

/// `Store::set_profile`'s statement on the caller's connection, stamped `at`, so a create writes the profile columns
/// as a poll does. Answers how many rows it wrote.
pub(crate) fn write_profile_in(
    conn: &rusqlite::Connection,
    id: &str,
    json: &str,
    subscription: &SubscriptionRow,
    at: i64,
) -> Result<usize, StoreError> {
    Ok(conn.execute(
        "UPDATE accounts SET profile_json = ?2, subscription_tier = ?3, subscription_since = ?4, \
         trial_ends_at = ?5, updated_at = ?6 WHERE id = ?1",
        params![
            id,
            json,
            subscription.tier,
            subscription.since,
            subscription.trial_ends_at,
            at,
        ],
    )?)
}

// The columns `row_to_account` reads, in its order.
const ACCOUNT_SELECT: &str =
    "SELECT id, kind, label, enabled, identity_json, incarnation, lifecycle_generation, \
     revision, state, state_cause FROM accounts";

/// Shared by `Store::account` and the account edit, which reads the row it changes inside its own transaction.
pub(crate) fn account_in(
    conn: &rusqlite::Connection,
    id: &str,
) -> Result<Option<AccountRow>, StoreError> {
    conn.query_row(
        &format!("{ACCOUNT_SELECT} WHERE id = ?1"),
        params![id],
        row_to_account,
    )
    .optional()
    .map_err(StoreError::from)
}

/// Shared by `Store::accounts` and `load_registry`, so the registry snapshot can read the account list
/// inside its own transaction instead of through the store's connection lock.
pub(crate) fn all_accounts(conn: &rusqlite::Connection) -> Result<Vec<AccountRow>, StoreError> {
    let mut stmt = conn.prepare(&format!("{ACCOUNT_SELECT} ORDER BY id"))?;
    let rows = stmt.query_map([], row_to_account)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(StoreError::from)
}

impl Store {
    pub fn accounts(&self) -> Result<Vec<AccountRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        all_accounts(&conn)
    }

    pub fn account(&self, id: &str) -> Result<Option<AccountRow>, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.count_credential_read();
        let conn = self.conn.lock().expect(LOCK_POISONED);
        account_in(&conn, id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_account(&self, row: &AccountRow) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        insert_account_row(&tx, row, self.now_ms())?;
        tx.commit()?;
        Ok(())
    }

    /// Brings a row in line with the bootstrap file. Identity, credentials, state, and the lifecycle
    /// generation are not configuration's to touch, so they stay as they are.
    pub fn update_account_config(
        &self,
        id: &str,
        kind: AccountKind,
        label: &str,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let stored: Option<AccountKind> = tx
            .query_row(
                "SELECT kind FROM accounts WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let stored = stored.ok_or_else(|| StoreError::AccountNotFound(id.to_string()))?;
        // A sealed bundle and a sealed API key are each reached through the account's kind, so flipping it
        // under a stored credential would orphan one with nothing left to open it.
        if stored != kind && holds_credentials(&tx, id)? {
            return Err(StoreError::KindChange(id.to_string()));
        }
        tx.execute(
            "UPDATE accounts SET kind = ?2, label = ?3, revision = revision + 1, updated_at = ?4 \
             WHERE id = ?1",
            params![id, kind, label, self.now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Pauses or resumes an account and records the flip as an `account_updated` change carrying the `enabled` pair,
    /// in one transaction. Writes nothing where the flag already reads `enabled`.
    pub fn set_enabled(&self, id: &str, enabled: bool, actor: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let before: Option<bool> = tx
            .query_row(
                "SELECT enabled FROM accounts WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let before = before.ok_or_else(|| StoreError::AccountNotFound(id.to_string()))?;
        // A repeat moves no revision, so an account edit opened before it still lands.
        if before == enabled {
            return Ok(());
        }
        let now = self.now_ms();
        tx.execute(
            "UPDATE accounts SET enabled = ?2, revision = revision + 1, updated_at = ?3 WHERE id = ?1",
            params![id, enabled, now],
        )?;
        crate::registry::record_change(
            &tx,
            now,
            actor,
            "account_updated",
            id,
            &serde_json::json!({ "enabled": { "from": before, "to": enabled } }).to_string(),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Binds the slot to an upstream account, once. A slot keeps the identity it was first paired with
    /// across disconnects, so re-pairing restores access to that same upstream account instead of moving
    /// the slot to another one; a second identity is `Stale` rather than an overwrite.
    pub fn set_identity(&self, id: &str, identity: &Identity) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let current: Option<Option<String>> = conn
            .query_row(
                "SELECT identity_json FROM accounts WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let current = current.ok_or_else(|| StoreError::AccountNotFound(id.to_string()))?;
        if let Some(json) = current {
            let bound: Identity = serde_json::from_str(&json)?;
            if bound.account_uuid != identity.account_uuid {
                return Err(StoreError::Stale);
            }
        }
        let written = conn.execute(
            "UPDATE accounts SET identity_json = ?2, revision = revision + 1, updated_at = ?3 WHERE id = ?1",
            params![
                id,
                serde_json::to_string(identity)?,
                self.now_ms()
            ],
        );
        match written {
            Ok(_) => Ok(()),
            Err(error) if crate::registry::is_constraint(&error) => Err(StoreError::IdentityTaken),
            Err(error) => Err(error.into()),
        }
    }

    pub fn set_state(
        &self,
        id: &str,
        state: AccountState,
        cause: StateCause,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "UPDATE accounts SET state = ?2, state_cause = ?4, revision = revision + 1, updated_at = ?3, \
             state_changed_at = CASE WHEN state = ?2 THEN state_changed_at ELSE ?3 END \
             WHERE id = ?1",
            params![id, state, self.now_ms(), cause],
        )?;
        Ok(())
    }

    /// `set_state` for a verdict formed before a request that took a while: the state only moves while the
    /// slot still holds the exact credentials the caller looked at. A disconnect moves the lifecycle
    /// generation and a re-pair without one moves the credential generation, so both have to match or the
    /// verdict is about credentials that no longer exist and would only lock out what replaced them. A slot
    /// deleted and created again under the same id reads both generations at 1 again, so the incarnation has
    /// to match as well. Answers the row's revision after the write and the state it held before it when the fences
    /// let the write through, a repeated verdict included, or `None` when they refused it.
    ///
    /// One statement for the fenced write, because two would leave a window between the check and the update
    /// exactly as wide as the one this exists to close.
    pub fn set_state_if_generations(
        &self,
        id: &str,
        state: AccountState,
        cause: StateCause,
        incarnation: u64,
        lifecycle_generation: u64,
        credential_generation: u64,
    ) -> Result<Option<(u64, AccountState)>, StoreError> {
        self.routing_commit(|conn| {
            set_state_if_generations_on(
                conn,
                self.now_ms(),
                id,
                state,
                cause,
                incarnation,
                lifecycle_generation,
                credential_generation,
            )
        })
    }

    /// Disconnects the account: the new lifecycle generation invalidates any refresh in flight for the
    /// old one, and both credential kinds are deleted so a disconnected slot cannot keep spending money.
    /// `cause` is who disconnected the slot. The statement writes `disconnected` and the word together.
    pub fn bump_lifecycle(&self, id: &str, cause: StateCause) -> Result<u64, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = self.now_ms();
        let updated = tx.execute(
            "UPDATE accounts SET lifecycle_generation = lifecycle_generation + 1, state = ?2, \
             state_cause = ?4, revision = revision + 1, updated_at = ?3, \
             state_changed_at = CASE WHEN state = ?2 THEN state_changed_at ELSE ?3 END WHERE id = ?1",
            params![id, AccountState::Disconnected, now, cause],
        )?;
        if updated == 0 {
            return Err(StoreError::AccountNotFound(id.to_string()));
        }
        let new_generation: i64 = tx.query_row(
            "SELECT lifecycle_generation FROM accounts WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        tx.execute("DELETE FROM credentials WHERE account_id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM api_key_secrets WHERE account_id = ?1",
            params![id],
        )?;
        // A disconnect settles every open attempt: one still in flight from before the disconnect would block
        // every refresh after re-pairing, and an uncertain one would quarantine the re-paired account at start.
        let disconnected = AttemptOutcome::Disconnected;
        tx.execute(
            &format!(
                "UPDATE refresh_attempts SET resolved_at = ?2, outcome = ?3, detail = ?4, cause = ?5 \
                 WHERE account_id = ?1 AND ({PENDING_WHERE})"
            ),
            params![
                id,
                now,
                disconnected.word(),
                disconnected.detail(),
                disconnected.cause()
            ],
        )?;
        tx.commit()?;
        Ok(new_generation as u64)
    }

    /// The row goes, the count of how often its id has been used stays: whatever is still in flight for the
    /// deleted slot names an incarnation the tombstone has already moved past, so it cannot land on whatever
    /// takes the id next.
    pub fn delete_account(&self, id: &str, actor: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = self.now_ms();
        if !delete_and_bury(&tx, "accounts", Tombstones::Account, id)? {
            return Err(StoreError::AccountNotFound(id.to_string()));
        }
        // An open attempt is the deleted holder's, and left standing it would block the next holder's first
        // refresh or quarantine it at the next start. A settled one stays as history under its incarnation.
        tx.execute(
            &format!("DELETE FROM refresh_attempts WHERE account_id = ?1 AND ({PENDING_WHERE})"),
            params![id],
        )?;
        crate::registry::record_change(&tx, now, actor, "account_deleted", id, "{}")?;
        tx.commit()?;
        Ok(())
    }

    /// The account's current revision, for a holder that wants to know whether the row moved without
    /// paying for the whole row and its identity.
    pub fn account_revision(&self, id: &str) -> Result<Option<u64>, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.count_credential_read();
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT revision FROM accounts WHERE id = ?1",
            params![id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map(|revision| revision.map(|r| r as u64))
        .map_err(StoreError::from)
    }

    /// The raw document and what it says about the subscription, in one write; bumps no `revision`, since it
    /// changes nothing a selection or a credential fence reads.
    pub fn set_profile(
        &self,
        id: &str,
        json: &str,
        subscription: &SubscriptionRow,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        if write_profile_in(&conn, id, json, subscription, self.now_ms())? == 0 {
            return Err(StoreError::AccountNotFound(id.to_string()));
        }
        Ok(())
    }

    /// Writes an account's limit reset block (`None` where the read's body carried none) and the instant of the read
    /// that answered it, for the holder and the epoch the read started under. Answers whether it wrote: a read that
    /// was out across a delete, a new account under the id, a disconnect, a claim's fold or a pairing lands nothing.
    /// Leaves the epoch as it is and bumps no `revision`, as `set_profile` bumps none.
    pub fn set_limit_resets(
        &self,
        id: &str,
        incarnation: u64,
        lifecycle_generation: u64,
        epoch: u64,
        block: Option<&str>,
        read_at: i64,
    ) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let wrote = conn.execute(
            &format!(
                "UPDATE accounts SET limit_resets_json = ?5, limit_resets_read_at = ?6 \
                 WHERE {LIMIT_RESETS_FENCE}"
            ),
            params![
                id,
                incarnation as i64,
                lifecycle_generation as i64,
                epoch as i64,
                block,
                read_at
            ],
        )?;
        Ok(wrote == 1)
    }

    /// Hands the stored block to `fold` and writes what it answers with the epoch advanced by one, in one
    /// transaction, for the holder and the epoch the claim started under, leaving the read's instant as it was. The
    /// advance refuses the write of a block read that started before the fold, which would bring a used reset back.
    /// Answers whether it wrote: `false` for another holder, for an epoch a pairing moved, for a row with no stored
    /// block and where `fold` answers `None`.
    pub fn fold_limit_resets(
        &self,
        id: &str,
        incarnation: u64,
        lifecycle_generation: u64,
        epoch: u64,
        fold: impl FnOnce(&str) -> Option<String>,
    ) -> Result<bool, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let stored: Option<Option<String>> = tx
            .query_row(
                &format!("SELECT limit_resets_json FROM accounts WHERE {LIMIT_RESETS_FENCE}"),
                params![
                    id,
                    incarnation as i64,
                    lifecycle_generation as i64,
                    epoch as i64
                ],
                |row| row.get(0),
            )
            .optional()?;
        let Some(folded) = stored.flatten().and_then(|block| fold(&block)) else {
            return Ok(false);
        };
        tx.execute(
            "UPDATE accounts SET limit_resets_json = ?2, limit_resets_epoch = limit_resets_epoch + 1 WHERE id = ?1",
            params![id, folded],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Stages or clears an account's cancellation as the operator's edit does and bumps `revision`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_canceled(&self, id: &str, cancellation: Cancellation) -> Result<(), StoreError> {
        let (canceled, ends_at) = cancellation.stored_pair();
        let conn = self.conn.lock().expect(LOCK_POISONED);
        if conn.execute(
            "UPDATE accounts SET canceled = ?2, subscription_ends_at = ?3, \
             revision = revision + 1, updated_at = ?4 WHERE id = ?1",
            params![id, canceled, ends_at, self.now_ms()],
        )? == 0
        {
            return Err(StoreError::AccountNotFound(id.to_string()));
        }
        Ok(())
    }

    pub fn subscription_term(&self, id: &str) -> Result<Option<SubscriptionTerm>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        subscription_term_in(&conn, id)
    }

    /// Every account's extras by account id, in one statement, so a list of accounts reads them once where
    /// `account_extras_of` and `subscription_term` read one row each.
    pub fn account_extras(&self) -> Result<BTreeMap<String, AccountExtras>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(&format!("SELECT {TERM_COLUMNS} FROM accounts"))?;
        let extras = stmt
            .query_map([], row_to_extras)?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        Ok(extras)
    }

    /// One account's `account_extras` entry, `None` for an unknown id, for a view of that account alone.
    pub fn account_extras_of(&self, id: &str) -> Result<Option<AccountExtras>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let extras = conn
            .prepare_cached(&TERM_BY_ID)?
            .query_row(params![id], row_to_extras)
            .optional()?;
        Ok(extras.map(|(_, extras)| extras))
    }

    /// One account's `LimitResetsRow`, `None` for an unknown id. One statement reads it, so a block read never takes
    /// its holder and its epoch from two row states.
    pub fn limit_resets_row_of(
        &self,
        account_id: &str,
    ) -> Result<Option<LimitResetsRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let row = conn
            .prepare_cached(
                "SELECT incarnation, lifecycle_generation, limit_resets_epoch, limit_resets_json \
                 FROM accounts WHERE id = ?1",
            )?
            .query_row(params![account_id], |row| {
                Ok(LimitResetsRow {
                    epoch: row.get::<_, i64>(2)? as u64,
                    incarnation: row.get::<_, i64>(0)? as u64,
                    lifecycle_generation: row.get::<_, i64>(1)? as u64,
                    text: row.get(3)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// Flags the account's plan as ended at `at` unless a flag already stands, and answers whether this write set it.
    /// Bumps `revision` so the next reload publishes it, as the operator's end does.
    pub fn set_plan_ended(&self, id: &str, at: i64) -> Result<bool, StoreError> {
        self.write_plan_ended(
            id,
            "UPDATE accounts SET plan_ended_at = ?2, revision = revision + 1, updated_at = ?3 \
             WHERE id = ?1 AND plan_ended_at IS NULL",
            Some(at),
        )
    }

    /// Clears the account's plan-ended flag and answers whether one stood, bumping `revision` as setting it does.
    pub fn clear_plan_ended(&self, id: &str) -> Result<bool, StoreError> {
        self.write_plan_ended(
            id,
            "UPDATE accounts SET plan_ended_at = ?2, revision = revision + 1, updated_at = ?3 \
             WHERE id = ?1 AND plan_ended_at IS NOT NULL",
            None,
        )
    }

    /// One conditional write of the flag, telling an unknown id from a row whose flag already read as asked.
    fn write_plan_ended(
        &self,
        id: &str,
        statement: &str,
        at: Option<i64>,
    ) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        if conn.execute(statement, params![id, at, self.now_ms()])? == 1 {
            return Ok(true);
        }
        match account_in(&conn, id)? {
            Some(_) => Ok(false),
            None => Err(StoreError::AccountNotFound(id.to_string())),
        }
    }
}

/// Shared by `Store::subscription_term` and the account edit, which reads the flag and the end it changes inside its
/// own transaction.
pub(crate) fn subscription_term_in(
    conn: &rusqlite::Connection,
    id: &str,
) -> Result<Option<SubscriptionTerm>, StoreError> {
    conn.prepare_cached(&TERM_BY_ID)?
        .query_row(params![id], row_to_term)
        .optional()
        .map_err(StoreError::from)
}

// The holder and the epoch that both block writers, the read's write and the claim's fold, fence on, so a column added
// here reaches both fences.
const LIMIT_RESETS_FENCE: &str =
    "id = ?1 AND incarnation = ?2 AND lifecycle_generation = ?3 AND limit_resets_epoch = ?4";

// The columns `row_to_term` reads, in its order, then the ones `row_to_extras` alone reads.
pub(crate) const TERM_COLUMNS: &str =
    "id, subscription_since, trial_ends_at, canceled, subscription_ends_at, \
     subscription_tier, kind, profile_json, plan_ended_at, \
     limit_resets_json, limit_resets_read_at, limit_resets_epoch, created_at, state_changed_at";

static TERM_BY_ID: LazyLock<String> =
    LazyLock::new(|| format!("SELECT {TERM_COLUMNS} FROM accounts WHERE id = ?1"));

pub(crate) fn row_to_term(row: &Row<'_>) -> rusqlite::Result<SubscriptionTerm> {
    let profile = document_of(row, 7)?;
    term_of(row, profile.as_ref())
}

fn row_to_extras(row: &Row<'_>) -> rusqlite::Result<(String, AccountExtras)> {
    // Parsed once, so the term's class, its plan and the extras share it.
    let profile = document_of(row, 7)?;
    let term = term_of(row, profile.as_ref())?;
    let extras = AccountExtras {
        created_at: row.get(12)?,
        limit_resets: row.get(9)?,
        limit_resets_epoch: row.get::<_, i64>(11)? as u64,
        limit_resets_read_at: row.get(10)?,
        profile,
        state_changed_at: row.get(13)?,
        term,
    };
    Ok((extras.term.account_id.clone(), extras))
}

/// The JSON document in a text column of a `TERM_COLUMNS` row. A document that does not parse reads as absent.
fn document_of(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<serde_json::Value>> {
    let raw: Option<String> = row.get(index)?;
    Ok(raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok()))
}

/// `row_to_term` over the profile document the caller already parsed from the row.
fn term_of(
    row: &Row<'_>,
    profile: Option<&serde_json::Value>,
) -> rusqlite::Result<SubscriptionTerm> {
    let tier: Option<String> = row.get(5)?;
    let usage_based = row.get::<_, AccountKind>(6)? == AccountKind::Oauth
        && usage_based_of(tier.as_deref(), profile);
    let plan =
        plan_of(organization_type_of(profile), tier.as_deref(), usage_based).map(String::from);
    Ok(SubscriptionTerm {
        account_id: row.get(0)?,
        since: row.get(1)?,
        trial_ends_at: row.get(2)?,
        cancellation: Cancellation::from_flag(row.get(3)?, row.get(4)?),
        tier,
        usage_based,
        plan,
        plan_ended_at: row.get(8)?,
    })
}

impl SubscriptionTerm {
    /// The stored tier where the resolved plan is one the cost table may price (`pro`, `max_5x`, `max_20x`, `max`,
    /// `team` or `enterprise`), the table's capacity rows judging the word, `None` for `free`, `usage_based` and an
    /// account with no plan: what every pricing read keys its capacity by, and the tier every priced row files under.
    pub fn priced_tier(&self) -> Option<&str> {
        match self.plan.as_deref()? {
            "pro" | "max_5x" | "max_20x" | "max" | "team" | "enterprise" => self.tier.as_deref(),
            _ => None,
        }
    }
}

/// What `Store::account_extras` answers for one account: the stored `profile_json` document, parsed, what
/// `subscription_term` answers for it, the stored limit reset block with its read's instant and its epoch, when the
/// row was created and when its state last changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountExtras {
    /// When the row was created, in epoch milliseconds: a new account under a deleted id starts over here.
    pub created_at: i64,
    /// The limit reset block's text as stored, `None` without one.
    pub limit_resets: Option<String>,
    /// Advanced by a claim's fold and by a pairing's clear. A block read writes only under the epoch it started
    /// under, so an answer that lands after either cannot bring the older block back.
    pub limit_resets_epoch: u64,
    /// The instant of the last limit reset read that answered 200, in epoch milliseconds.
    pub limit_resets_read_at: Option<i64>,
    /// `None` without a stored document and for one that does not parse.
    pub profile: Option<serde_json::Value>,
    /// When the account's state last changed, in epoch milliseconds: the stamp of the write that set the state the row
    /// holds. A write that keeps the state leaves it. A row stored before the column reads its `updated_at`.
    pub state_changed_at: i64,
    pub term: SubscriptionTerm,
}

/// What `Store::limit_resets_row_of` answers for one account: the holder and the epoch that a block write fences on,
/// and the block's text, all of one row state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitResetsRow {
    /// The row's `limit_resets_epoch`, which `AccountExtras::limit_resets_epoch` describes.
    pub epoch: u64,
    pub incarnation: u64,
    pub lifecycle_generation: u64,
    /// The limit reset block's text as stored, `None` without one.
    pub text: Option<String>,
}

/// What the profile document says about the subscription, stored beside the raw document; each field `None`
/// where the document lacks it. The dates are epoch milliseconds parsed from the RFC 3339 strings it sends.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscriptionRow {
    pub tier: Option<String>,
    pub since: Option<i64>,
    pub trial_ends_at: Option<i64>,
}

/// The tables that keep a deleted id's incarnation, one per kind of row a later row can take the id of.
#[derive(Clone, Copy)]
pub(crate) enum Tombstones {
    Account,
    ApiKey,
    Guardrail,
    Pool,
}

impl Tombstones {
    fn table(self) -> &'static str {
        match self {
            Tombstones::Account => "account_tombstones",
            Tombstones::ApiKey => "api_key_tombstones",
            Tombstones::Guardrail => "guardrail_tombstones",
            Tombstones::Pool => "pool_tombstones",
        }
    }
}

/// Leaves the id's incarnation behind in `tombstones`, never lowering one a later holder left, so the next holder's
/// incarnation from `next_incarnation_in` goes past every earlier one.
pub(crate) fn bury_in(
    conn: &rusqlite::Connection,
    tombstones: Tombstones,
    id: &str,
    incarnation: i64,
) -> Result<(), StoreError> {
    let table = tombstones.table();
    conn.execute(
        &format!(
            "INSERT INTO {table} (id, incarnation) VALUES (?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET incarnation = max({table}.incarnation, excluded.incarnation)"
        ),
        params![id, incarnation],
    )?;
    Ok(())
}

/// Deletes the row under `id` from `table` and buries its incarnation in `tombstones`, answering `false` when no row
/// stands under the id.
pub(crate) fn delete_and_bury(
    conn: &rusqlite::Connection,
    table: &str,
    tombstones: Tombstones,
    id: &str,
) -> Result<bool, StoreError> {
    let incarnation: Option<i64> = conn
        .query_row(
            &format!("SELECT incarnation FROM {table} WHERE id = ?1"),
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(incarnation) = incarnation else {
        return Ok(false);
    };
    conn.execute(&format!("DELETE FROM {table} WHERE id = ?1"), params![id])?;
    bury_in(conn, tombstones, id, incarnation)?;
    Ok(true)
}

/// The incarnation a fresh row under `id` takes in the `tombstones` table: one past what held the id before,
/// 1 when new.
pub(crate) fn next_incarnation_in(
    conn: &rusqlite::Connection,
    tombstones: Tombstones,
    id: &str,
) -> Result<u64, StoreError> {
    let table = tombstones.table();
    let buried: Option<i64> = conn
        .query_row(
            &format!("SELECT incarnation FROM {table} WHERE id = ?1"),
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(buried.unwrap_or(0) as u64 + 1)
}

/// The state before the write is read on the same connection first: every caller holds the connection's lock or
/// its transaction across both statements, so no other write lands between them, and the routing loop tells a
/// transition from a repeated verdict by it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn set_state_if_generations_on(
    conn: &rusqlite::Connection,
    now: i64,
    id: &str,
    state: AccountState,
    cause: StateCause,
    incarnation: u64,
    lifecycle_generation: u64,
    credential_generation: u64,
) -> Result<Option<(u64, AccountState)>, StoreError> {
    let Some(before) = conn
        .prepare_cached("SELECT state FROM accounts WHERE id = ?1")?
        .query_row(params![id], |row| row.get::<_, AccountState>(0))
        .optional()?
    else {
        return Ok(None);
    };
    let revision: Option<i64> = conn
        .prepare_cached(
            "UPDATE accounts SET state = ?2, state_cause = ?7, revision = revision + 1, updated_at = ?3, \
             state_changed_at = CASE WHEN state = ?2 THEN state_changed_at ELSE ?3 END \
             WHERE id = ?1 AND lifecycle_generation = ?4 AND incarnation = ?6 \
             AND (EXISTS (SELECT 1 FROM credentials WHERE account_id = ?1 AND generation = ?5) \
                  OR EXISTS (SELECT 1 FROM api_key_secrets WHERE account_id = ?1 AND generation = ?5)) \
             RETURNING revision",
        )?
        .query_row(
            params![
                id,
                state,
                now,
                lifecycle_generation as i64,
                credential_generation as i64,
                incarnation as i64,
                cause
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(revision.map(|revision| (revision as u64, before)))
}

/// The revision bump for a write that changes a dispatch input without touching the account row itself.
pub(crate) fn bump_revision(
    conn: &rusqlite::Connection,
    now: i64,
    id: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE accounts SET revision = revision + 1, updated_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

fn holds_credentials(tx: &rusqlite::Transaction<'_>, id: &str) -> Result<bool, StoreError> {
    let held: i64 = tx.query_row(
        "SELECT (SELECT COUNT(*) FROM credentials WHERE account_id = ?1) \
         + (SELECT COUNT(*) FROM api_key_secrets WHERE account_id = ?1)",
        params![id],
        |row| row.get(0),
    )?;
    Ok(held > 0)
}
