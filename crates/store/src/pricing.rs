//! Imported price tables, one row per confirmed import. The shipped table is not stored; it is the fallback
//! when this table is empty.

use rusqlite::{params, OptionalExtension};

use crate::{Store, StoreError, LOCK_POISONED};

#[derive(Clone, Debug, PartialEq)]
pub struct PricingVersion {
    pub version: String,
    pub effective: String,
    pub source: String,
    pub table_json: String,
    pub imported_at: i64,
}

const SELECT: &str =
    "SELECT version, effective, source, table_json, imported_at FROM pricing_versions";

fn row_to_version(r: &rusqlite::Row<'_>) -> rusqlite::Result<PricingVersion> {
    Ok(PricingVersion {
        version: r.get(0)?,
        effective: r.get(1)?,
        source: r.get(2)?,
        table_json: r.get(3)?,
        imported_at: r.get(4)?,
    })
}

impl Store {
    pub fn insert_pricing_version(
        &self,
        row: &PricingVersion,
        actor: &str,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO pricing_versions (version, effective, source, table_json, imported_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row.version, row.effective, row.source, row.table_json, row.imported_at],
        )?;
        crate::registry::record_change(
            &tx,
            row.imported_at,
            actor,
            "pricing_imported",
            &row.version,
            &serde_json::json!({ "effective": row.effective, "source": row.source }).to_string(),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn latest_pricing_version(&self) -> Result<Option<PricingVersion>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            &format!("{SELECT} ORDER BY imported_at DESC, version DESC LIMIT 1"),
            [],
            row_to_version,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn pricing_versions(&self) -> Result<Vec<PricingVersion>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(&format!("{SELECT} ORDER BY imported_at, version"))?;
        let rows = stmt.query_map([], row_to_version)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }
}
