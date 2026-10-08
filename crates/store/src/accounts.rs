//! Account registry: rows, lifecycle state, and the lifecycle-generation bump that disconnects a slot.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, OptionalExtension, Row};

use crate::registry::SubscriptionTerm;
use crate::{Store, StoreError, LOCK_POISONED, PENDING_WHERE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountKind {
    Oauth,
    ApiKey,
}

impl AccountKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AccountKind::Oauth => "oauth",
            AccountKind::ApiKey => "api_key",
        }
    }
}

impl FromSql for AccountKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "oauth" => Ok(AccountKind::Oauth),
            "api_key" => Ok(AccountKind::ApiKey),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl ToSql for AccountKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountState {
    Unpaired,
    Ready,
    ReauthRequired,
    FundingRequired,
    Disconnected,
}

impl AccountState {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountState::Unpaired => "unpaired",
            AccountState::Ready => "ready",
            AccountState::ReauthRequired => "reauth_required",
            AccountState::FundingRequired => "funding_required",
            AccountState::Disconnected => "disconnected",
        }
    }
}

impl FromSql for AccountState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "unpaired" => Ok(AccountState::Unpaired),
            "ready" => Ok(AccountState::Ready),
            "reauth_required" => Ok(AccountState::ReauthRequired),
            "funding_required" => Ok(AccountState::FundingRequired),
            "disconnected" => Ok(AccountState::Disconnected),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl ToSql for AccountState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

/// Who moved an account's state, stored beside it in the same statement and read by the routing log's transition
/// row. The list is closed: a stored word outside it fails the read, as an unknown state does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateCause {
    /// The upstream refused the credential: a 401, or a 403 `authentication_error`, on an `api_key` account, or a
    /// 401 on the replay with a freshly refreshed token.
    AuthFailed,
    /// The account's deletion. The row goes with it, so the word reaches the log alone.
    Deleted,
    /// The upstream named an `api_key` account's credit balance.
    FundingRefused,
    /// The admin routes, and whatever reaches them.
    Operator,
    /// A pairing's confirm.
    Paired,
    /// The token refresh's definitive failure, the oauth manager's own write.
    RefreshFailed,
    /// The migration's default on a row from before the column. No writer stores it.
    Unrecorded,
}

impl StateCause {
    pub fn as_str(self) -> &'static str {
        match self {
            StateCause::AuthFailed => "auth_failed",
            StateCause::Deleted => "deleted",
            StateCause::FundingRefused => "funding_refused",
            StateCause::Operator => "operator",
            StateCause::Paired => "paired",
            StateCause::RefreshFailed => "refresh_failed",
            StateCause::Unrecorded => "unrecorded",
        }
    }
}

