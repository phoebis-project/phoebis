//! The keepalive's probe budgets per UTC day, written as each probe settles so a restart resumes both caps.

use rusqlite::params;

use crate::{Store, StoreError, LOCK_POISONED};

/// Which cap a budget row counts toward: a pool's dollar cap or a subscription account's plan-side one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetScope {
    Account,
    Pool,
}

impl BudgetScope {
    fn as_str(self) -> &'static str {
        match self {
            BudgetScope::Account => "account",
            BudgetScope::Pool => "pool",
        }
    }

    fn parse(s: &str) -> Option<BudgetScope> {
        match s {
            "account" => Some(BudgetScope::Account),
            "pool" => Some(BudgetScope::Pool),
            _ => None,
        }
    }
}

/// One day's spend against one cap: `usd` on a pool's row, `five_hour_share` on an account's.
#[derive(Clone, Debug, PartialEq)]
pub struct BudgetRow {
    pub scope: BudgetScope,
    pub id: String,
    pub usd: f64,
    /// The share of the account's 5-hour window its plan-funded probes consumed on `day`.
    pub five_hour_share: f64,
    /// Set once any share added on `day` was priced through a stand-in cell or factor, and kept for the day.
    pub five_hour_share_unmeasured: bool,
}

impl Store {
    /// Adds a settled probe's charge to its row for `day`, a UTC date as `YYYY-MM-DD`. Additive, so two probes
    /// settling at once land both whichever writes first; the stand-in flag only ever turns on, since a day's
    /// share with one estimate in it is an estimate.
    pub fn add_keepalive_budget(&self, day: &str, charge: &BudgetRow) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "INSERT INTO keepalive_budgets (day, scope, id, usd, five_hour_share, five_hour_share_unmeasured) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(day, scope, id) DO UPDATE SET \
               usd = keepalive_budgets.usd + excluded.usd, \
               five_hour_share = keepalive_budgets.five_hour_share + excluded.five_hour_share, \
               five_hour_share_unmeasured = \
                 keepalive_budgets.five_hour_share_unmeasured OR excluded.five_hour_share_unmeasured",
            params![
                day,
                charge.scope.as_str(),
                charge.id,
                charge.usd,
                charge.five_hour_share,
                charge.five_hour_share_unmeasured
            ],
        )?;
        Ok(())
    }

    pub fn keepalive_budgets(&self, day: &str) -> Result<Vec<BudgetRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let rows = conn
            .prepare(
                "SELECT scope, id, usd, five_hour_share, five_hour_share_unmeasured FROM keepalive_budgets \
                 WHERE day = ?1 ORDER BY scope, id",
            )?
            .query_map(params![day], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, f64>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(
                |(scope, id, usd, five_hour_share, five_hour_share_unmeasured)| {
                    Some(BudgetRow {
                        scope: BudgetScope::parse(&scope)?,
                        id,
                        usd,
                        five_hour_share,
                        five_hour_share_unmeasured,
                    })
                },
            )
            .collect())
    }

    /// Deletes every row of a day before `day`; the dates compare as text because they are zero-padded.
    pub fn purge_keepalive_budgets_before(&self, day: &str) -> Result<usize, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        Ok(conn.execute("DELETE FROM keepalive_budgets WHERE day < ?1", params![day])?)
    }
}
