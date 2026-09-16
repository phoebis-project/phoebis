//! Account registry: rows, lifecycle state, and the lifecycle-generation bump that disconnects a slot.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, OptionalExtension, Row};

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
    Pairing,
    Ready,
    Refreshing,
    ReauthRequired,
    FundingRequired,
    Disconnected,
}

impl AccountState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AccountState::Unpaired => "unpaired",
            AccountState::Pairing => "pairing",
            AccountState::Ready => "ready",
            AccountState::Refreshing => "refreshing",
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
            "pairing" => Ok(AccountState::Pairing),
            "ready" => Ok(AccountState::Ready),
            "refreshing" => Ok(AccountState::Refreshing),
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
}

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
    })
}

/// Shared by `Store::accounts` and `load_registry`, so the registry snapshot can read the account list
/// inside its own transaction instead of through the store's connection lock.
pub(crate) fn all_accounts(conn: &rusqlite::Connection) -> Result<Vec<AccountRow>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, label, enabled, identity_json, incarnation, lifecycle_generation, revision, state \
         FROM accounts ORDER BY id",
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
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT id, kind, label, enabled, identity_json, incarnation, lifecycle_generation, revision, state \
             FROM accounts WHERE id = ?1",
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
            "INSERT INTO accounts (id, kind, label, enabled, identity_json, incarnation, lifecycle_generation, revision, state, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?9)",
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
            params![
                id,
                kind,
                label,
                jiff::Timestamp::now().as_millisecond()
            ],
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

    pub fn set_state(&self, id: &str, state: AccountState) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "UPDATE accounts SET state = ?2, revision = revision + 1, updated_at = ?3 WHERE id = ?1",
            params![id, state, jiff::Timestamp::now().as_millisecond()],
        )?;
        Ok(())
    }

    /// `set_state` for a verdict formed before a request that took a while: the state only moves while the
    /// slot still holds the exact credentials the caller looked at. A disconnect moves the lifecycle
    /// generation and a re-pair without one moves the credential generation, so both have to match or the
    /// verdict is about credentials that no longer exist and would only lock out what replaced them. Reports
    /// whether the row moved.
    ///
    /// One statement, because two would leave a window between the check and the update exactly as wide as
    /// the one this exists to close.
    pub fn set_state_if_generations(
        &self,
        id: &str,
        state: AccountState,
        lifecycle_generation: u64,
        credential_generation: u64,
    ) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let updated = conn.execute(
            "UPDATE accounts SET state = ?2, revision = revision + 1, updated_at = ?3 \
             WHERE id = ?1 AND lifecycle_generation = ?4 \
             AND (EXISTS (SELECT 1 FROM credentials WHERE account_id = ?1 AND generation = ?5) \
                  OR EXISTS (SELECT 1 FROM api_key_secrets WHERE account_id = ?1 AND generation = ?5))",
            params![
                id,
                state,
                jiff::Timestamp::now().as_millisecond(),
                lifecycle_generation as i64,
                credential_generation as i64
            ],
        )?;
        Ok(updated > 0)
    }

    /// Disconnects the account: the new lifecycle generation invalidates any refresh in flight for the
    /// old one, and both credential kinds are deleted so a disconnected slot cannot keep spending money.
    pub fn bump_lifecycle(&self, id: &str) -> Result<u64, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = jiff::Timestamp::now().as_millisecond();
        let updated = tx.execute(
            "UPDATE accounts SET lifecycle_generation = lifecycle_generation + 1, state = ?2, \
             revision = revision + 1, updated_at = ?3 WHERE id = ?1",
            params![id, AccountState::Disconnected, now],
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
        // A disconnect must also clear the refresh gate: otherwise a pending or uncertain attempt from
        // before the disconnect keeps blocking every refresh after re-pairing, until the process restarts.
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
        tx.execute(
            "INSERT INTO account_tombstones (id, incarnation) VALUES (?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET incarnation = max(account_tombstones.incarnation, excluded.incarnation)",
            params![id, incarnation],
        )?;
        crate::registry::record_change(&tx, now, actor, "account_deleted", id, "{}")?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_account(
        &self,
        id: &str,
        label: &str,
        actor: &str,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = jiff::Timestamp::now().as_millisecond();
        if tx.execute(
            "UPDATE accounts SET label = ?2, revision = revision + 1, updated_at = ?3 \
             WHERE id = ?1",
            params![id, label, now],
        )? == 0
        {
            return Err(StoreError::AccountNotFound(id.to_string()));
        }
        let diff = serde_json::json!({ "label": label }).to_string();
        crate::registry::record_change(&tx, now, actor, "account_updated", id, &diff)?;
        tx.commit()?;
        Ok(())
    }

    /// The account's current revision, for a holder that wants to know whether the row moved without
    /// paying for the whole row and its identity.
    pub fn account_revision(&self, id: &str) -> Result<Option<u64>, StoreError> {
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

    pub fn catalog_status(&self, id: &str) -> Result<(Option<i64>, Option<String>), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT catalog_synced_at, catalog_error FROM accounts WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| StoreError::AccountNotFound(id.to_string()))
    }

    pub fn set_catalog_status(
        &self,
        id: &str,
        synced_at: Option<i64>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "UPDATE accounts SET catalog_synced_at = COALESCE(?2, catalog_synced_at), catalog_error = ?3 WHERE id = ?1",
            params![id, synced_at, error],
        )?;
        Ok(())
    }
}

/// The incarnation a fresh row under `id` takes: one past the tombstone of whatever held that id before,
/// and 1 when the id is new.
pub(crate) fn next_incarnation(conn: &rusqlite::Connection, id: &str) -> Result<u64, StoreError> {
    let buried: Option<i64> = conn
        .query_row(
            "SELECT incarnation FROM account_tombstones WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(buried.unwrap_or(0) as u64 + 1)
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
