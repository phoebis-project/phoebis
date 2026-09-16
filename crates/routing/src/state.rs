//! What the router task owns between selections: pins, cooldowns, the newest overage evidence per account, and
//! the newest utilization sample per account. Every mutation reports whether it changed anything so the task
//! knows what to write through.

use std::collections::HashMap;

use phoebis_store::{AccountState, CooldownRow, OverageRow, PinRow, RoutingLoad};

use crate::RegistrySnapshot;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinRef {
    pub key_id: String,
    pub session_id: String,
    pub model: String,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceSource {
    Head,
    Sample,
}

impl EvidenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceSource::Head => "head",
            EvidenceSource::Sample => "sample",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverageEvidence {
    pub in_use: bool,
    pub since: Option<i64>,
    pub reset: Option<i64>,
    pub observed_at: i64,
    pub source: EvidenceSource,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UtilizationSample {
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub observed_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StateWrite {
    /// The relay's verdict on an account, fenced by the generations the attempt that formed it went out
    /// with. It travels with the rest of the routing writes so shutdown drains it like any other.
    AccountState {
        account_id: String,
        state: AccountState,
        lifecycle_generation: u64,
        credential_generation: u64,
    },
    DeleteCooldown(String, String),
    DeletePin(String, String, String),
    UpsertCooldown(CooldownRow),
    UpsertOverage(OverageRow),
    UpsertPin(PinRow),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub cooldowns: Vec<(String, String)>,
    pub pins: Vec<(String, String, String)>,
}

pub struct RoutingState {
    cooldowns: HashMap<(String, String), CooldownRow>,
    next_epoch: u64,
    overage: HashMap<String, OverageEvidence>,
    pins: HashMap<(String, String, String), PinRow>,
    samples: HashMap<String, UtilizationSample>,
}

impl RoutingState {
    pub fn from_load(load: RoutingLoad) -> RoutingState {
        let next_epoch = load.pins.iter().map(|p| p.epoch).max().unwrap_or(0) + 1;
        RoutingState {
            cooldowns: load
                .cooldowns
                .into_iter()
                .map(|c| ((c.account_id.clone(), c.scope.clone()), c))
                .collect(),
            next_epoch,
            overage: load
                .overage
                .into_iter()
                .map(|o| {
                    (
                        o.account_id.clone(),
                        OverageEvidence {
                            in_use: o.in_use,
                            since: o.since,
                            reset: o.reset,
                            observed_at: o.observed_at,
                            source: if o.source == "sample" {
                                EvidenceSource::Sample
                            } else {
                                EvidenceSource::Head
                            },
                        },
                    )
                })
                .collect(),
            pins: load
                .pins
                .into_iter()
                .map(|p| ((p.key_id.clone(), p.session_id.clone(), p.model.clone()), p))
                .collect(),
            samples: HashMap::new(),
        }
    }

    pub(crate) fn fresh_epoch(&mut self) -> u64 {
        let e = self.next_epoch;
        self.next_epoch += 1;
        e
    }

    pub(crate) fn pin(&self, key_id: &str, session_id: &str, model: &str) -> Option<&PinRow> {
        self.pins.get(&(
            key_id.to_string(),
            session_id.to_string(),
            model.to_string(),
        ))
    }

    pub(crate) fn put_pin(&mut self, pin: PinRow) {
        self.pins.insert(
            (
                pin.key_id.clone(),
                pin.session_id.clone(),
                pin.model.clone(),
            ),
            pin,
        );
    }

    pub(crate) fn remove_pin(
        &mut self,
        key_id: &str,
        session_id: &str,
        model: &str,
    ) -> Option<PinRow> {
        self.pins.remove(&(
            key_id.to_string(),
            session_id.to_string(),
            model.to_string(),
        ))
    }

    /// The later deadline always wins, as in the store.
    pub fn record_cooldown(&mut self, row: CooldownRow) -> bool {
        let key = (row.account_id.clone(), row.scope.clone());
        match self.cooldowns.get(&key) {
            Some(current) if current.until >= row.until => false,
            _ => {
                self.cooldowns.insert(key, row);
                true
            }
        }
    }

    pub fn clear_cooldown(&mut self, account_id: &str, scope: &str) -> bool {
        self.cooldowns
            .remove(&(account_id.to_string(), scope.to_string()))
            .is_some()
    }

    /// The live row that blocks `canonical_model` on the account, the later of the account-wide and the model row.
    pub fn cooldown(
        &self,
        account_id: &str,
        canonical_model: &str,
        now: i64,
    ) -> Option<&CooldownRow> {
        let account = self
            .cooldowns
            .get(&(account_id.to_string(), "account".to_string()))
            .filter(|c| c.until > now);
        let model = self
            .cooldowns
            .get(&(account_id.to_string(), format!("model:{canonical_model}")))
            .filter(|c| c.until > now);
        match (account, model) {
            (Some(a), Some(m)) => Some(if a.until >= m.until { a } else { m }),
            (a, m) => a.or(m),
        }
    }

    pub fn cooldowns(&self) -> Vec<CooldownRow> {
        let mut v: Vec<_> = self.cooldowns.values().cloned().collect();
        v.sort_by(|a, b| (&a.account_id, &a.scope).cmp(&(&b.account_id, &b.scope)));
        v
    }

    /// Only newer evidence replaces the current state, so a slow response cannot roll it back.
    pub fn record_overage(&mut self, account_id: &str, evidence: OverageEvidence) -> bool {
        let current = self.overage.get(account_id);
        if current.is_some_and(|c| c.observed_at >= evidence.observed_at) {
            return false;
        }
        let since = since_of(current, &evidence);
        self.overage.insert(
            account_id.to_string(),
            OverageEvidence { since, ..evidence },
        );
        true
    }

    pub fn in_overage(&self, account_id: &str) -> bool {
        self.overage.get(account_id).is_some_and(|e| e.in_use)
    }
    pub fn overage(&self, account_id: &str) -> Option<&OverageEvidence> {
        self.overage.get(account_id)
    }
    pub fn overages(&self) -> Vec<(String, OverageEvidence)> {
        let mut v: Vec<_> = self
            .overage
            .iter()
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// When the account counts as cooled under `never`: the latest reset among its exhausted shared windows (the
    /// evidence's `reset` is built that way on both paths), else `fallback_ms` (the bootstrap's
    /// `cooldown_fallback_seconds`) past the observation and then re-checked, because a permanent skip would
    /// need an operator to notice.
    pub fn overage_deadline(&self, account_id: &str, fallback_ms: i64) -> Option<i64> {
        let evidence = self.overage.get(account_id).filter(|e| e.in_use)?;
        Some(evidence.reset.unwrap_or(evidence.observed_at + fallback_ms))
    }

    pub fn record_sample(&mut self, account_id: &str, sample: UtilizationSample) -> bool {
        match self.samples.get(account_id) {
            Some(current) if current.observed_at >= sample.observed_at => false,
            _ => {
                self.samples.insert(account_id.to_string(), sample);
                true
            }
        }
    }

    pub fn sample(&self, account_id: &str) -> Option<&UtilizationSample> {
        self.samples.get(account_id)
    }

    /// A late failure from account A must not remove a newer pin on account B, so both have to match.
    pub fn invalidate_pin(&mut self, pin: &PinRef, account_id: &str) -> bool {
        let key = (
            pin.key_id.clone(),
            pin.session_id.clone(),
            pin.model.clone(),
        );
        let matches = self
            .pins
            .get(&key)
            .is_some_and(|p| p.epoch == pin.epoch && p.account_id == account_id);
        if matches {
            self.pins.remove(&key);
        }
        matches
    }

    pub fn release_session(
        &mut self,
        key_id: &str,
        session_id: &str,
    ) -> Vec<(String, String, String)> {
        let gone: Vec<_> = self
            .pins
            .keys()
            .filter(|(k, s, _)| k == key_id && s == session_id)
            .cloned()
            .collect();
        for key in &gone {
            self.pins.remove(key);
        }
        gone
    }

    pub fn pins(&self) -> Vec<PinRow> {
        let mut v: Vec<_> = self.pins.values().cloned().collect();
        v.sort_by(|a, b| {
            (&a.key_id, &a.session_id, &a.model).cmp(&(&b.key_id, &b.session_id, &b.model))
        });
        v
    }

    /// Expired cooldowns, pins past their pool's idle TTL, pins whose key or pool is gone, and everything
    /// held against an account the registry no longer has. An id is reusable, so state the deleted account
    /// left behind would otherwise be inherited by whatever is created under that id next: a two-day cooldown
    /// would keep a brand new account out of every pool it joins.
    pub fn purge(&mut self, snapshot: &RegistrySnapshot, now: i64) -> Purged {
        let gone = |account_id: &str| snapshot.account(account_id).is_none();
        let cooldowns: Vec<_> = self
            .cooldowns
            .iter()
            .filter(|((account_id, _), c)| c.until <= now || gone(account_id))
            .map(|(k, _)| k.clone())
            .collect();
        for key in &cooldowns {
            self.cooldowns.remove(key);
        }
        let pins: Vec<_> = self
            .pins
            .iter()
            .filter(|(_, p)| {
                if gone(&p.account_id) {
                    return true;
                }
                let ttl = snapshot
                    .key(&p.key_id)
                    .and_then(|k| snapshot.pool(&k.pool_id))
                    .map(|pool| {
                        i64::try_from(pool.affinity_ttl_seconds)
                            .ok()
                            .and_then(|s| s.checked_mul(1_000))
                            .unwrap_or(i64::MAX)
                    });
                match ttl {
                    None => true,
                    Some(0) => true,
                    Some(ttl) => now - p.last_used_at > ttl,
                }
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in &pins {
            self.pins.remove(key);
        }
        // Neither needs a deletion written: the overage row cascades with the account, and samples are held
        // in memory only.
        self.overage.retain(|account_id, _| !gone(account_id));
        self.samples.retain(|account_id, _| !gone(account_id));
        Purged { cooldowns, pins }
    }
}

/// The start of the current overage stretch, on first evidence and on every replacement alike: an account out of
/// overage has none, one already in it keeps the start it had, and one just entering falls back to the time the
/// evidence was observed.
fn since_of(current: Option<&OverageEvidence>, evidence: &OverageEvidence) -> Option<i64> {
    if !evidence.in_use {
        return None;
    }
    match current {
        Some(current) if current.in_use => current.since,
        _ => evidence.since.or(Some(evidence.observed_at)),
    }
}
