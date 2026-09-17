//! The savings accounting: what a kept-warm request saved, what a probe cost, and what a stopped recipe wasted,
//! as the ledger's rollup rows carry them. The accounting this feeds is an estimate in money for an API key and
//! a proxy for quota on a subscription account, which is why it is kept apart from the ledger's own measured
//! spend.

use std::collections::BTreeMap;

use phoebis_ledger::{KeepaliveDelta, PriceTable, Record, UNATTRIBUTED};

use crate::engine::{Engine, SessionKey, Settlement};
use crate::probe::{Applied, ProbeOutcome, ProbeRecord};
use crate::usage::ProbeUsage;

/// The gross saving of a request that read `cache_read_tokens` it would otherwise have paid as input.
pub(crate) fn gross_saving(
    table: &PriceTable,
    canonical_model: &str,
    cache_read_tokens: u64,
) -> Option<f64> {
    let price = table.price(canonical_model)?;
    Some(cache_read_tokens as f64 * (price.input - price.cache_read) / 1_000_000.0)
}

/// A probe's estimated cost at list prices, for every account kind; a subscription account's is a proxy for
/// quota rather than money.
pub(crate) fn probe_cost(
    table: &PriceTable,
    canonical_model: &str,
    usage: ProbeUsage,
) -> Option<f64> {
    table
        .estimate(
            canonical_model,
            usage.input,
            usage.read,
            usage.five_minutes,
            usage.one_hour,
            usage.output,
        )
        .map(|(total, _)| total)
}

/// One delta of a recipe's key. An account the recipe never reached takes the ledger's reserved id, which is
/// what every reader groups an unattributed row under; an empty string would read as an account of its own.
fn delta(
    at: i64,
    key: &SessionKey,
    pool_id: &str,
    account: Option<&str>,
    canonical_model: &str,
    kind: &str,
) -> KeepaliveDelta {
    KeepaliveDelta {
        at,
        key_id: key.0.clone(),
        pool_id: pool_id.to_string(),
        account_id: account
            .filter(|a| !a.is_empty())
            .unwrap_or(UNATTRIBUTED)
            .to_string(),
        canonical_model: canonical_model.to_string(),
        kind: kind.to_string(),
        ..Default::default()
    }
}

impl Engine {
    /// Every probe the upstream served lands here, landed on its recipe or not: the cost is the pool's whatever
    /// happened to the recipe while the probe was in flight.
    pub(crate) fn record_probe(&self, record: &ProbeRecord, _applied: Applied, now: i64) {
        let mut d = delta(
            now,
            &record.key,
            &record.pool_id,
            Some(&record.account),
            &record.canonical_model,
            "probe",
        );
        match record.outcome {
            ProbeOutcome::Migrated => d.probe_migrated = 1,
            _ => d.probe_cost_usd = record.cost_usd,
        }
        self.parts.ledger.record(Record::Keepalive(d));
    }

    /// One request the previous recipe kept warm. The caller reads every part of the key off the agent under
    /// the lock and hands them over, so nothing here needs the lock back.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_kept_warm(
        &self,
        at: i64,
        key: &SessionKey,
        pool_id: &str,
        account: Option<&str>,
        canonical_model: &str,
        request_id: &str,
        tokens: u64,
        saving: f64,
    ) {
        let mut d = delta(at, key, pool_id, account, canonical_model, "inference");
        d.kept_warm_request_id = Some(request_id.to_string());
        d.kept_warm_requests = 1;
        d.kept_warm_tokens = tokens;
        d.saved_usd_gross = saving;
        self.parts.ledger.record(Record::Keepalive(d));
    }

    pub(crate) fn record_settlement(&self, at: i64, settlement: Settlement) {
        let mut d = delta(
            at,
            &settlement.key,
            &settlement.pool_id,
            settlement.account.as_deref(),
            &settlement.canonical_model,
            "probe",
        );
        d.wasted_probes = settlement.costs.len() as u64;
        d.wasted_probe_usd = settlement.costs.iter().sum();
        d.wasted_probe_reasons =
            BTreeMap::from([(settlement.label.as_str().to_string(), d.wasted_probes)]);
        self.parts.ledger.record(Record::Keepalive(d));
    }

    pub(crate) fn record_skipped(
        &self,
        at: i64,
        key: &SessionKey,
        pool_id: &str,
        account: Option<&str>,
        canonical_model: &str,
    ) {
        let mut d = delta(at, key, pool_id, account, canonical_model, "probe");
        d.probes_skipped = 1;
        self.parts.ledger.record(Record::Keepalive(d));
    }

    /// Drains what the last locked section queued: the settlements of stopped recipes and the probes a gate
    /// withheld. Called after every lock release by the entry points that can queue either.
    pub(crate) fn flush(&self) {
        let now = self.now();
        let (settlements, skipped) = {
            let mut inner = self.lock();
            let queued = std::mem::take(&mut inner.skipped);
            let mut skipped: Vec<(SessionKey, String, String, Option<String>, String)> = Vec::new();
            for (key, agent_id) in queued {
                let Some(agent) = inner
                    .sessions
                    .get(&key)
                    .and_then(|s| s.agents.get(&agent_id))
                else {
                    continue;
                };
                skipped.push((
                    key,
                    agent_id,
                    agent.pool_id.clone(),
                    agent.account.clone(),
                    agent.canonical_model.clone(),
                ));
            }
            (std::mem::take(&mut inner.settlements), skipped)
        };
        for settlement in settlements {
            self.record_settlement(now, settlement);
        }
        for (key, _, pool_id, account, model) in skipped {
            self.record_skipped(now, &key, &pool_id, account.as_deref(), &model);
        }
    }

    pub fn spent_today(&self) -> BTreeMap<String, f64> {
        self.lock().spent_today.clone()
    }

    /// The pool's probe spend for the current UTC day, from the rollups: what a restart starts from.
    pub async fn load_spent_today(&self) -> BTreeMap<String, f64> {
        let now = self.now();
        let day_start = now.div_euclid(86_400_000) * 86_400_000 / 1_000;
        let db = self.parts.ledger.db().clone();
        let rows = tokio::task::spawn_blocking(move || {
            db.read(|c| phoebis_ledger::read_rollups(c, day_start, day_start + 86_400))
        })
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
        let mut spent = BTreeMap::new();
        for row in rows.into_iter().filter(|r| r.kind == "probe") {
            *spent.entry(row.pool_id).or_insert(0.0) += row.probe_cost_usd;
        }
        let mut inner = self.lock();
        inner.spent_today = spent.clone();
        inner.spent_day = now.div_euclid(86_400_000);
        spent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delta_without_an_account_takes_the_ledgers_reserved_id() {
        let key = ("desktop".to_string(), "session".to_string());
        let of = |account| delta(0, &key, "main", account, "claude-sonnet-5", "probe").account_id;
        assert_eq!(of(None), UNATTRIBUTED, "a recipe with no account");
        assert_eq!(of(Some("")), UNATTRIBUTED, "a probe record that named none");
        assert_eq!(of(Some("a")), "a");
    }
}
