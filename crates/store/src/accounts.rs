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
    fn as_str(self) -> &'static str {
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
    fn as_str(self) -> &'static str {
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub account_uuid: String,
    pub email: String,
    pub organization_uuid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AccountRow {
    pub id: String,
    pub kind: AccountKind,
    pub label: String,
    pub enabled: bool,
    pub identity: Option<Identity>,
    pub lifecycle_generation: u64,
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
        lifecycle_generation: row.get::<_, i64>("lifecycle_generation")? as u64,
        state: row.get("state")?,
    })
}

impl Store {
    pub fn accounts(&self) -> Result<Vec<AccountRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(
            "SELECT id, kind, label, enabled, identity_json, lifecycle_generation, state \
             FROM accounts ORDER BY id",
        )?;
        let rows = stmt.query_map([], row_to_account)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    pub fn account(&self, id: &str) -> Result<Option<AccountRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT id, kind, label, enabled, identity_json, lifecycle_generation, state \
             FROM accounts WHERE id = ?1",
            params![id],
            row_to_account,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn insert_account(&self, row: &AccountRow) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let identity_json = row
            .identity
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let now = jiff::Timestamp::now().as_millisecond();
        conn.execute(
            "INSERT INTO accounts (id, kind, label, enabled, identity_json, lifecycle_generation, state, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                row.id,
                row.kind,
                row.label,
                row.enabled,
                identity_json,
                row.lifecycle_generation as i64,
                row.state,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "UPDATE accounts SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, enabled, jiff::Timestamp::now().as_millisecond()],
        )?;
        Ok(())
    }

    pub fn set_state(&self, id: &str, state: AccountState) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "UPDATE accounts SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, state, jiff::Timestamp::now().as_millisecond()],
        )?;
        Ok(())
    }

    /// Disconnects the account: the new lifecycle generation invalidates any refresh in flight for the
    /// old one, and both credential kinds are deleted so a disconnected slot cannot keep spending money.
    pub fn bump_lifecycle(&self, id: &str) -> Result<u64, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = jiff::Timestamp::now().as_millisecond();
        let updated = tx.execute(
            "UPDATE accounts SET lifecycle_generation = lifecycle_generation + 1, state = ?2, updated_at = ?3 WHERE id = ?1",
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
}