impl FromSql for StateCause {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "auth_failed" => Ok(StateCause::AuthFailed),
            "deleted" => Ok(StateCause::Deleted),
            "funding_refused" => Ok(StateCause::FundingRefused),
            "operator" => Ok(StateCause::Operator),
            "paired" => Ok(StateCause::Paired),
            "refresh_failed" => Ok(StateCause::RefreshFailed),
            "unrecorded" => Ok(StateCause::Unrecorded),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl ToSql for StateCause {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
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
const EDIT_TOKEN_REVISION_MAX: u64 = (1 << EDIT_TOKEN_REVISION_BITS) - 1;
/// The token stays below 2^53, the largest integer a JavaScript number holds exactly, so the console sends back the
/// very value it was given.
const EDIT_TOKEN_INCARNATION_MAX: u64 = (1 << (53 - EDIT_TOKEN_REVISION_BITS)) - 1;

impl AccountRow {
    /// What an operator's edit sends back to be fenced on: the incarnation and the revision in one integer. An
    /// account deleted and created again under the same id restarts its revision at 1, so the revision alone would
    /// let an editor opened before the deletion land on the replacement. A part past its share saturates: the token
    /// then names a row this one is not, so a save sending it back is refused rather than fenced onto another holder.
    pub fn edit_token(&self) -> u64 {
        (self.incarnation.min(EDIT_TOKEN_INCARNATION_MAX) << EDIT_TOKEN_REVISION_BITS)
            | self.revision.min(EDIT_TOKEN_REVISION_MAX)
    }

    /// Whether `token` names this row's incarnation and revision both.
    pub fn answers_edit_token(&self, token: u64) -> bool {
        (
            token >> EDIT_TOKEN_REVISION_BITS,
            token & EDIT_TOKEN_REVISION_MAX,
        ) == (self.incarnation, self.revision)
    }
}

/// The low bits of a stamp edit token, which carry the `updated_at` millisecond; 42 bits hold every stamp to the
/// year 2109, and the incarnation takes the bits above them, below 2^53.
pub(crate) const STAMP_TOKEN_BITS: u32 = 42;
const STAMP_TOKEN_STAMP_MAX: u64 = (1 << STAMP_TOKEN_BITS) - 1;
const STAMP_TOKEN_INCARNATION_MAX: u64 = (1 << (53 - STAMP_TOKEN_BITS)) - 1;

/// What an operator's edit of a pool or a key sends back to be fenced on: the row's incarnation and its `updated_at`
/// in one integer, so an editor opened before the row was deleted and created again cannot land on the replacement
/// even when both stamps fall in one millisecond. A part past its share saturates, as `edit_token`'s does, and the
/// token then names a row this one is not.
pub fn stamp_token(incarnation: u64, updated_at: i64) -> u64 {
    let stamp = u64::try_from(updated_at).unwrap_or(0);
    (incarnation.min(STAMP_TOKEN_INCARNATION_MAX) << STAMP_TOKEN_BITS)
        | stamp.min(STAMP_TOKEN_STAMP_MAX)
}

/// Whether `token` names a row of this incarnation and this `updated_at` both.
pub fn answers_stamp_token(incarnation: u64, updated_at: i64, token: u64) -> bool {
    (
        token >> STAMP_TOKEN_BITS,
        (token & STAMP_TOKEN_STAMP_MAX) as i64,
    ) == (incarnation, updated_at)
}

/// The one host the relay dials.
pub const INFERENCE_HOST: &str = "api.anthropic.com";

fn row_to_account(row: &Row<'_>) -> rusqlite::Result<AccountRow> {
    let identity_json: Option<String> = row.get("identity_json")?;
    let identity = identity_json
        .map(|json| {
            serde_json::from_str(&json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;
    Ok(AccountRow {
        id: row.get("id")?,
        kind: row.get("kind")?,
        label: row.get("label")?,
        enabled: row.get("enabled")?,
        identity,
        incarnation: row.get::<_, i64>("incarnation")? as u64,
        lifecycle_generation: row.get::<_, i64>("lifecycle_generation")? as u64,
        revision: row.get::<_, i64>("revision")? as u64,
        state: row.get("state")?,
        state_cause: row.get("state_cause")?,
    })
}

/// Shared by `Store::accounts` and `load_registry`, so the registry snapshot can read the account list
/// inside its own transaction instead of through the store's connection lock.
pub(crate) fn all_accounts(conn: &rusqlite::Connection) -> Result<Vec<AccountRow>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, label, enabled, identity_json, incarnation, lifecycle_generation, revision, state, \
         state_cause FROM accounts ORDER BY id",
    )?;
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
        conn.query_row(
            "SELECT id, kind, label, enabled, identity_json, incarnation, lifecycle_generation, revision, state, \
             state_cause FROM accounts WHERE id = ?1",
            params![id],
            row_to_account,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn insert_account(&self, row: &AccountRow) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let identity_json = row
            .identity
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let now = jiff::Timestamp::now().as_millisecond();
        tx.execute(
            "INSERT INTO accounts (id, kind, label, enabled, identity_json, incarnation, lifecycle_generation, revision, state, state_cause, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?10, ?9, ?9)",
            params![
                row.id,
                row.kind,
                row.label,
                row.enabled,
                identity_json,
                next_incarnation(&tx, &row.id)? as i64,
                row.lifecycle_generation as i64,
                row.state,
                now,
                row.state_cause,
            ],
        )?;
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
            params![id, kind, label, jiff::Timestamp::now().as_millisecond()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "UPDATE accounts SET enabled = ?2, revision = revision + 1, updated_at = ?3 WHERE id = ?1",
            params![id, enabled, jiff::Timestamp::now().as_millisecond()],
        )?;
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
                jiff::Timestamp::now().as_millisecond()
            ],
        );
        match written {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::IdentityTaken)
            }
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
            "UPDATE accounts SET state = ?2, state_cause = ?4, revision = revision + 1, updated_at = ?3 \
             WHERE id = ?1",
            params![id, state, jiff::Timestamp::now().as_millisecond(), cause],
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
        let now = jiff::Timestamp::now().as_millisecond();
        let updated = tx.execute(
            "UPDATE accounts SET lifecycle_generation = lifecycle_generation + 1, state = ?2, \
             state_cause = ?4, revision = revision + 1, updated_at = ?3 WHERE id = ?1",
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
        tx.execute(
            &format!(
                "UPDATE refresh_attempts SET resolved_at = ?2, outcome = 'failed', detail = 'disconnected' \
                 WHERE account_id = ?1 AND ({PENDING_WHERE})"
            ),
            params![id, now],
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
        let now = jiff::Timestamp::now().as_millisecond();
        let incarnation: Option<i64> = tx
            .query_row(
                "SELECT incarnation FROM accounts WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let incarnation = incarnation.ok_or_else(|| StoreError::AccountNotFound(id.to_string()))?;
        tx.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
        // An open attempt is the deleted holder's, and left standing it would block the next holder's first
        // refresh or quarantine it at the next start. A settled one stays as history under its incarnation.
        tx.execute(
            &format!("DELETE FROM refresh_attempts WHERE account_id = ?1 AND ({PENDING_WHERE})"),
            params![id],
        )?;
        tx.execute(
            "INSERT INTO account_tombstones (id, incarnation) VALUES (?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET incarnation = max(account_tombstones.incarnation, excluded.incarnation)",
            params![id, incarnation],
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
        if conn.execute(
            "UPDATE accounts SET profile_json = ?2, subscription_plan = ?3, subscription_tier = ?4, \
             subscription_since = ?5, trial_ends_at = ?6, updated_at = ?7 WHERE id = ?1",
            params![
                id,
                json,
                subscription.plan,
                subscription.tier,
                subscription.since,
                subscription.trial_ends_at,
                jiff::Timestamp::now().as_millisecond(),
            ],
        )? == 0
        {
            return Err(StoreError::AccountNotFound(id.to_string()));
        }
        Ok(())
    }

    pub fn profile_json(&self, id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        // `.optional()` catches a missing row; the column itself is nullable before the first poll, so the
        // row-level get is an `Option` too and the two collapse into one `None`.
        conn.query_row(
            "SELECT profile_json FROM accounts WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(StoreError::from)
    }

    /// `None` for an unknown id; a row never written answers every field `None`.
    pub fn subscription(&self, id: &str) -> Result<Option<SubscriptionRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT subscription_plan, subscription_tier, subscription_since, trial_ends_at \
             FROM accounts WHERE id = ?1",
            params![id],
            |row| {
                Ok(SubscriptionRow {
                    plan: row.get(0)?,
                    tier: row.get(1)?,
                    since: row.get(2)?,
                    trial_ends_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// The operator's answer alone: the flag and the end together, the end dropped when the flag is off;
    /// bumps `revision` so the next reload publishes it.
    pub fn set_cancelled(
        &self,
        id: &str,
        cancelled: bool,
        ends_at: Option<i64>,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        if conn.execute(
            "UPDATE accounts SET cancelled = ?2, \
             subscription_ends_at = CASE WHEN ?2 THEN ?3 ELSE NULL END, \
             revision = revision + 1, updated_at = ?4 WHERE id = ?1",
            params![
                id,
                cancelled,
                ends_at,
                jiff::Timestamp::now().as_millisecond(),
            ],
        )? == 0
        {
            return Err(StoreError::AccountNotFound(id.to_string()));
        }
        Ok(())
    }

    pub fn subscription_term(&self, id: &str) -> Result<Option<SubscriptionTerm>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT id, subscription_since, trial_ends_at, cancelled, subscription_ends_at, \
             subscription_tier FROM accounts WHERE id = ?1",
            params![id],
            |row| {
                Ok(SubscriptionTerm {
                    account_id: row.get(0)?,
                    since: row.get(1)?,
                    trial_ends_at: row.get(2)?,
                    cancelled: row.get(3)?,
                    ends_at: row.get(4)?,
                    tier: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
    }
}

/// What the profile document says about the subscription, stored beside the raw document; each field `None`
/// where the document lacks it. The dates are epoch milliseconds parsed from the RFC 3339 strings it sends.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscriptionRow {
    pub plan: Option<String>,
    pub tier: Option<String>,
    pub since: Option<i64>,
    pub trial_ends_at: Option<i64>,
}

/// The incarnation a fresh row under `id` takes: one past the tombstone of whatever held that id before,
/// and 1 when the id is new.
pub(crate) fn next_incarnation(conn: &rusqlite::Connection, id: &str) -> Result<u64, StoreError> {
    next_incarnation_in(conn, "account_tombstones", id)
}

/// The incarnation a fresh row under `id` takes in the `tombstones` table: one past what held the id before, 1 when new.
pub(crate) fn next_incarnation_in(
    conn: &rusqlite::Connection,
    tombstones: &str,
    id: &str,
) -> Result<u64, StoreError> {
    let buried: Option<i64> = conn
        .query_row(
            &format!("SELECT incarnation FROM {tombstones} WHERE id = ?1"),
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(buried.unwrap_or(0) as u64 + 1)
}

/// The state before the write is read on the same connection first: every caller holds the connection's lock or
/// its transaction across both statements, so no other write lands between them, and the routing loop tells a
/// transition from a repeated verdict by it.
pub(crate) fn set_state_if_generations_on(
    conn: &rusqlite::Connection,
    id: &str,
    state: AccountState,
    cause: StateCause,
    incarnation: u64,
    lifecycle_generation: u64,
    credential_generation: u64,
) -> Result<Option<(u64, AccountState)>, StoreError> {
    let Some(before) = conn
        .query_row(
            "SELECT state FROM accounts WHERE id = ?1",
            params![id],
            |row| row.get::<_, AccountState>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let revision: Option<i64> = conn
        .query_row(
            "UPDATE accounts SET state = ?2, state_cause = ?7, revision = revision + 1, updated_at = ?3 \
             WHERE id = ?1 AND lifecycle_generation = ?4 AND incarnation = ?6 \
             AND (EXISTS (SELECT 1 FROM credentials WHERE account_id = ?1 AND generation = ?5) \
                  OR EXISTS (SELECT 1 FROM api_key_secrets WHERE account_id = ?1 AND generation = ?5)) \
             RETURNING revision",
            params![
                id,
                state,
                jiff::Timestamp::now().as_millisecond(),
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
pub(crate) fn bump_revision(conn: &rusqlite::Connection, id: &str) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE accounts SET revision = revision + 1, updated_at = ?2 WHERE id = ?1",
        params![id, jiff::Timestamp::now().as_millisecond()],
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
